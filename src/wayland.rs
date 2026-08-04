//! Phase 5: a minimal, Wayland-*inspired* compositor endpoint, kernel-
//! native (its own background task — see `task::spawn_kernel_task`), for
//! a real Linux-ABI GUI client to render a pixel buffer through.
//!
//! This is the piece Phase 4's plan notes called out as a separate,
//! larger project once dynamic linking was stable: real `wl_shm`-based
//! rendering needs `AF_UNIX` sockets, `SCM_RIGHTS` fd-passing, and
//! genuinely shared (not copied) memory between two processes — none of
//! which existed before this phase (see `socket.rs`/`shm.rs`, and
//! `process::Process::mmap_shared`). Those three pieces are real Linux
//! ABI surface, exercised by a real Linux-ABI test client
//! (`user/src/bin/prog_linux_wayland_client.rs`) issuing real
//! `socket`/`connect`/`sendmsg`/`recvmsg`/`memfd_create`/`ftruncate`/
//! `mmap` syscalls. What's *not* real is the wire protocol carried over
//! that socket: it borrows Wayland's shape (numbered objects, an
//! opcode+size header, requests flowing one way and events the other,
//! `SCM_RIGHTS` for the shared-memory pool's fd) but isn't the actual
//! Wayland wire format — no interface-string/version negotiation, no
//! generic `new_id` encoding, no real `wl_display`/`wl_registry`/
//! `wl_shm`/`wl_compositor`/`wl_surface` interfaces, and the compositor
//! doesn't keep a real per-connection object table: it just walks
//! `client_session`'s hardcoded step sequence once per connection and
//! trusts the client to follow it. A real Wayland client (GTK, Qt, a
//! genuine `libwayland-client`) could not talk to this. Given a full
//! wire-compatible implementation plus `xdg-shell`, input event
//! forwarding, and multi-client compositing is a project on the scale of
//! everything else in this file's siblings combined, this proves the
//! *mechanism* — a real client process handing the compositor a real
//! shared frame of pixels entirely through real Linux syscalls, which
//! then land on MachaOS's actual screen — and leaves protocol fidelity
//! as future work, consistent with this project's plan explicitly
//! scoping Phase 5 at roadmap, not full-completion, rigor.
//!
//! KNOWN GAPS:
//! - One client at a time; a second `connect` while one is in flight
//!   just queues on the listener's backlog until the first disconnects.
//! - No real threading (`clone`/`CLONE_THREAD`) exists yet (see
//!   `linux_abi.rs`'s module docs) — a real toolkit client that spawns
//!   worker threads during startup won't run at all. The hand-written
//!   test client is deliberately single-threaded.
//! - No damage tracking / partial redraw, no double-buffering beyond
//!   the client's own pool, no input forwarding, no `xdg-shell`
//!   (window management) — `commit` just blits the whole attached
//!   buffer to a fixed screen position and presents.

use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::{fb, gfx, interrupts, shm, socket};

/// Purely a lookup key in `socket.rs` — see that module's docs on why
/// binding a path needs no real filesystem entry.
const SOCKET_PATH: &str = "machaos-wayland-0";

/// Where `client_session` blits a committed buffer on the real screen.
const SURFACE_X: u32 = 40;
const SURFACE_Y: u32 = 40;

/// Bumped once per successfully rendered `commit` — the selftest polls
/// this instead of guessing how many ticks the compositor task needs.
pub static FRAMES_RENDERED: AtomicU64 = AtomicU64::new(0);

/// Starts the compositor as a permanent background kernel task. A no-op
/// without a real framebuffer to render onto.
pub fn start() {
    if !fb::is_graphics_mode() {
        return;
    }
    crate::task::spawn_kernel_task(compositor_main, "wayland");
}

fn compositor_main() -> ! {
    let listener = socket::create();
    // Both of these only fail if something else already bound the same
    // path or `listener` isn't fresh — neither can happen for the one
    // listener this task ever creates, for the one path it ever binds.
    socket::bind(listener, SOCKET_PATH);
    socket::listen(listener);
    loop {
        let conn = accept_blocking(listener);
        client_session(conn);
        socket::close(conn);
    }
}

fn accept_blocking(listener: usize) -> usize {
    loop {
        if let Some(conn) = socket::accept(listener) {
            return conn;
        }
        interrupts::halt();
    }
}

/// Accumulates bytes/fds off a connection across possibly many
/// underlying `socket::recv` calls, so a caller can ask for an exact
/// message size without caring how the client's writes happened to be
/// chunked (or coalesced) by the time this task gets scheduled — the
/// same problem a real stream socket reader always has to solve.
struct ConnReader {
    conn: usize,
    buf: Vec<u8>,
    pos: usize,
    fds: Vec<usize>,
}

impl ConnReader {
    fn new(conn: usize) -> Self {
        ConnReader { conn, buf: Vec::new(), pos: 0, fds: Vec::new() }
    }

    fn fill_to(&mut self, want: usize) {
        let mut scratch = [0u8; 512];
        while self.buf.len() - self.pos < want {
            let (n, fds) = socket::recv(self.conn, &mut scratch);
            if n == 0 {
                interrupts::halt();
                continue;
            }
            self.buf.extend_from_slice(&scratch[..n]);
            self.fds.extend(fds);
        }
    }

    fn read_exact(&mut self, want: usize) -> Vec<u8> {
        self.fill_to(want);
        let out = self.buf[self.pos..self.pos + want].to_vec();
        self.pos += want;
        out
    }

    fn read_u32(&mut self) -> u32 {
        u32::from_ne_bytes(self.read_exact(4).try_into().unwrap())
    }

    /// The message header every step starts with: `object_id`, `opcode`,
    /// `size` (total bytes including this 8-byte header) — read but not
    /// meaningfully validated (see module docs: no real object table).
    fn read_header(&mut self) -> u32 {
        let _object_id = self.read_u32();
        let opcode_and_size = self.read_u32();
        opcode_and_size & 0xFFFF // low 16 bits: total message size
    }

    fn take_fd(&mut self) -> Option<usize> {
        if self.fds.is_empty() {
            None
        } else {
            Some(self.fds.remove(0))
        }
    }
}

fn push_header(buf: &mut Vec<u8>, object_id: u32, opcode: u16, size: u16) {
    buf.extend_from_slice(&object_id.to_ne_bytes());
    buf.extend_from_slice(&(opcode as u32 | ((size as u32) << 16)).to_ne_bytes());
}

fn push_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_ne_bytes());
}

fn push_str(buf: &mut Vec<u8>, s: &str) {
    push_u32(buf, s.len() as u32);
    buf.extend_from_slice(s.as_bytes());
}

/// `wl_registry.global(name, interface, version)` — opcode 0, addressed
/// to `registry_id`.
fn send_global(conn: usize, registry_id: u32, name: u32, interface: &str, version: u32) {
    let mut body = Vec::new();
    push_u32(&mut body, name);
    push_str(&mut body, interface);
    push_u32(&mut body, version);
    let mut msg = Vec::new();
    push_header(&mut msg, registry_id, 0, (8 + body.len()) as u16);
    msg.extend_from_slice(&body);
    socket::send(conn, &msg, &[]);
}

/// Walks one client connection through the fixed handshake
/// `prog_linux_wayland_client.rs` performs — see module docs for why
/// this is a scripted sequence rather than a real dispatch table.
fn client_session(conn: usize) {
    let mut r = ConnReader::new(conn);

    // 1. wl_display.get_registry(new_id) — the only field this minimal
    //    compositor actually needs back is the id the client chose for
    //    its registry object, to address the `global` events at.
    let size = r.read_header();
    if size != 12 {
        return;
    }
    let registry_id = r.read_u32();

    // 2. Advertise the two globals a `wl_shm`-based client needs.
    send_global(conn, registry_id, 1, "wl_shm", 1);
    send_global(conn, registry_id, 2, "wl_compositor", 4);

    // 3. wl_registry.bind(name=1, id) for wl_shm — id unused (no object
    //    table; see module docs), just consumed off the wire.
    if r.read_header() != 16 {
        return;
    }
    let _name = r.read_u32();
    let _shm_bound_id = r.read_u32();

    // 4. wl_shm.create_pool(id, size) — the fd arrives via SCM_RIGHTS on
    //    this same message (see `sys_sendmsg`), not in the byte stream.
    if r.read_header() != 16 {
        return;
    }
    let _pool_id = r.read_u32();
    let pool_size = r.read_u32() as usize;
    let Some(pool_shm_id) = r.take_fd() else {
        return;
    };

    // 5. wl_shm_pool.create_buffer(id, offset, width, height, stride, format)
    if r.read_header() != 32 {
        return;
    }
    let _buffer_id = r.read_u32();
    let offset = r.read_u32() as usize;
    let width = r.read_u32();
    let height = r.read_u32();
    let stride = r.read_u32() as usize;
    let _format = r.read_u32(); // only one format supported — see module docs

    // 6. wl_registry.bind(name=2, id) for wl_compositor.
    if r.read_header() != 16 {
        return;
    }
    let _name = r.read_u32();
    let _compositor_bound_id = r.read_u32();

    // 7. wl_compositor.create_surface(id)
    if r.read_header() != 12 {
        return;
    }
    let _surface_id = r.read_u32();

    // 8. wl_surface.attach(buffer, x, y) — the buffer id is unused (only
    //    one buffer ever exists in this minimal flow).
    if r.read_header() != 20 {
        return;
    }
    let _buffer = r.read_u32();
    let _x = r.read_u32();
    let _y = r.read_u32();

    // 9. wl_surface.commit() — render.
    if r.read_header() != 8 {
        return;
    }

    render(pool_shm_id, pool_size, offset, width, height, stride);
    shm::close(pool_shm_id); // this connection's own reference to the pool
}

/// Blits `width`x`height` pixels (native `0x00RRGGBB`, `stride` bytes
/// per row) starting at `offset` bytes into the shared-memory object
/// `shm_id` onto the real framebuffer at a fixed screen position, then
/// presents. No page-table mapping needed on this side — the kernel's
/// own low memory is already identity-mapped, so `shm::phys_of` is
/// directly readable, the kernel-native equivalent of the client's own
/// `mmap(MAP_SHARED)`.
fn render(shm_id: usize, pool_size: usize, offset: usize, width: u32, height: u32, stride: usize) {
    let Some(phys) = shm::phys_of(shm_id) else {
        return;
    };
    let need = offset + stride * height as usize;
    if need > pool_size {
        return;
    }
    let mut pixels = vec![0u32; (width * height) as usize];
    for row in 0..height as usize {
        let row_phys = phys + offset + row * stride;
        let src = unsafe { core::slice::from_raw_parts(row_phys as *const u32, width as usize) };
        pixels[row * width as usize..(row + 1) * width as usize].copy_from_slice(src);
    }
    fb::with_surface(|surface| {
        gfx::blit(surface, SURFACE_X, SURFACE_Y, &pixels, width, height);
    });
    fb::present();
    FRAMES_RENDERED.fetch_add(1, Ordering::Release);
}
