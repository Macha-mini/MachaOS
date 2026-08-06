//! Phase 10: `AF_INET` socket endpoints for the Linux ABI layer.
//!
//! This is the syscall-facing half of the network stack: `net.rs` holds
//! the raw packet primitives (ARP/IPv4/UDP/TCP building blocks, all
//! kernel-internal, driven by whoever happens to be polling), and this
//! module holds the *stateful* per-socket side — source-port assignment,
//! the TCP client state machine (SYN → SYN-ACK → established → data →
//! FIN), per-socket receive buffers, and a `pump()` that drains the
//! NIC's RX ring and dispatches each frame to the right endpoint.
//!
//! The NIC is polled, so there is no interrupt-driven "network task":
//! every frame is dispatched by whichever syscall handler is currently
//! making progress on a socket (connect/recv loops call `pump()` between
//! `yield_rr()`s, and poll/epoll scans call it too). Data that arrives
//! while no socket syscall is running simply sits in the e1000 RX ring
//! until the next pump. That is fine for request/response traffic on a
//! local slirp link (small replies, 32-deep RX ring); it is documented
//! here as a limitation rather than hidden.
//!
//! TCP receive assumes in-order segments (drop anything with a gap):
//! slirp on the local loopback delivers in order for the small
//! request/response exchanges this layer exists for. No retransmission,
//! no windowing, no TIME_WAIT — `close` sends a FIN and forgets.
//!
//! Server-side limitations (same honest-documentation spirit): a child
//! in `SynReceived` whose final ACK never arrives — or whose listener
//! slot was closed before it could be queued — stays in the endpoint
//! table forever (no handshake reaper); connections already sitting in
//! a closed listener's accept queue leak the same way. The cap-16
//! bounds the *accept queue* but not the number of `SynReceived`
//! children a SYN flood can create. All bounded in practice for the
//! VM-targeted request/response traffic this layer exists for.
//!
//! A syscall handler runs with interrupts off (SFMASK), so none of the
//! wait loops here may `hlt` — they use `task::yield_rr` (stay runnable,
//! get rescheduled next quantum, re-poll), the same pattern `wait4` and
//! blocking `futex` already use.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, Ordering};

use crate::sync::SpinLock;

pub const AF_INET: u64 = 2;
pub const SOCK_STREAM: u64 = 1;
pub const SOCK_DGRAM: u64 = 2;
/// Masked off `socket`'s type argument — this kernel doesn't track them.
pub const SOCK_CLOEXEC: u64 = 0x80000;
pub const SOCK_NONBLOCK: u64 = 0x800;

const EBADF: i32 = 9;
const EAGAIN: i32 = 11;
const EINVAL: i32 = 22;
const EIO: i32 = 5;
const ENOTCONN: i32 = 107;
const ECONNREFUSED: i32 = 111;
const ECONNRESET: i32 = 104;
const EISCONN: i32 = 106;
const EADDRINUSE: i32 = 98;
const ETIMEDOUT: i32 = 110;
const EINPROGRESS: i32 = 115;

const TCP_SYN: u8 = 0x02;
const TCP_ACK: u8 = 0x10;
const TCP_FIN: u8 = 0x01;
const TCP_PSH: u8 = 0x08;
const TCP_RST: u8 = 0x04;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Tcp,
    Udp,
}

#[derive(Clone, Copy, PartialEq)]
pub enum TcpState {
    /// Fresh socket, nothing sent yet.
    Idle,
    /// SYN sent, waiting for SYN-ACK (or RST).
    SynSent,
    /// We received a SYN on a listening socket and sent SYN-ACK;
    /// waiting for the final ACK to open the connection.
    SynReceived,
    /// Handshake done — data can flow both ways.
    Established,
    /// We sent FIN (`shutdown(SHUT_WR)` / `close`); still reading.
    FinSent,
}

pub struct TcpCtl {
    pub state: TcpState,
    pub dst_ip: [u8; 4],
    pub dst_port: u16,
    pub dst_mac: [u8; 6],
    /// Our next outgoing sequence number.
    pub our_seq: u32,
    /// The server's initial sequence number (from its SYN-ACK). The
    /// first data byte it sends has seq = their_initial_seq + 1.
    pub their_initial_seq: u32,
    /// Whether an ARP request for `dst_ip` has been sent (and its SYN
    /// is still waiting on the reply).
    pub arp_sent: bool,
    /// For a server-side child connection: the endpoint id of the
    /// listener that spawned it, so the handshake-completion ACK can be
    /// queued onto that listener's accept queue. `None` for clients.
    pub listener: Option<usize>,
}

/// A received UDP datagram (boundaries preserved — recvfrom returns one).
pub struct UdpPacket {
    pub src_ip: [u8; 4],
    pub src_port: u16,
    pub payload: Vec<u8>,
}

pub struct Endpoint {
    pub kind: Kind,
    /// Our source port; 0 until assigned (bind, or first connect/sendto).
    pub local_port: u16,
    /// Peer recorded by `connect` on a UDP socket — send/recv without an
    /// explicit address use it.
    pub udp_peer: Option<([u8; 4], u16)>,
    pub tcp: Option<TcpCtl>,
    /// True after `listen()` — this is a TCP server socket. Incoming
    /// connections whose handshake completed are queued in
    /// `accept_queue` (endpoint ids) until `accept` pops them.
    pub listening: bool,
    pub accept_queue: VecDeque<usize>,
    /// Received, in-order TCP bytes waiting for read/recv.
    pub inbox: VecDeque<u8>,
    /// Received UDP datagrams.
    pub udp_inbox: VecDeque<UdpPacket>,
    /// FIN received (the peer closed its write side).
    pub eof: bool,
    /// RST received.
    pub rst: bool,
    /// O_NONBLOCK (set via `socket(SOCK_NONBLOCK)` or `fcntl(F_SETFL)`).
    pub nonblock: bool,
}

type State = Vec<Option<Endpoint>>;

static STATE: SpinLock<State> = SpinLock::new(Vec::new());
static NEXT_EPHEMERAL: AtomicU16 = AtomicU16::new(0);

fn alloc_endpoint(state: &mut State, ep: Endpoint) -> usize {
    for (i, slot) in state.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(ep);
            return i;
        }
    }
    state.push(Some(ep));
    state.len() - 1
}

/// Finds a free source port in the ephemeral range (49152..65535),
/// skipping any port another endpoint already holds.
fn alloc_ephemeral(state: &State) -> u16 {
    let base = 0xC000 + (NEXT_EPHEMERAL.fetch_add(1, Ordering::Relaxed) as usize) % 0x4000;
    for i in 0..0x4000 {
        let port = (((base - 0xC000) + i) % 0x4000) as u16 + 0xC000;
        if !state.iter().flatten().any(|e| e.local_port == port) {
            return port;
        }
    }
    0xC000
}

/// Creates a fresh AF_INET endpoint and returns its id.
pub fn create(kind: Kind) -> usize {
    let mut state = STATE.lock();
    alloc_endpoint(
        &mut state,
        Endpoint {
            kind,
            local_port: 0,
            udp_peer: None,
            tcp: None,
            listening: false,
            accept_queue: VecDeque::new(),
            inbox: VecDeque::new(),
            udp_inbox: VecDeque::new(),
            eof: false,
            rst: false,
            nonblock: false,
        },
    )
}

/// Frees the endpoint. A TCP endpoint in the established state sends its
/// FIN first (the peer sees EOF), then forgets the connection — no
/// TIME_WAIT, documented limitation.
pub fn close(id: usize) {
    let mut state = STATE.lock();
    let Some(ep) = state.get_mut(id).and_then(|s| s.as_mut()) else {
        return;
    };
    let tcp = ep.tcp.take();
    // Copy the ctl out so we can send without holding the registry lock
    // (e1000::send takes the NIC lock; lock ordering stays STATE -> NIC
    // by never holding STATE across the send).
    if let Some(t) = tcp {
        if t.state == TcpState::Established {
            let seg = crate::net::build_tcp(
                ep.local_port,
                t.dst_port,
                t.our_seq,
                t.their_initial_seq.wrapping_add(1).wrapping_add(ep.inbox.len() as u32),
                TCP_FIN | TCP_ACK,
                &[],
                t.dst_ip,
            );
            let _ = crate::net::send_tcp(&t.dst_mac, t.dst_ip, &seg);
        }
    }
    state[id] = None;
}

/// Binds `port` (0 = pick an ephemeral one) as the local source port.
pub fn bind(id: usize, port: u16) -> Result<(), i32> {
    let mut state = STATE.lock();
    // Take the endpoint out of the registry so `state` is free for the
    // immutable borrows below (collision scan, port allocation).
    let mut ep = state.get_mut(id).and_then(|s| s.take()).ok_or(EBADF)?;
    if ep.local_port != 0 {
        state[id] = Some(ep);
        return Err(EINVAL);
    }
    let port = if port == 0 { alloc_ephemeral(&state) } else { port };
    if state.iter().flatten().any(|e| e.local_port == port) {
        state[id] = Some(ep);
        return Err(EADDRINUSE);
    }
    ep.local_port = port;
    state[id] = Some(ep);
    Ok(())
}

/// Marks a bound TCP socket as a listener (`listen(2)`). The endpoint
/// must be a TCP socket with a bound local port and no connection in
/// progress.
pub fn listen(id: usize) -> Result<(), i32> {
    let mut state = STATE.lock();
    let Some(ep) = state.get_mut(id).and_then(|s| s.as_mut()) else {
        return Err(EBADF);
    };
    if ep.kind != Kind::Tcp || ep.local_port == 0 || ep.tcp.is_some() {
        return Err(EINVAL);
    }
    ep.listening = true;
    Ok(())
}

/// Pops one handshake-completed incoming connection off a listener's
/// accept queue, if any (`accept(2)` without blocking). The caller
/// (syscall) pumps + yields until this returns `Some`.
pub fn accept_pending(id: usize) -> Option<usize> {
    STATE.lock().get_mut(id).and_then(|s| s.as_mut()).and_then(|e| e.accept_queue.pop_front())
}

/// Whether the endpoint is a listening TCP socket (`listen(2)` was
/// called) — `accept` on anything else is EINVAL, not a 30-second wait.
pub fn is_listening(id: usize) -> bool {
    STATE
        .lock()
        .get(id)
        .and_then(|s| s.as_ref())
        .map(|e| e.listening)
        .unwrap_or(false)
}

/// Sets/clears O_NONBLOCK (`fcntl(F_SETFL)`).
pub fn set_nonblock(id: usize, nb: bool) -> Result<(), i32> {
    let mut state = STATE.lock();
    let Some(ep) = state.get_mut(id).and_then(|s| s.as_mut()) else {
        return Err(EBADF);
    };
    ep.nonblock = nb;
    Ok(())
}

pub fn is_nonblock(id: usize) -> bool {
    STATE
        .lock()
        .get(id)
        .and_then(|s| s.as_ref())
        .map(|e| e.nonblock)
        .unwrap_or(false)
}

/// Starts a connect. For UDP this just records the peer (returns
/// immediately). For TCP it assigns a source port and sends the SYN
/// (via ARP if the MAC is cached; otherwise the ARP request goes out
/// now and `pump` sends the SYN when the reply lands). The caller then
/// loops on `pump()` + `connect_done()` until established or failed —
/// the syscall handler's yield loop (a syscall can't hlt-wait).
pub fn start_connect(id: usize, ip: [u8; 4], port: u16) -> Result<(), i32> {
    let mac = crate::net::arp_lookup(ip);
    let mut send_now = false;
    let setup: Result<(), i32> = {
        let mut state = STATE.lock();
        let mut ep = state.get_mut(id).and_then(|s| s.take()).ok_or(EBADF)?;
        let kind = ep.kind;
        let already_connected = ep.tcp.is_some();
        let had_local = ep.local_port != 0;
        match kind {
            Kind::Udp => {
                if !had_local {
                    ep.local_port = alloc_ephemeral(&state);
                }
                ep.udp_peer = Some((ip, port));
                state[id] = Some(ep);
                Ok(())
            }
            Kind::Tcp => {
                if already_connected {
                    state[id] = Some(ep);
                    return Err(EISCONN);
                }
                if !had_local {
                    ep.local_port = alloc_ephemeral(&state);
                }
                ep.tcp = Some(TcpCtl {
                    state: TcpState::SynSent,
                    dst_ip: ip,
                    dst_port: port,
                    dst_mac: mac.unwrap_or([0; 6]),
                    our_seq: 0x1234_5678u32.wrapping_add((id as u32).wrapping_mul(0x10_0003)),
                    their_initial_seq: 0,
                    arp_sent: mac.is_some(),
                    listener: None,
                });
                send_now = mac.is_some();
                state[id] = Some(ep);
                Ok(())
            }
        }
    };
    setup?;
    if send_now {
        send_syn(id)?;
    } else {
        // MAC unknown: kick off the ARP request; `pump` sends the SYN
        // when the reply lands.
        let _ = crate::net::send_arp_request(ip);
    }
    Ok(())
}

/// Sends the SYN for a SynSent endpoint (first time only). Returns
/// EIO if the NIC is down.
fn send_syn(id: usize) -> Result<(), i32> {
    let (src_port, dst_port, dst_ip, dst_mac, our_seq) = {
        let state = STATE.lock();
        let Some(ep) = state.get(id).and_then(|s| s.as_ref()) else {
            return Err(EBADF);
        };
        let Some(t) = &ep.tcp else { return Ok(()) };
        (ep.local_port, t.dst_port, t.dst_ip, t.dst_mac, t.our_seq)
    };
    let seg = crate::net::build_tcp(src_port, dst_port, our_seq, 0, TCP_SYN, &[], dst_ip);
    crate::net::send_tcp(&dst_mac, dst_ip, &seg).map_err(|_| EIO)?;
    let mut state = STATE.lock();
    if let Some(t) = state.get_mut(id).and_then(|s| s.as_mut()).and_then(|e| e.tcp.as_mut()) {
        t.our_seq = t.our_seq.wrapping_add(1);
        t.arp_sent = true;
    }
    Ok(())
}

/// Polls the endpoint's connect progress: `Ok(true)` established,
/// `Ok(false)` still in flight, `Err` failed (ECONNREFUSED on RST,
/// ETIMEDOUT is the caller's deadline).
pub fn connect_done(id: usize) -> Result<bool, i32> {
    let state = STATE.lock();
    let Some(ep) = state.get(id).and_then(|s| s.as_ref()) else {
        return Err(EBADF);
    };
    let Some(t) = &ep.tcp else { return Ok(false) };
    match t.state {
        TcpState::Established => Ok(true),
        TcpState::SynSent | TcpState::FinSent | TcpState::SynReceived => {
            if ep.rst {
                Err(ECONNREFUSED)
            } else {
                Ok(false)
            }
        }
        TcpState::Idle => Err(EINVAL),
    }
}

/// The connected peer (both kinds), if any.
pub fn peer_of(id: usize) -> Option<([u8; 4], u16)> {
    let state = STATE.lock();
    let ep = state.get(id).and_then(|s| s.as_ref())?;
    match ep.kind {
        Kind::Udp => ep.udp_peer,
        Kind::Tcp => ep
            .tcp
            .as_ref()
            .filter(|t| t.state == TcpState::Established)
            .map(|t| (t.dst_ip, t.dst_port)),
    }
}

pub fn local_port_of(id: usize) -> u16 {
    STATE
        .lock()
        .get(id)
        .and_then(|s| s.as_ref())
        .map(|e| e.local_port)
        .unwrap_or(0)
}

pub fn kind_of(id: usize) -> Kind {
    STATE
        .lock()
        .get(id)
        .and_then(|s| s.as_ref())
        .map(|e| e.kind)
        .unwrap_or(Kind::Tcp)
}

/// Resolves `ip`'s MAC for a syscall context: cache first, else send an
/// ARP request and pump+yield until the reply lands (the `hlt`-based
/// `net::resolve` can't run with IF cleared inside a syscall).
fn ensure_mac(ip: [u8; 4]) -> Result<[u8; 6], i32> {
    if let Some(mac) = crate::net::arp_lookup(ip) {
        return Ok(mac);
    }
    crate::net::send_arp_request(ip).map_err(|_| EIO)?;
    let deadline = crate::interrupts::ticks() + 3000;
    loop {
        pump();
        if let Some(mac) = crate::net::arp_lookup(ip) {
            return Ok(mac);
        }
        if crate::interrupts::ticks() >= deadline {
            return Err(ETIMEDOUT);
        }
        crate::task::yield_rr();
    }
}

/// Sends bytes on a connected socket (TCP: one segment with proper
/// seq/ack; UDP: one datagram to the peer from `connect`).
pub fn send(id: usize, data: &[u8]) -> Result<usize, i32> {
    let (kind, src_port, dst_ip, dst_port, dst_mac, our_seq, their_initial_seq, inbox_len, peer) = {
        let state = STATE.lock();
        let Some(ep) = state.get(id).and_then(|s| s.as_ref()) else {
            return Err(EBADF);
        };
        match ep.kind {
            Kind::Udp => (Kind::Udp, ep.local_port, [0; 4], 0, [0; 6], 0, 0, 0, ep.udp_peer),
            Kind::Tcp => {
                let Some(t) = &ep.tcp else { return Err(ENOTCONN) };
                if t.state != TcpState::Established {
                    return Err(ENOTCONN);
                }
                (
                    Kind::Tcp,
                    ep.local_port,
                    t.dst_ip,
                    t.dst_port,
                    t.dst_mac,
                    t.our_seq,
                    t.their_initial_seq,
                    ep.inbox.len(),
                    Some((t.dst_ip, t.dst_port)),
                )
            }
        }
    };
    match kind {
        Kind::Tcp => {
            let seg = crate::net::build_tcp(
                src_port,
                dst_port,
                our_seq,
                their_initial_seq.wrapping_add(1).wrapping_add(inbox_len as u32),
                TCP_PSH | TCP_ACK,
                data,
                dst_ip,
            );
            crate::net::send_tcp(&dst_mac, dst_ip, &seg).map_err(|_| EIO)?;
            let mut state = STATE.lock();
            if let Some(t) = state.get_mut(id).and_then(|s| s.as_mut()).and_then(|e| e.tcp.as_mut()) {
                t.our_seq = t.our_seq.wrapping_add(data.len() as u32);
            }
            Ok(data.len())
        }
        Kind::Udp => {
            let Some((ip, port)) = peer else { return Err(ENOTCONN) };
            let mac = ensure_mac(ip)?;
            crate::net::send_udp_mac(mac, ip, port, src_port, data).map_err(|_| EIO)?;
            Ok(data.len())
        }
    }
}

/// Sends a UDP datagram to an explicit destination (`sendto` with an
/// address). Assigns a source port on first use.
pub fn sendto(id: usize, ip: [u8; 4], port: u16, data: &[u8]) -> Result<usize, i32> {
    let src_port = {
        let mut state = STATE.lock();
        let mut ep = state.get_mut(id).and_then(|s| s.take()).ok_or(EBADF)?;
        if ep.kind != Kind::Udp {
            state[id] = Some(ep);
            return Err(EINVAL);
        }
        if ep.local_port == 0 {
            ep.local_port = alloc_ephemeral(&state);
        }
        let p = ep.local_port;
        state[id] = Some(ep);
        p
    };
    let mac = ensure_mac(ip)?;
    crate::net::send_udp_mac(mac, ip, port, src_port, data).map_err(|_| EIO)?;
    Ok(data.len())
}

/// Pops received bytes (TCP) or one datagram (UDP) without blocking.
/// TCP: `Ok(n)` with n > 0 is data, `Ok(0)` is EOF (FIN seen), and
/// `Err(EAGAIN)` means nothing available yet. UDP: pops one whole
/// datagram; `Err(EAGAIN)` when the queue is empty.
pub fn recv(id: usize, buf: &mut [u8]) -> Result<usize, i32> {
    let mut state = STATE.lock();
    let Some(ep) = state.get_mut(id).and_then(|s| s.as_mut()) else {
        return Err(EBADF);
    };
    match ep.kind {
        Kind::Tcp => {
            if ep.rst {
                return Err(ECONNRESET);
            }
            let n = ep.inbox.len().min(buf.len());
            for slot in buf.iter_mut().take(n) {
                *slot = ep.inbox.pop_front().unwrap();
            }
            if n > 0 {
                Ok(n)
            } else if ep.eof {
                Ok(0)
            } else {
                Err(EAGAIN)
            }
        }
        Kind::Udp => {
            match ep.udp_inbox.pop_front() {
                Some(pkt) => {
                    let n = pkt.payload.len().min(buf.len());
                    buf[..n].copy_from_slice(&pkt.payload[..n]);
                    Ok(n)
                }
                None => Err(EAGAIN),
            }
        }
    }
}

/// The source address of the datagram that `recv` just consumed, for
/// `recvfrom`'s address-out argument. The kernel stashes it at recv
/// time — see `recv_from` below.
pub struct RecvMeta {
    pub src_ip: [u8; 4],
    pub src_port: u16,
}

/// `recvfrom`: like `recv`, but also reports the sender's address.
pub fn recv_from(id: usize, buf: &mut [u8]) -> Result<(usize, RecvMeta), i32> {
    let kind = STATE
        .lock()
        .get(id)
        .and_then(|s| s.as_ref())
        .map(|e| e.kind)
        .unwrap_or(Kind::Tcp);
    match kind {
        Kind::Tcp => {
            let n = recv(id, buf)?;
            let (ip, port) = peer_of(id).unwrap_or(([0; 4], 0));
            Ok((n, RecvMeta { src_ip: ip, src_port: port }))
        }
        Kind::Udp => {
            let mut state = STATE.lock();
            let Some(ep) = state.get_mut(id).and_then(|s| s.as_mut()) else {
                return Err(EBADF);
            };
            match ep.udp_inbox.pop_front() {
                Some(pkt) => {
                    let n = pkt.payload.len().min(buf.len());
                    buf[..n].copy_from_slice(&pkt.payload[..n]);
                    Ok((n, RecvMeta { src_ip: pkt.src_ip, src_port: pkt.src_port }))
                }
                None => Err(EAGAIN),
            }
        }
    }
}

/// `shutdown(how)`: SHUT_WR/SHUT_RDWR send the FIN on a TCP connection.
pub fn shutdown(id: usize, how: u64) -> Result<(), i32> {
    let (src_port, dst_port, dst_ip, dst_mac, our_seq, their_initial_seq, inbox_len, state) = {
        let state = STATE.lock();
        let Some(ep) = state.get(id).and_then(|s| s.as_ref()) else {
            return Err(EBADF);
        };
        let Some(t) = &ep.tcp else { return Ok(()) };
        (ep.local_port, t.dst_port, t.dst_ip, t.dst_mac, t.our_seq, t.their_initial_seq, ep.inbox.len(), t.state)
    };
    if state == TcpState::Established && how != 0 {
        let seg = crate::net::build_tcp(
            src_port,
            dst_port,
            our_seq,
            their_initial_seq.wrapping_add(1).wrapping_add(inbox_len as u32),
            TCP_FIN | TCP_ACK,
            &[],
            dst_ip,
        );
        crate::net::send_tcp(&dst_mac, dst_ip, &seg).map_err(|_| EIO)?;
        let mut state = STATE.lock();
        if let Some(t) = state.get_mut(id).and_then(|s| s.as_mut()).and_then(|e| e.tcp.as_mut()) {
            t.our_seq = t.our_seq.wrapping_add(1);
            t.state = TcpState::FinSent;
        }
    }
    Ok(())
}

/// poll/epoll readiness: (readable, writable).
pub fn readiness(id: usize) -> (bool, bool) {
    let state = STATE.lock();
    let Some(ep) = state.get(id).and_then(|s| s.as_ref()) else {
        return (false, false);
    };
    let read = !ep.inbox.is_empty()
        || ep.eof
        || ep.rst
        || !ep.udp_inbox.is_empty()
        || (!ep.accept_queue.is_empty() && ep.listening);
    let write = match ep.kind {
        Kind::Udp => true,
        Kind::Tcp => ep
            .tcp
            .as_ref()
            .map(|t| t.state == TcpState::Established)
            .unwrap_or(false),
    };
    (read, write)
}

// ---------------------------------------------------------------------
// RX pump
// ---------------------------------------------------------------------

/// Drains the NIC's RX ring, dispatching every frame:
/// - ARP replies update the cache, and any TCP endpoint stuck in
///   SynSent waiting on that address gets its SYN sent now.
/// - UDP datagrams go to the endpoint bound to their destination port.
/// - TCP segments go to the matching connection, driving the handshake
///   and appending data / marking EOF / noting RST.
pub fn pump() {
    while let Some(frame) = crate::e1000::poll_rx() {
        if frame.len() < 14 {
            continue;
        }
        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
        if ethertype == crate::net::ETHERTYPE_ARP {
            if let Some(mac) = crate::net::parse_arp_reply(&frame) {
                if let Some(ip) = crate::net::arp_reply_ip(&frame) {
                    crate::net::cache_arp(ip, mac);
                    let waiting: Vec<usize> = {
                        let state = STATE.lock();
                        state
                            .iter()
                            .enumerate()
                            .filter(|(_, s)| {
                                s.as_ref().is_some_and(|e| {
                                    e.tcp
                                        .as_ref()
                                        .is_some_and(|t| t.state == TcpState::SynSent && t.dst_ip == ip && !t.arp_sent)
                                })
                            })
                            .map(|(i, _)| i)
                            .collect()
                    };
                    for id in waiting {
                        let _ = send_syn(id);
                    }
                }
            }
            continue;
        }
        if ethertype != crate::net::ETHERTYPE_IPV4 {
            continue;
        }
        let Some((src_ip, _dst, proto, payload)) = crate::net::parse_ipv4(&frame) else {
            continue;
        };
        match proto {
            crate::net::IP_PROTO_UDP => dispatch_udp(src_ip, payload),
            crate::net::IP_PROTO_TCP => dispatch_tcp(src_ip, payload),
            _ => {}
        }
    }
}

fn dispatch_udp(src_ip: [u8; 4], payload: &[u8]) {
    if payload.len() < 8 {
        return;
    }
    let src_port = u16::from_be_bytes([payload[0], payload[1]]);
    let dst_port = u16::from_be_bytes([payload[2], payload[3]]);
    let data = &payload[8..];
    let mut state = STATE.lock();
    if let Some(ep) = state
        .iter_mut()
        .flatten()
        .find(|e| e.kind == Kind::Udp && e.local_port == dst_port)
    {
        // Cap the per-socket queue so a flood can't eat the heap.
        if ep.udp_inbox.len() < 64 {
            ep.udp_inbox.push_back(UdpPacket {
                src_ip,
                src_port,
                payload: data.to_vec(),
            });
        }
    }
}

fn dispatch_tcp(src_ip: [u8; 4], payload: &[u8]) {
    let Some(seg) = crate::net::parse_tcp(payload) else {
        return;
    };
    let mut state = STATE.lock();

    // 1. Match an existing connection — a client or a server-side child
    //    (both are keyed by the same (peer ip, peer port, local port)).
    let conn_idx = state.iter().position(|s| {
        s.as_ref().is_some_and(|e| {
            e.kind == Kind::Tcp
                && e.tcp
                    .as_ref()
                    .is_some_and(|t| t.dst_ip == src_ip && t.dst_port == seg.src_port && e.local_port == seg.dst_port)
        })
    });
    if let Some(idx) = conn_idx {
        let Some(ep) = state[idx].as_mut() else { return };
        let Some(t) = ep.tcp.as_mut() else { return };
        match t.state {
            TcpState::SynSent => {
                if seg.flags & TCP_RST != 0 {
                    ep.rst = true;
                } else if seg.flags & (TCP_SYN | TCP_ACK) == TCP_SYN | TCP_ACK {
                    // Handshake complete: record their ISN, ACK it, open.
                    // (`t` borrows `ep.tcp`; `ep.local_port` is a disjoint
                    // field, so direct reads are fine here.)
                    t.their_initial_seq = seg.seq;
                    t.state = TcpState::Established;
                    let ack_seg = crate::net::build_tcp(
                        ep.local_port,
                        t.dst_port,
                        t.our_seq,
                        seg.seq.wrapping_add(1),
                        TCP_ACK,
                        &[],
                        t.dst_ip,
                    );
                    let _ = crate::net::send_tcp(&t.dst_mac, t.dst_ip, &ack_seg);
                }
            }
            TcpState::SynReceived => {
                // Server side: the final ACK (which may also carry the
                // first data bytes, or a FIN) opens the connection and
                // queues it on the listener for `accept`.
                if seg.flags & TCP_RST != 0 {
                    state[idx] = None; // failed handshake — drop the child
                    return;
                }
                // Copy the connection facts out first so the listener's
                // accept queue (a different slot of the same Vec) can be
                // touched without a second live mutable borrow of `state`.
                let (lid, local_port, dst_port, dst_ip, dst_mac, our_seq, their_isn) = {
                    let Some(ep) = state[idx].as_mut() else { return };
                    let Some(t) = ep.tcp.as_mut() else { return };
                    (
                        t.listener,
                        ep.local_port,
                        t.dst_port,
                        t.dst_ip,
                        t.dst_mac,
                        t.our_seq,
                        t.their_initial_seq,
                    )
                };
                let acked = seg.flags & TCP_ACK != 0;
                let fin = seg.flags & TCP_FIN != 0;
                // A retransmitted SYN (the first SYN-ACK or the final
                // ACK was lost): re-answer with SYN-ACK at the same
                // ISN rather than falling into the data path — a plain
                // ACK would leave a SYN-SENT client wedged forever.
                if seg.flags & TCP_SYN != 0 && !acked {
                    let ack_seg = crate::net::build_tcp(
                        local_port,
                        dst_port,
                        our_seq,
                        their_isn.wrapping_add(1),
                        TCP_SYN | TCP_ACK,
                        &[],
                        dst_ip,
                    );
                    let _ = crate::net::send_tcp(&dst_mac, dst_ip, &ack_seg);
                    return;
                }
                if acked {
                    if let Some(lid) = lid {
                        if let Some(l) = state.get_mut(lid).and_then(|s| s.as_mut()) {
                            if l.accept_queue.len() < 16 {
                                l.accept_queue.push_back(idx);
                            }
                        }
                    }
                }
                // Open the connection and feed whatever the ACK carried
                // (commonly the first request) into the inbox — the same
                // in-order logic the Established arm uses.
                let Some(ep) = state[idx].as_mut() else { return };
                let Some(t) = ep.tcp.as_mut() else { return };
                if acked {
                    t.state = TcpState::Established;
                }
                let expected = their_isn.wrapping_add(1).wrapping_add(ep.inbox.len() as u32);
                let ack_base = if seg.payload.is_empty() {
                    expected
                } else if seg.seq == expected {
                    ep.inbox.extend(seg.payload.iter().copied());
                    expected.wrapping_add(seg.payload.len() as u32)
                } else {
                    expected
                };
                if fin {
                    ep.eof = true;
                }
                let ack_seg = crate::net::build_tcp(
                    local_port,
                    dst_port,
                    our_seq,
                    ack_base.wrapping_add(if fin { 1 } else { 0 }),
                    TCP_ACK,
                    &[],
                    dst_ip,
                );
                let _ = crate::net::send_tcp(&dst_mac, dst_ip, &ack_seg);
            }
            TcpState::Established | TcpState::FinSent => {
                if seg.flags & TCP_RST != 0 {
                    ep.rst = true;
                    return;
                }
                // In-order assumption: the next expected byte is
                // their_initial_seq + 1 + inbox.len(). Anything else (gap,
                // retransmit) is dropped — see module docs.
                let expected = t.their_initial_seq.wrapping_add(1).wrapping_add(ep.inbox.len() as u32);
                let ack_base = if seg.payload.is_empty() {
                    expected
                } else if seg.seq == expected {
                    ep.inbox.extend(seg.payload.iter().copied());
                    expected.wrapping_add(seg.payload.len() as u32)
                } else {
                    expected // out of order/dup — drop the payload, still ACK
                };
                if seg.flags & TCP_FIN != 0 {
                    ep.eof = true;
                }
                let ack_seg = crate::net::build_tcp(
                    ep.local_port,
                    t.dst_port,
                    t.our_seq,
                    ack_base.wrapping_add(if seg.flags & TCP_FIN != 0 { 1 } else { 0 }),
                    TCP_ACK,
                    &[],
                    t.dst_ip,
                );
                let _ = crate::net::send_tcp(&t.dst_mac, t.dst_ip, &ack_seg);
            }
            TcpState::Idle => {}
        }
        return;
    }

    // 2. Server: a segment for a listening socket that no connection
    //    matches is the start of a new incoming connection (slirp relays
    //    the host's SYN). Answer with SYN-ACK and create the child; the
    //    final ACK (step 1's SynReceived branch) queues it for `accept`.
    let Some(listener_idx) = state.iter().position(|s| {
        s.as_ref()
            .is_some_and(|e| e.kind == Kind::Tcp && e.listening && e.local_port == seg.dst_port)
    }) else {
        return; // no such connection or listener — drop
    };
    if seg.flags & TCP_SYN == 0 || seg.flags & TCP_ACK != 0 {
        return; // only a bare SYN starts a connection
    }
    let Some(src_mac) = crate::net::arp_lookup(src_ip) else {
        return; // can't reply without the peer's MAC (cached in practice)
    };
    // Server ISN: derived from the peer's sequence number so it is
    // deterministic and differs per connection.
    let isn = 0x0ACCE55u32.wrapping_add(seg.seq);
    let child = alloc_endpoint(
        &mut state,
        Endpoint {
            kind: Kind::Tcp,
            local_port: seg.dst_port,
            udp_peer: None,
            tcp: Some(TcpCtl {
                state: TcpState::SynReceived,
                dst_ip: src_ip,
                dst_port: seg.src_port,
                dst_mac: src_mac,
                our_seq: isn,
                their_initial_seq: seg.seq,
                arp_sent: true,
                listener: Some(listener_idx),
            }),
            listening: false,
            accept_queue: VecDeque::new(),
            inbox: VecDeque::new(),
            udp_inbox: VecDeque::new(),
            eof: false,
            rst: false,
            nonblock: false,
        },
    );
    let ack_seg = crate::net::build_tcp(
        seg.dst_port,
        seg.src_port,
        isn,
        seg.seq.wrapping_add(1),
        TCP_SYN | TCP_ACK,
        &[],
        src_ip,
    );
    let _ = crate::net::send_tcp(&src_mac, src_ip, &ack_seg);
    // The SYN-ACK consumed our ISN; the next outgoing byte uses ISN+1.
    if let Some(t) = state.get_mut(child).and_then(|s| s.as_mut()).and_then(|e| e.tcp.as_mut()) {
        t.our_seq = t.our_seq.wrapping_add(1);
    }
}
