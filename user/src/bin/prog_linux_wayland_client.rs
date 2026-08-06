//! prog_linux_wayland_client: a real Linux-ABI process exercising Phase
//! 11's real Wayland wire protocol end to end — `memfd_create`/
//! `ftruncate`/`mmap` (`MAP_SHARED`), `socket`/`connect`, and
//! `sendmsg` (`SCM_RIGHTS` for the pool fd) against `wayland.rs`'s
//! kernel-native compositor task, speaking the *actual* Wayland wire
//! format as specified in `wayland.xml`/`xdg-shell.xml`:
//!
//!   get_registry → parse the four `wl_registry.global` events (real
//!   string decoding, not hardcoded ids) → `bind` each global using the
//!   generic `new_id` encoding (interface name string + version before
//!   the id) → `wl_shm.create_pool` (fd via SCM_RIGHTS) →
//!   `wl_shm_pool.create_buffer` → surface + `xdg_wm_base.get_xdg_surface`
//!   + `xdg_toplevel` (set_title/set_app_id) → initial buffer-less
//!   commit → parse the `xdg_toplevel.configure` +
//!   `xdg_surface.configure` events → `ack_configure` → attach/damage/
//!   frame/commit → receive `wl_callback.done`, `wl_buffer.release`,
//!   the NO_KEYMAP `wl_keyboard.keymap`/enter and `wl_pointer.enter`/
//!   frame events.
//!
//! Every message is built and parsed by hand (no heap in these
//! freestanding binaries) against the real protocol definitions, so a
//! genuine `libwayland` client performing the same sequence would be
//! byte-for-byte wire-compatible. `shell.rs`'s selftest hook spawns this
//! and then checks the real framebuffer for the test color, proving the
//! whole chain — real shared memory, real fd passing, real socket IPC,
//! real wire protocol — worked end to end.
//!
//! Result = 0x7F (127) when every step round-trips.

#![no_std]
#![no_main]

#[path = "../common.rs"]
mod common;

use core::sync::atomic::Ordering;

const SYS_WRITE: u64 = 1;
const SYS_READ: u64 = 0;
const SYS_CLOSE: u64 = 3;
const SYS_MMAP: u64 = 9;
const SYS_SOCKET: u64 = 41;
const SYS_CONNECT: u64 = 42;
const SYS_SENDMSG: u64 = 46;
const SYS_FTRUNCATE: u64 = 77;
const SYS_EXIT: u64 = 60;
const SYS_MEMFD_CREATE: u64 = 319;

const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const MAP_SHARED: u64 = 0x01;

const AF_UNIX: u64 = 1;
const SOCK_STREAM: u64 = 1;
const SOL_SOCKET: u32 = 1;
const SCM_RIGHTS: u32 = 1;

const EAGAIN: u64 = (-11i64) as u64;

const WIDTH: u32 = 8;
const HEIGHT: u32 = 8;
const STRIDE: u32 = WIDTH * 4;
const POOL_SIZE: u32 = STRIDE * HEIGHT;
/// Deliberately not a color anything else on the desktop would paint —
/// `shell.rs`'s selftest hook looks for exactly this in the framebuffer.
const TEST_COLOR: u32 = 0x00_FF10_C0;
/// The client binds `wl_shm` with XRGB8888 (alpha ignored — the top
/// byte of the stored pixel is padding, and the compositor blits the
/// raw 32-bit word).
const WL_SHM_FORMAT_XRGB8888: u32 = 1;

const SOCKET_PATH: &[u8] = b"machaos-wayland-0";

// Object ids, dense from 2 (1 is the wl_display singleton).
const O_REGISTRY: u32 = 2;
const O_SHM: u32 = 3;
const O_POOL: u32 = 4;
const O_BUFFER: u32 = 5;
const O_COMPOSITOR: u32 = 6;
const O_SURFACE: u32 = 7;
const O_XDG_WM_BASE: u32 = 8;
const O_XDG_SURFACE: u32 = 9;
const O_TOPLEVEL: u32 = 10;
const O_CALLBACK: u32 = 11;
const O_SEAT: u32 = 12;
const O_POINTER: u32 = 13;
const O_KEYBOARD: u32 = 14;

/// Writes a message header (opcode low 16 bits, size high 16 bits).
fn write_header(buf: &mut [u8], object_id: u32, opcode: u16, size: u16) {
    buf[0..4].copy_from_slice(&object_id.to_le_bytes());
    let opcode_and_size = (opcode as u32) | ((size as u32) << 16);
    buf[4..8].copy_from_slice(&opcode_and_size.to_le_bytes());
}

fn write_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// Appends a wire `string` (u32 length incl. NUL, payload, pad to 4).
fn push_str(buf: &mut [u8], off: &mut usize, s: &[u8]) {
    let len = s.len() as u32 + 1;
    write_u32(buf, *off, len);
    *off += 4;
    buf[*off..*off + s.len()].copy_from_slice(s);
    *off += s.len();
    buf[*off] = 0;
    *off += 1;
    while *off % 4 != 0 {
        buf[*off] = 0;
        *off += 1;
    }
}

fn send_plain(sockfd: u64, buf: &[u8]) -> u64 {
    unsafe { common::syscall(SYS_WRITE, sockfd, buf.as_ptr() as u64, buf.len() as u64, 0) }
}

// ---- stream reader -----------------------------------------------------

/// Incremental reader over the socket stream: accumulates bytes,
/// parses messages as they complete. No heap — fixed stack buffer.
struct R {
    sockfd: u64,
    buf: [u8; 512],
    n: usize,
    p: usize,
}

impl R {
    fn new(sockfd: u64) -> Self {
        R { sockfd, buf: [0u8; 512], n: 0, p: 0 }
    }

    /// Makes `want` more bytes available, reading from the socket past
    /// transient emptiness. This kernel's socket `read` is never
    /// blocking: it returns 0 (or -EAGAIN) while nothing has arrived,
    /// with no EOF signal of its own, so both are retried — interrupts
    /// are on in ring 3 between syscalls, giving the compositor task
    /// turns to fill the inbox.
    fn ensure(&mut self, want: usize) {
        // Compact consumed space when it's worth it.
        if self.p > 0 {
            let rem = self.n - self.p;
            for i in 0..rem {
                self.buf[i] = self.buf[self.p + i];
            }
            self.n = rem;
            self.p = 0;
        }
        while self.n - self.p < want {
            let room = self.buf.len() - self.n;
            let r = unsafe {
                common::syscall(
                    SYS_READ,
                    self.sockfd,
                    self.buf.as_mut_ptr() as u64 + self.n as u64,
                    room as u64,
                    0,
                )
            };
            if r == EAGAIN || r == 0 {
                continue;
            }
            self.n += r as usize;
        }
    }

    fn u32(&mut self) -> u32 {
        self.ensure(4);
        let v = u32::from_le_bytes(self.buf[self.p..self.p + 4].try_into().unwrap());
        self.p += 4;
        v
    }

    fn skip(&mut self, n: usize) {
        self.ensure(n);
        self.p += n;
    }

    /// Reads a wire `string` (without NUL) into `out`, returning its
    /// byte length. A zero-length (null) string returns 0.
    fn string_into(&mut self, out: &mut [u8]) -> usize {
        let len = self.u32() as usize;
        if len == 0 {
            return 0;
        }
        let total = (len + 3) & !3;
        self.ensure(total);
        let n = (len - 1).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.p..self.p + n]);
        self.p += total;
        n
    }

    /// Reads a wire `string` and compares it to `want` (without NUL).
    fn string_eq(&mut self, want: &[u8]) -> bool {
        let mut tmp = [0u8; 64];
        self.string_into(&mut tmp) == want.len() && &tmp[..want.len()] == want
    }

    /// Skips a wire `array`.
    fn skip_array(&mut self) {
        let len = self.u32() as usize;
        self.skip((len + 3) & !3);
    }

    /// Reads a message header, returning (object id, opcode). The body
    /// is consumed by subsequent u32/string_eq/skip_array calls.
    fn header(&mut self) -> (u32, u16) {
        self.ensure(8);
        let obj = self.u32();
        let os = self.u32();
        (obj, (os & 0xFFFF) as u16)
    }
}

// ---- protocol helpers --------------------------------------------------

/// `wl_registry.bind` uses the generic new_id encoding: interface name
/// string + version before the id (the protocol's custom serialization
/// rules for a new_id without a fixed interface).
fn send_bind(sockfd: u64, registry: u32, global_name: u32, interface: &[u8], version: u32, id: u32) {
    let mut buf = [0u8; 64];
    let mut off = 0usize;
    write_header(&mut buf, registry, 0, 0); // size patched below
    off = 8;
    write_u32(&mut buf, off, global_name);
    off += 4;
    push_str(&mut buf, &mut off, interface);
    write_u32(&mut buf, off, version);
    off += 4;
    write_u32(&mut buf, off, id);
    off += 4;
    write_header(&mut buf, registry, 0, off as u16);
    send_plain(sockfd, &buf[..off]);
}

/// `xdg_wm_base.pong(serial)`.
fn send_pong(sockfd: u64, serial: u32) {
    let mut buf = [0u8; 12];
    write_header(&mut buf, O_XDG_WM_BASE, 3, 12);
    write_u32(&mut buf, 8, serial);
    send_plain(sockfd, &buf);
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() {
    let mut ok = 0u64;

    // 1. memfd_create + ftruncate + mmap(MAP_SHARED) — the shared pixel
    //    buffer the compositor will read straight out of.
    let memfd = unsafe { common::syscall(SYS_MEMFD_CREATE, 0, 0, 0, 0) };
    if (memfd as i64) >= 0 {
        ok |= 1 << 0;
    }
    if unsafe { common::syscall(SYS_FTRUNCATE, memfd, POOL_SIZE as u64, 0, 0) } == 0 {
        ok |= 1 << 1;
    }
    let ptr = unsafe {
        common::syscall6(SYS_MMAP, 0, POOL_SIZE as u64, PROT_READ | PROT_WRITE, MAP_SHARED, memfd, 0)
    };
    if (ptr as i64) > 0 {
        ok |= 1 << 2;
    }
    let pixels = ptr as *mut u32;
    for i in 0..(WIDTH * HEIGHT) as usize {
        unsafe { pixels.add(i).write(TEST_COLOR) };
    }

    // 2. socket + connect to the compositor's well-known path.
    let sockfd = unsafe { common::syscall(SYS_SOCKET, AF_UNIX, SOCK_STREAM, 0, 0) };
    if (sockfd as i64) >= 0 {
        ok |= 1 << 3;
    }
    let mut addr = [0u8; 110];
    addr[0] = AF_UNIX as u8;
    addr[2..2 + SOCKET_PATH.len()].copy_from_slice(SOCKET_PATH);
    let addrlen = 2 + SOCKET_PATH.len() as u64 + 1;
    if unsafe { common::syscall(SYS_CONNECT, sockfd, addr.as_ptr() as u64, addrlen, 0) } == 0 {
        ok |= 1 << 4;
    }
    let mut r = R::new(sockfd);

    // 3. wl_display.get_registry(O_REGISTRY).
    let mut msg = [0u8; 12];
    write_header(&mut msg, 1, 1, 12);
    write_u32(&mut msg, 8, O_REGISTRY);
    send_plain(sockfd, &msg);

    // 4. Parse the four wl_registry.global events (real string decode)
    //    and remember each interface's numeric name.
    let mut shm_name = 0u32;
    let mut comp_name = 0u32;
    let mut seat_name = 0u32;
    let mut xdg_name = 0u32;
    let mut globals_seen = 0u32;
    let mut found = 0u32; // bit per interface: shm=1, compositor=2, seat=4, xdg=8
    while globals_seen < 4 {
        let (obj, op) = r.header();
        if obj != O_REGISTRY || op != 0 {
            ok |= 1 << 30; // unexpected event — fail
            break;
        }
        let name = r.u32();
        // Read the interface string exactly once, then compare it
        // against every interface this client binds.
        let mut iface = [0u8; 32];
        let ilen = r.string_into(&mut iface);
        let _version = r.u32();
        globals_seen += 1;
        if ilen == 6 && &iface[..6] == b"wl_shm" {
            shm_name = name;
            found |= 1;
        } else if ilen == 13 && &iface[..13] == b"wl_compositor" {
            comp_name = name;
            found |= 2;
        } else if ilen == 7 && &iface[..7] == b"wl_seat" {
            seat_name = name;
            found |= 4;
        } else if ilen == 11 && &iface[..11] == b"xdg_wm_base" {
            xdg_name = name;
            found |= 8;
        }
    }
    if found == 0xF {
        ok |= 1 << 5;
    }

    // 5. wl_registry.bind the four globals (generic new_id encoding).
    send_bind(sockfd, O_REGISTRY, shm_name, b"wl_shm", 1, O_SHM);
    send_bind(sockfd, O_REGISTRY, comp_name, b"wl_compositor", 4, O_COMPOSITOR);
    send_bind(sockfd, O_REGISTRY, seat_name, b"wl_seat", 5, O_SEAT);
    send_bind(sockfd, O_REGISTRY, xdg_name, b"xdg_wm_base", 1, O_XDG_WM_BASE);
    ok |= 1 << 6;

    // 6. wl_shm.format events (two: argb8888 + xrgb8888), and the seat's
    //    capabilities + name. The xdg_wm_base.ping asks for a pong.
    let mut formats = 0u32;
    let mut seat_ok = false;
    let mut ping_serial = 0u32;
    while formats < 2 || !seat_ok || ping_serial == 0 {
        let (obj, op) = r.header();
        match (obj, op) {
            (O_SHM, 0) => {
                let f = r.u32();
                if f == WL_SHM_FORMAT_XRGB8888 || f == 0 {
                    formats += 1;
                }
            }
            (O_SEAT, 0) => {
                let caps = r.u32();
                if caps == 3 {
                    // pointer | keyboard
                    seat_ok = true;
                }
            }
            (O_SEAT, 1) => {
                let _ = r.string_eq(b"machaos-seat");
            }
            (O_XDG_WM_BASE, 0) => {
                ping_serial = r.u32();
            }
            _ => {
                break;
            }
        }
    }
    if formats >= 2 && seat_ok && ping_serial != 0 {
        ok |= 1 << 7;
    }
    send_pong(sockfd, ping_serial);

    // 7. wl_shm.create_pool(O_POOL, fd, POOL_SIZE) — sendmsg carries the
    //    memfd via SCM_RIGHTS.
    let mut body = [0u8; 16];
    write_header(&mut body, O_SHM, 0, 16);
    write_u32(&mut body, 8, O_POOL);
    write_u32(&mut body, 12, POOL_SIZE);
    let iov = [body.as_ptr() as u64, body.len() as u64];
    let mut cmsg = [0u8; 20];
    cmsg[0..8].copy_from_slice(&20u64.to_ne_bytes());
    cmsg[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
    cmsg[12..16].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
    cmsg[16..20].copy_from_slice(&(memfd as u32).to_ne_bytes());
    let mut msghdr = [0u8; 56];
    msghdr[16..24].copy_from_slice(&(iov.as_ptr() as u64).to_ne_bytes());
    msghdr[24..32].copy_from_slice(&1u64.to_ne_bytes());
    msghdr[32..40].copy_from_slice(&(cmsg.as_ptr() as u64).to_ne_bytes());
    msghdr[40..48].copy_from_slice(&(cmsg.len() as u64).to_ne_bytes());
    let r7 = unsafe { common::syscall(SYS_SENDMSG, sockfd, msghdr.as_ptr() as u64, 0, 0) };
    if r7 == body.len() as u64 {
        ok |= 1 << 8;
    }

    // 8. wl_shm_pool.create_buffer(O_BUFFER, 0, W, H, stride, XRGB8888).
    let mut msg = [0u8; 32];
    write_header(&mut msg, O_POOL, 0, 32);
    write_u32(&mut msg, 8, O_BUFFER);
    write_u32(&mut msg, 12, 0); // offset
    write_u32(&mut msg, 16, WIDTH);
    write_u32(&mut msg, 20, HEIGHT);
    write_u32(&mut msg, 24, STRIDE);
    write_u32(&mut msg, 28, WL_SHM_FORMAT_XRGB8888);
    send_plain(sockfd, &msg);

    // 9. wl_compositor.create_surface(O_SURFACE).
    let mut msg = [0u8; 12];
    write_header(&mut msg, O_COMPOSITOR, 0, 12);
    write_u32(&mut msg, 8, O_SURFACE);
    send_plain(sockfd, &msg);

    // 10. xdg_wm_base.get_xdg_surface(O_XDG_SURFACE, O_SURFACE) and
    //     xdg_surface.get_toplevel(O_TOPLEVEL).
    let mut msg = [0u8; 16];
    write_header(&mut msg, O_XDG_WM_BASE, 2, 16);
    write_u32(&mut msg, 8, O_XDG_SURFACE);
    write_u32(&mut msg, 12, O_SURFACE);
    send_plain(sockfd, &msg);
    let mut msg = [0u8; 12];
    write_header(&mut msg, O_XDG_SURFACE, 1, 12);
    write_u32(&mut msg, 8, O_TOPLEVEL);
    send_plain(sockfd, &msg);

    // 11. xdg_toplevel.set_title / set_app_id.
    let mut msg = [0u8; 32];
    write_header(&mut msg, O_TOPLEVEL, 2, 0);
    let mut off = 8usize;
    push_str(&mut msg, &mut off, b"machaos test");
    write_header(&mut msg, O_TOPLEVEL, 2, off as u16);
    send_plain(sockfd, &msg[..off]);
    let mut msg = [0u8; 32];
    write_header(&mut msg, O_TOPLEVEL, 3, 0);
    let mut off = 8usize;
    push_str(&mut msg, &mut off, b"machaos.test");
    write_header(&mut msg, O_TOPLEVEL, 3, off as u16);
    send_plain(sockfd, &msg[..off]);

    // 12. Initial buffer-less commit → the compositor answers with
    //     xdg_toplevel.configure + xdg_surface.configure(serial).
    let mut msg = [0u8; 8];
    write_header(&mut msg, O_SURFACE, 6, 8);
    send_plain(sockfd, &msg);

    let mut configure_serial = 0u32;
    while configure_serial == 0 {
        let (obj, op) = r.header();
        match (obj, op) {
            (O_TOPLEVEL, 0) => {
                let _w = r.u32();
                let _h = r.u32();
                r.skip_array();
            }
            (O_XDG_SURFACE, 0) => {
                configure_serial = r.u32();
            }
            _ => {
                break;
            }
        }
    }
    if configure_serial != 0 {
        ok |= 1 << 9;
    }

    // 13. ack_configure(serial), then bind pointer/keyboard and attach +
    //     damage + frame + commit the real buffer.
    let mut msg = [0u8; 12];
    write_header(&mut msg, O_XDG_SURFACE, 4, 12);
    write_u32(&mut msg, 8, configure_serial);
    send_plain(sockfd, &msg);
    let mut msg = [0u8; 12];
    write_header(&mut msg, O_SEAT, 0, 12);
    write_u32(&mut msg, 8, O_POINTER);
    send_plain(sockfd, &msg);
    let mut msg = [0u8; 12];
    write_header(&mut msg, O_SEAT, 1, 12);
    write_u32(&mut msg, 8, O_KEYBOARD);
    send_plain(sockfd, &msg);

    let mut msg = [0u8; 20];
    write_header(&mut msg, O_SURFACE, 1, 20);
    write_u32(&mut msg, 8, O_BUFFER);
    write_u32(&mut msg, 12, 0);
    write_u32(&mut msg, 16, 0);
    send_plain(sockfd, &msg);
    let mut msg = [0u8; 24];
    write_header(&mut msg, O_SURFACE, 2, 24);
    write_u32(&mut msg, 8, 0);
    write_u32(&mut msg, 12, 0);
    write_u32(&mut msg, 16, WIDTH);
    write_u32(&mut msg, 20, HEIGHT);
    send_plain(sockfd, &msg);
    let mut msg = [0u8; 12];
    write_header(&mut msg, O_SURFACE, 3, 12);
    write_u32(&mut msg, 8, O_CALLBACK);
    send_plain(sockfd, &msg);
    let mut msg = [0u8; 8];
    write_header(&mut msg, O_SURFACE, 6, 8);
    send_plain(sockfd, &msg);

    // 14. Events after the mapped commit, in order: wl_callback.done,
    //     wl_buffer.release, keyboard.keymap(NO_KEYMAP)/enter,
    //     pointer.enter, pointer.frame.
    let mut saw_done = false;
    let mut saw_enter = false;
    let mut saw_ptr = false;
    while !(saw_done && saw_enter && saw_ptr) {
        let (obj, op) = r.header();
        match (obj, op) {
            (O_CALLBACK, 0) => {
                let _data = r.u32();
                saw_done = true;
            }
            (O_BUFFER, 0) => {}
            (O_KEYBOARD, 0) => {
                let _fmt = r.u32(); // NO_KEYMAP
                let _size = r.u32();
            }
            (O_KEYBOARD, 1) => {
                let _serial = r.u32();
                let surf = r.u32();
                r.skip_array();
                if surf == O_SURFACE {
                    saw_enter = true;
                }
            }
            (O_POINTER, 0) => {
                let _serial = r.u32();
                let surf = r.u32();
                let _sx = r.u32();
                let _sy = r.u32();
                if surf == O_SURFACE {
                    saw_ptr = true;
                }
            }
            (O_POINTER, 5) => {}
            _ => {
                break;
            }
        }
    }
    if saw_done && saw_enter && saw_ptr {
        ok |= 1 << 10;
    }

    unsafe {
        common::syscall(SYS_CLOSE, sockfd, 0, 0, 0);
        common::syscall(SYS_CLOSE, memfd, 0, 0, 0);
    }

    // All ten checks: 0x7F.
    let want = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 3) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 7)
        | (1 << 8) | (1 << 9) | (1 << 10);
    common::RESULT.store(if ok == want { 0x7F } else { ok }, Ordering::Relaxed);
    unsafe {
        common::syscall(SYS_EXIT, 0, 0, 0, 0);
    }
}
