//! Minimal `AF_UNIX`/`SOCK_STREAM` sockets: enough of `socket`/`bind`/
//! `listen`/`connect`/`accept`/`send`/`recv` (plus `SCM_RIGHTS` fd
//! passing over `sendmsg`/`recvmsg`) for a Wayland-style client/
//! compositor handshake over a well-known path.
//!
//! There's no filesystem inode behind a "bound" path — `bind` just
//! records it as a lookup key in `LISTENERS`, entirely inside this
//! module. That's enough for MachaOS's own compositor (`wayland.rs`,
//! kernel-native — see its module docs for why) and a Linux-ABI client
//! process to find each other; nothing outside this kernel could ever
//! connect to one of these paths anyway.
//!
//! Every syscall built on top of this (`linux_abi.rs`'s `sys_connect`/
//! `sys_sendmsg`/`sys_recvmsg`) is **non-blocking**: interrupts are off
//! for a syscall's entire duration (see `syscall.rs`'s module docs), so
//! a syscall handler can never `hlt`-and-wait for another task to
//! produce data the way `process::wait` or the compositor's own
//! accept loop can. `recv` returning "0 bytes, no fds" for an empty
//! inbox is the whole story on that side; a client polls by retrying
//! the syscall from ring 3, where interrupts *are* enabled between
//! syscalls, so the scheduler keeps giving the compositor task turns to
//! fill the inbox. `accept`, only ever called from the compositor's own
//! kernel-native task (never through the non-blocking syscall path), is
//! the one place in this module that's fine to leave to a spin-`hlt`
//! loop at the call site.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::shm;
use crate::sync::SpinLock;

enum Role {
    Idle,
    Bound(String),
    Listening,
    /// The id of the endpoint on the other end of this connection.
    Connected(usize),
}

struct Endpoint {
    role: Role,
    /// Bytes the peer has `send`t us, waiting to be `recv`d.
    inbox: VecDeque<u8>,
    /// `shm.rs` object ids the peer passed via `SCM_RIGHTS`, waiting to
    /// be claimed (as new fds) by our `recvmsg`.
    inbox_fds: VecDeque<usize>,
    /// Endpoint ids of not-yet-`accept`ed connections — populated by
    /// `connect()` on whichever endpoint is `Listening` at the bound
    /// path, meaningful only for that one.
    backlog: VecDeque<usize>,
}

impl Endpoint {
    const fn new() -> Self {
        Endpoint {
            role: Role::Idle,
            inbox: VecDeque::new(),
            inbox_fds: VecDeque::new(),
            backlog: VecDeque::new(),
        }
    }
}

struct State {
    endpoints: Vec<Option<Endpoint>>,
    listeners: BTreeMap<String, usize>,
}

static STATE: SpinLock<State> = SpinLock::new(State {
    endpoints: Vec::new(),
    listeners: BTreeMap::new(),
});

fn alloc_endpoint(state: &mut State, ep: Endpoint) -> usize {
    for (i, slot) in state.endpoints.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(ep);
            return i;
        }
    }
    state.endpoints.push(Some(ep));
    state.endpoints.len() - 1
}

/// Creates a fresh, unbound/unconnected endpoint. Returns its id.
pub fn create() -> usize {
    let mut state = STATE.lock();
    alloc_endpoint(&mut state, Endpoint::new())
}

pub fn bind(id: usize, path: &str) -> bool {
    let mut state = STATE.lock();
    if state.listeners.contains_key(path) {
        return false;
    }
    let Some(Some(ep)) = state.endpoints.get_mut(id) else {
        return false;
    };
    if !matches!(ep.role, Role::Idle) {
        return false;
    }
    ep.role = Role::Bound(path.to_string());
    true
}

pub fn listen(id: usize) -> bool {
    let mut state = STATE.lock();
    let Some(Some(ep)) = state.endpoints.get_mut(id) else {
        return false;
    };
    let Role::Bound(path) = &ep.role else {
        return false;
    };
    let path = path.clone();
    ep.role = Role::Listening;
    state.listeners.insert(path, id);
    true
}

/// Connects `id` to whatever endpoint is `Listening` at `path`. Creates
/// the paired server-side endpoint and queues it on the listener's
/// backlog for `accept` — real `connect()`/`accept()` semantics, just
/// both happening synchronously here since nothing in this kernel needs
/// the three-way delay a real network stack would have.
pub fn connect(id: usize, path: &str) -> bool {
    let mut state = STATE.lock();
    let Some(&listener_id) = state.listeners.get(path) else {
        return false;
    };
    let server_ep = Endpoint {
        role: Role::Connected(id),
        ..Endpoint::new()
    };
    let server_id = alloc_endpoint(&mut state, server_ep);
    let Some(Some(listener)) = state.endpoints.get_mut(listener_id) else {
        return false;
    };
    listener.backlog.push_back(server_id);
    let Some(Some(client_ep)) = state.endpoints.get_mut(id) else {
        return false;
    };
    client_ep.role = Role::Connected(server_id);
    true
}

/// Pops the next pending connection off `listener_id`'s backlog, if any
/// — non-blocking; callers that want to wait (only the kernel-native
/// compositor task ever does) spin-`hlt` around this themselves.
pub fn accept(listener_id: usize) -> Option<usize> {
    let mut state = STATE.lock();
    let Some(Some(ep)) = state.endpoints.get_mut(listener_id) else {
        return None;
    };
    ep.backlog.pop_front()
}

fn peer_of(state: &State, id: usize) -> Option<usize> {
    match state.endpoints.get(id)?.as_ref()?.role {
        Role::Connected(peer) => Some(peer),
        _ => None,
    }
}

/// Delivers `bytes` and `fds` (already this *sender's* fds — see
/// `linux_abi.rs::sys_sendmsg`, which resolves them from the caller's fd
/// table before calling this) to `id`'s peer. Each passed fd's
/// `shm.rs` refcount is bumped here, on the sender's behalf, matching
/// real `SCM_RIGHTS`: the sender keeps its own fd open too.
pub fn send(id: usize, bytes: &[u8], fds: &[usize]) -> Option<usize> {
    let mut state = STATE.lock();
    let peer_id = peer_of(&state, id)?;
    for &fid in fds {
        shm::dup(fid);
    }
    let Some(Some(peer)) = state.endpoints.get_mut(peer_id) else {
        // Peer already closed — real Linux would SIGPIPE/EPIPE; drop the
        // fd references we just took since nothing will ever claim them.
        for &fid in fds {
            shm::close(fid);
        }
        return None;
    };
    peer.inbox.extend(bytes.iter().copied());
    peer.inbox_fds.extend(fds.iter().copied());
    Some(bytes.len())
}

/// Drains up to `buf.len()` pending bytes and *every* pending passed fd
/// (real `recvmsg` respects message boundaries per call; this collapses
/// them, which is fine for the one handshake this module exists for —
/// see `wayland.rs`, which never pipelines more than one fd-bearing
/// message in flight at a time).
pub fn recv(id: usize, buf: &mut [u8]) -> (usize, Vec<usize>) {
    let mut state = STATE.lock();
    let Some(Some(ep)) = state.endpoints.get_mut(id) else {
        return (0, Vec::new());
    };
    let n = ep.inbox.len().min(buf.len());
    for slot in buf.iter_mut().take(n) {
        *slot = ep.inbox.pop_front().unwrap();
    }
    let fds = ep.inbox_fds.drain(..).collect();
    (n, fds)
}

pub fn has_data(id: usize) -> bool {
    STATE
        .lock()
        .endpoints
        .get(id)
        .and_then(|e| e.as_ref())
        .is_some_and(|e| !e.inbox.is_empty())
}

pub fn has_backlog(id: usize) -> bool {
    STATE
        .lock()
        .endpoints
        .get(id)
        .and_then(|e| e.as_ref())
        .is_some_and(|e| !e.backlog.is_empty())
}

/// Closes `id`: drops any never-received passed fds (their sender isn't
/// getting them back — same as a real process dying with unclaimed
/// `SCM_RIGHTS` in flight) and removes it from `LISTENERS` if it was
/// the listening endpoint for some path.
pub fn close(id: usize) {
    let mut state = STATE.lock();
    if let Some(Some(ep)) = state.endpoints.get(id) {
        for &fid in &ep.inbox_fds {
            shm::close(fid);
        }
    }
    state.listeners.retain(|_, &mut lid| lid != id);
    if let Some(slot) = state.endpoints.get_mut(id) {
        *slot = None;
    }
}
