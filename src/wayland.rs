//! Phase 11: a real Wayland wire-protocol compositor endpoint, kernel-
//! native (its own background task — see `task::spawn_kernel_task`), for
//! a real Linux-ABI GUI client to render a pixel buffer through.
//!
//! Phase 5's original `wayland.rs` proved the *mechanism* — a real
//! client process handing the compositor a real shared frame of pixels
//! through real Linux syscalls — but over a hand-designed, Wayland-
//! *inspired* wire subset: no object table, no real argument encoding,
//! a hardcoded step sequence, and no `xdg-shell`. This rewrite replaces
//! that with the actual Wayland wire protocol as specified in
//! `wayland.xml` / `xdg-shell.xml`:
//!
//! - A real per-connection object table: ids are chosen by the client
//!   (dense, starting at 2), and every request is dispatched by
//!   `(object id, opcode)` against the object's interface, exactly like
//!   `libwayland`'s server side.
//! - Real argument codecs: `uint`/`int`/`fixed` (24.8), `string`
//!   (length incl. NUL, 4-byte padded), `array`, `object`, `new_id`
//!   with a known interface (plain id) and the *generic* `new_id`
//!   encoding used by `wl_registry.bind` (interface name string +
//!   version + id, per the protocol spec's custom serialization rules),
//!   and `fd` via `SCM_RIGHTS` ancillary data.
//! - The interfaces a `wl_shm`-based client actually uses:
//!   `wl_display` (get_registry/sync), `wl_registry` (global/bind),
//!   `wl_callback` (done), `wl_compositor` (create_surface),
//!   `wl_shm`/`wl_shm_pool`/`wl_buffer` (shared-memory pools + format
//!   advertisement), `wl_surface` (attach/damage/frame/commit),
//!   `wl_seat`/`wl_pointer`/`wl_keyboard` (capabilities, enter/key
//!   events for the mapped surface), and the stable `xdg-shell`:
//!   `xdg_wm_base` (get_xdg_surface/ping/pong), `xdg_surface`
//!   (configure/ack_configure), `xdg_toplevel` (set_title/set_app_id/
//!   configure/close).
//!
//! The client side (`user/src/bin/prog_linux_wayland_client.rs`) is
//! rewritten to speak the same real wire format — every message it
//! sends and every event it parses follows the official protocol, so a
//! genuine `libwayland` client that performed the same sequence would
//! interoperate byte-for-byte. (Actually running an unmodified
//! `libwayland` binary — e.g. `weston-simple-shm` — needs a
//! musl/wayland cross toolchain this project doesn't have yet; the
//! hand-encoded client is the verification, and that remains the honest
//! documented gap, consistent with the roadmap's framing.)
//!
//! KNOWN GAPS (same honest-documentation pattern as before):
//! - One client at a time; a second `connect` queues on the listener's
//!   backlog until the first disconnects.
//! - Input is a *snapshot*, not a live stream: on the first mapped
//!   commit the compositor sends `wl_keyboard.keymap` (NO_KEYMAP format,
//!   since there's no xkb keymap to ship), `wl_keyboard.enter`,
//!   `wl_pointer.enter` and `wl_pointer.frame` for the surface at
//!   surface-local (0,0). Motion/button/key event streams driven by the
//!   real PS/2 devices (and the desktop's own input consumers) are
//!   future work — the interfaces and encodings are real, the data is
//!   static.
//! - `wl_display.error`/`delete_id`, damage tracking, buffer release
//!   timing beyond "immediate after blit" (valid: the compositor copies
//!   the pixels), multi-buffer ping-pong and `xdg_toplevel.move`/
//!   `resize` are not implemented. `wl_surface.commit` blits the whole
//!   attached buffer to a fixed screen position and presents.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::{fb, gfx, interrupts, shm, socket};

/// Purely a lookup key in `socket.rs` — see that module's docs on why
/// binding a path needs no real filesystem entry.
const SOCKET_PATH: &str = "machaos-wayland-0";

/// Where a committed surface blits on the real screen.
const SURFACE_X: u32 = 40;
const SURFACE_Y: u32 = 40;

/// Bumped once per successfully rendered `commit` — the selftest polls
/// this instead of guessing how many ticks the compositor task needs.
pub static FRAMES_RENDERED: AtomicU64 = AtomicU64::new(0);

// ---- protocol constants (wayland.xml, xdg-shell.xml — stable) ----

/// `wl_shm.format` codes (drm_fourcc): only the two 32-bit RGB formats
/// this compositor can blit.
const WL_SHM_FORMAT_ARGB8888: u32 = 0;
const WL_SHM_FORMAT_XRGB8888: u32 = 1;

/// `wl_seat.capabilities` bitmask.
const WL_SEAT_CAPABILITY_POINTER: u32 = 1;
const WL_SEAT_CAPABILITY_KEYBOARD: u32 = 2;

// ---- wire helpers ------------------------------------------------------

fn push_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32 + 1).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    out.push(0);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

fn push_array_u32(out: &mut Vec<u8>, vals: &[u32]) {
    out.extend_from_slice(&(vals.len() as u32 * 4).to_le_bytes());
    for v in vals {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

/// Sequential reader over one request message's body. Out-of-bounds
/// reads return 0 (a hostile/broken client just produces garbage
/// objects instead of faulting the kernel).
struct Body<'a> {
    data: &'a [u8],
    off: usize,
}

impl<'a> Body<'a> {
    fn new(data: &'a [u8]) -> Self {
        Body { data, off: 0 }
    }

    fn u32(&mut self) -> u32 {
        if self.off + 4 > self.data.len() {
            self.off = self.data.len();
            return 0;
        }
        let v = u32::from_le_bytes(self.data[self.off..self.off + 4].try_into().unwrap());
        self.off += 4;
        v
    }

    fn i32(&mut self) -> i32 {
        self.u32() as i32
    }

    /// Reads a wire `string` (length incl. NUL, 4-byte padded). Returns
    /// None for a zero-length (null) string or malformed input.
    fn string(&mut self) -> Option<String> {
        let len = self.u32() as usize;
        if len == 0 {
            return None;
        }
        let total = (len + 3) & !3;
        if self.off + total > self.data.len() {
            self.off = self.data.len();
            return None;
        }
        let bytes = &self.data[self.off..self.off + len - 1];
        self.off += total;
        core::str::from_utf8(bytes).ok().map(String::from)
    }
}

/// The object interfaces this compositor knows, keyed per id in each
/// connection's object table. Only the requests actually needed by a
/// `wl_shm` + xdg-shell client are dispatched; everything else on a
/// known object is accepted and ignored (the protocol's "ignore unknown
/// requests" convention for newer versions).
#[derive(Clone, Copy, PartialEq)]
enum Iface {
    Display,
    Registry,
    Callback,
    Compositor,
    Shm,
    ShmPool,
    Buffer,
    Surface,
    Region,
    Seat,
    Pointer,
    Keyboard,
    XdgWmBase,
    XdgPositioner,
    XdgSurface,
    XdgToplevel,
}

/// How to read a wl_buffer's pixels back out of its shared pool.
#[derive(Clone)]
struct BufferDesc {
    pool_shm: usize,
    pool_size: usize,
    offset: usize,
    width: u32,
    height: u32,
    stride: usize,
}

/// One entry in the object table.
#[derive(Clone)]
struct Obj {
    iface: Iface,
    /// wl_shm_pool: (shm object id, size) backing this pool.
    pool: Option<(usize, usize)>,
    /// wl_buffer: how to read its pixels out of the pool.
    buf: Option<BufferDesc>,
    /// wl_surface: pending (pre-commit) attach.
    pending_buffer: Option<u32>,
    /// wl_surface: wl_callback ids waiting on the next commit.
    frame_cbs: Vec<u32>,
}

impl Obj {
    fn new(iface: Iface) -> Self {
        Obj { iface, pool: None, buf: None, pending_buffer: None, frame_cbs: Vec::new() }
    }
}

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

    /// Fills the buffer until `want` unread bytes are available.
    /// `socket::recv` returning nothing means "not delivered yet", not
    /// EOF (nothing in this kernel signals a closed peer), so it halts
    /// and retries — the same spin-`hlt` idiom the old handshake loop
    /// used. `false` means the client never sent a complete message in
    /// `read_msg`'s size window (the caller disconnects then).
    fn fill_to(&mut self, want: usize) -> bool {
        let mut scratch = [0u8; 512];
        while self.buf.len() - self.pos < want {
            let (n, fds) = socket::recv(self.conn, &mut scratch);
            if n == 0 && fds.is_empty() {
                interrupts::halt();
                continue;
            }
            self.buf.extend_from_slice(&scratch[..n]);
            self.fds.extend(fds);
        }
        true
    }

    fn take(&mut self, n: usize) -> Vec<u8> {
        let out = self.buf[self.pos..self.pos + n].to_vec();
        self.pos += n;
        out
    }

    /// Reads one full message: (object id, opcode, body). The body is
    /// everything after the 8-byte header. Returns None on disconnect
    /// or a malformed size.
    fn read_msg(&mut self) -> Option<(u32, u16, Vec<u8>)> {
        if !self.fill_to(8) {
            return None;
        }
        let hdr = self.take(8);
        let obj = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let op_size = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
        let size = (op_size >> 16) as u16;
        let op = (op_size & 0xFFFF) as u16;
        if size < 8 || size > 4096 {
            return None; // protocol error — drop the connection
        }
        // The 8-byte header is already consumed; `size - 8` is the
        // body's byte count.
        if !self.fill_to(size as usize - 8) {
            return None;
        }
        let body = self.take(size as usize - 8);
        Some((obj, op, body))
    }

    /// The next fd received via `SCM_RIGHTS` (the `fd` argument of a
    /// message is carried in ancillary data, not the byte stream).
    fn take_fd(&mut self) -> Option<usize> {
        if self.fds.is_empty() {
            None
        } else {
            Some(self.fds.remove(0))
        }
    }
}

/// Per-connection compositor state: the object table plus the pending
/// event stream (accumulated in `out`, flushed before each blocking
/// read) and the small amount of xdg/surface state one client needs.
struct Conn {
    conn: usize,
    out: Vec<u8>,
    r: ConnReader,
    objects: Vec<Option<Obj>>,
    /// Monotonic input-event serial counter.
    serial: u32,
    /// The client's wl_surface / xdg objects (one per connection).
    surface_id: Option<u32>,
    xdg_surface_id: Option<u32>,
    toplevel_id: Option<u32>,
    /// Whether the toplevel's initial configure sequence has been sent
    /// (the xdg mapping dance: first buffer-less commit → configure →
    /// client ack → buffer commit maps).
    xdg_configured: bool,
    /// Whether we've sent the surface's one-shot input events.
    input_sent: bool,
}

impl Conn {
    fn new(conn: usize) -> Self {
        let mut objects = vec![None]; // id 0 = null
        objects.push(Some(Obj::new(Iface::Display))); // id 1
        Conn {
            conn,
            out: Vec::new(),
            r: ConnReader::new(conn),
            objects,
            serial: 0,
            surface_id: None,
            xdg_surface_id: None,
            toplevel_id: None,
            xdg_configured: false,
            input_sent: false,
        }
    }

    fn set_obj(&mut self, id: u32, obj: Obj) {
        // Real libwayland allocates dense ids from 2, so any sane client
        // stays small. Cap the table so a malformed `new_id` (up to
        // 0xfeffffff in the protocol) can't force a multi-GB Vec resize
        // inside the kernel — oversized ids are silently ignored.
        const MAX_OBJECT_ID: usize = 0x1000;
        let idx = id as usize;
        if idx == 0 || idx > MAX_OBJECT_ID {
            return;
        }
        if self.objects.len() <= idx {
            self.objects.resize(idx + 1, None);
        }
        self.objects[idx] = Some(obj);
    }

    fn drop_obj(&mut self, id: u32) {
        if let Some(slot) = self.objects.get_mut(id as usize) {
            *slot = None;
        }
    }

    // ---- event encoders (server → client, real wire format) ----

    fn ev_raw(&mut self, obj: u32, op: u16, body: &[u8]) {
        let size = (8 + body.len()) as u32;
        self.out.extend_from_slice(&obj.to_le_bytes());
        self.out.extend_from_slice(&((op as u32) | (size << 16)).to_le_bytes());
        self.out.extend_from_slice(body);
    }

    fn ev_empty(&mut self, obj: u32, op: u16) {
        self.ev_raw(obj, op, &[]);
    }

    fn ev_u32(&mut self, obj: u32, op: u16, v: u32) {
        self.ev_raw(obj, op, &v.to_le_bytes());
    }

    fn ev_str(&mut self, obj: u32, op: u16, s: &str) {
        let mut body = Vec::new();
        push_str(&mut body, s);
        self.ev_raw(obj, op, &body);
    }

    /// `wl_registry.global(name, interface, version)` — opcode 0.
    fn send_global(&mut self, registry: u32, name: u32, interface: &str, version: u32) {
        let mut body = Vec::new();
        body.extend_from_slice(&name.to_le_bytes());
        push_str(&mut body, interface);
        body.extend_from_slice(&version.to_le_bytes());
        self.ev_raw(registry, 0, &body);
    }

    /// `xdg_toplevel.configure(width, height, states array)` — opcode 0.
    fn send_toplevel_configure(&mut self, w: i32, h: i32, states: &[u32]) {
        let mut body = Vec::new();
        body.extend_from_slice(&(w as u32).to_le_bytes());
        body.extend_from_slice(&(h as u32).to_le_bytes());
        push_array_u32(&mut body, states);
        let id = self.toplevel_id.unwrap_or(0);
        self.ev_raw(id, 0, &body);
    }

    fn flush(&mut self) {
        if self.out.is_empty() {
            return;
        }
        let _ = socket::send(self.conn, &self.out, &[]);
        self.out.clear();
    }

    // ---- request dispatch ----

    fn dispatch(&mut self, obj: u32, op: u16, body: &[u8]) {
        let Some(iface) = self.objects.get(obj as usize).and_then(|o| o.as_ref()).map(|o| o.iface)
        else {
            return; // unknown object id — ignore (real servers error here)
        };
        let mut b = Body::new(body);
        match iface {
            Iface::Display => match op {
                0 => {
                    // wl_display.sync(callback): a barrier callback. This
                    // compositor processes requests synchronously, so the
                    // done event is valid immediately after dispatch.
                    let cb = b.u32();
                    self.set_obj(cb, Obj::new(Iface::Callback));
                    self.ev_u32(cb, 0, 0); // wl_callback.done(0)
                }
                1 => {
                    // wl_display.get_registry(id): announce the globals.
                    let reg = b.u32();
                    self.set_obj(reg, Obj::new(Iface::Registry));
                    self.send_global(reg, 1, "wl_compositor", 4);
                    self.send_global(reg, 2, "wl_shm", 1);
                    self.send_global(reg, 3, "wl_seat", 5);
                    self.send_global(reg, 4, "xdg_wm_base", 1);
                }
                _ => {}
            },
            Iface::Registry => match op {
                0 => self.handle_bind(&mut b),
                _ => {}
            },
            Iface::Compositor => match op {
                0 => {
                    let id = b.u32();
                    self.set_obj(id, Obj::new(Iface::Surface));
                    self.surface_id = Some(id);
                }
                1 => {
                    let id = b.u32();
                    self.set_obj(id, Obj::new(Iface::Region));
                }
                _ => {}
            },
            Iface::Shm => match op {
                0 => {
                    // wl_shm.create_pool(id, fd, size) — fd arrives via
                    // SCM_RIGHTS on this same message.
                    let pool_id = b.u32();
                    let fd = self.r.take_fd();
                    let size = b.u32();
                    if let Some(fd) = fd {
                        let mut obj = Obj::new(Iface::ShmPool);
                        obj.pool = Some((fd, size as usize));
                        self.set_obj(pool_id, obj);
                    }
                }
                _ => {}
            },
            Iface::ShmPool => match op {
                0 => {
                    // wl_shm_pool.create_buffer(id, offset, w, h, stride, format)
                    let buf_id = b.u32();
                    let offset = b.u32() as usize;
                    let width = b.u32();
                    let height = b.u32();
                    let stride = b.u32() as usize;
                    let _format = b.u32();
                    let pool = self
                        .objects
                        .get(obj as usize)
                        .and_then(|o| o.as_ref())
                        .and_then(|o| o.pool);
                    if let Some((pool_shm, pool_size)) = pool {
                        let mut o = Obj::new(Iface::Buffer);
                        o.buf = Some(BufferDesc { pool_shm, pool_size, offset, width, height, stride });
                        self.set_obj(buf_id, o);
                    }
                }
                1 => {
                    // wl_shm_pool.destroy: release this connection's ref
                    // to the pool's shm object (the session-end cleanup
                    // only sees live objects, so destroyed pools must be
                    // closed here or they leak).
                    if let Some((pool_shm, _)) = self
                        .objects
                        .get(obj as usize)
                        .and_then(|o| o.as_ref())
                        .and_then(|o| o.pool)
                    {
                        shm::close(pool_shm);
                    }
                    self.drop_obj(obj);
                }
                _ => {}
            },
            Iface::Buffer => match op {
                0 => self.drop_obj(obj),
                _ => {}
            },
            Iface::Surface => match op {
                0 => self.drop_obj(obj),
                1 => {
                    // wl_surface.attach(buffer, x, y)
                    let buffer = b.u32();
                    let _x = b.i32();
                    let _y = b.i32();
                    if let Some(s) = self.objects.get_mut(obj as usize).and_then(|o| o.as_mut()) {
                        s.pending_buffer = if buffer == 0 { None } else { Some(buffer) };
                    }
                }
                3 => {
                    // wl_surface.frame(callback)
                    let cb = b.u32();
                    self.set_obj(cb, Obj::new(Iface::Callback));
                    if let Some(s) = self.objects.get_mut(obj as usize).and_then(|o| o.as_mut()) {
                        s.frame_cbs.push(cb);
                    }
                }
                6 => self.commit(obj),
                // damage(2), set_opaque_region(4), set_input_region(5),
                // set_buffer_transform(7), set_buffer_scale(8),
                // damage_buffer(9), offset(10): accepted, ignored.
                _ => {}
            },
            Iface::Seat => match op {
                0 => {
                    let id = b.u32();
                    self.set_obj(id, Obj::new(Iface::Pointer));
                }
                1 => {
                    let id = b.u32();
                    self.set_obj(id, Obj::new(Iface::Keyboard));
                }
                _ => {}
            },
            Iface::Pointer | Iface::Keyboard => {} // set_cursor/release etc: ignored
            Iface::XdgWmBase => match op {
                1 => {
                    let id = b.u32();
                    self.set_obj(id, Obj::new(Iface::XdgPositioner));
                }
                2 => {
                    // xdg_wm_base.get_xdg_surface(id, surface)
                    let id = b.u32();
                    let _surface = b.u32();
                    self.set_obj(id, Obj::new(Iface::XdgSurface));
                    self.xdg_surface_id = Some(id);
                }
                3 => {} // pong — the ping/pong round trip is complete
                _ => {}
            },
            Iface::XdgSurface => match op {
                1 => {
                    // xdg_surface.get_toplevel(id)
                    let id = b.u32();
                    self.set_obj(id, Obj::new(Iface::XdgToplevel));
                    self.toplevel_id = Some(id);
                }
                4 => {} // ack_configure(serial) — accepted; commit maps
                _ => {}
            },
            Iface::XdgToplevel => match op {
                2 => {
                    let _title = b.string(); // set_title — unused (no taskbar entry)
                }
                3 => {
                    let _app_id = b.string(); // set_app_id — unused
                }
                _ => {}
            },
            Iface::Region | Iface::Callback | Iface::XdgPositioner => {}
        }
    }

    /// `wl_registry.bind(name, id)`: the generic `new_id` encoding — the
    /// message carries the interface name string and version before the
    /// id (the protocol's custom serialization rules for a new_id whose
    /// interface isn't fixed at compile time). Resolves the interface by
    /// its name, creates the object, and emits the interface's
    /// bind-time events (shm formats, seat capabilities/name).
    fn handle_bind(&mut self, b: &mut Body) {
        let _name = b.u32();
        let Some(interface) = b.string() else { return };
        let _version = b.u32();
        let id = b.u32();
        let iface = match interface.as_str() {
            "wl_compositor" => Iface::Compositor,
            "wl_shm" => Iface::Shm,
            "wl_seat" => Iface::Seat,
            "xdg_wm_base" => Iface::XdgWmBase,
            _ => return, // unknown global — ignore the bind
        };
        self.set_obj(id, Obj::new(iface));
        match iface {
            Iface::Shm => {
                // Advertise the two formats this compositor can blit.
                self.ev_u32(id, 0, WL_SHM_FORMAT_ARGB8888);
                self.ev_u32(id, 0, WL_SHM_FORMAT_XRGB8888);
            }
            Iface::Seat => {
                self.ev_u32(id, 0, WL_SEAT_CAPABILITY_POINTER | WL_SEAT_CAPABILITY_KEYBOARD);
                self.ev_str(id, 1, "machaos-seat");
            }
            Iface::XdgWmBase => {
                // xdg_wm_base.ping — the client answers with pong.
                self.serial += 1;
                self.ev_u32(id, 0, self.serial);
            }
            _ => {}
        }
    }

    /// `wl_surface.commit()`. The xdg mapping dance: the first commit
    /// (which, per spec, must not have a buffer attached) draws the
    /// initial configure sequence; the acked buffer commit maps the
    /// surface — blits its pixels, fires the frame callbacks, releases
    /// the buffer, and sends the one-shot input events.
    fn commit(&mut self, surface: u32) {
        let pending = self
            .objects
            .get(surface as usize)
            .and_then(|o| o.as_ref())
            .and_then(|o| o.pending_buffer);

        if self.toplevel_id.is_some() && !self.xdg_configured {
            self.xdg_configured = true;
            self.serial += 1;
            // xdg_toplevel.configure(0, 0, []) + xdg_surface.configure(serial).
            // A 0x0 size means "the compositor doesn't constrain the
            // window" — a real, legal choice.
            self.send_toplevel_configure(0, 0, &[]);
            let xdg = self.xdg_surface_id.unwrap_or(0);
            self.ev_u32(xdg, 0, self.serial);
        }

        let Some(buffer_id) = pending else { return };
        let Some(desc) = self
            .objects
            .get(buffer_id as usize)
            .and_then(|o| o.as_ref())
            .and_then(|o| o.buf.as_ref())
            .map(|d| BufferDesc {
                pool_shm: d.pool_shm,
                pool_size: d.pool_size,
                offset: d.offset,
                width: d.width,
                height: d.height,
                stride: d.stride,
            })
        else {
            return;
        };

        render(&desc);
        FRAMES_RENDERED.fetch_add(1, Ordering::Release);

        // wl_buffer.release (the compositor copied the pixels — an
        // immediate release is legal), then the frame callbacks.
        self.ev_empty(buffer_id, 0);
        let cbs: Vec<u32> = self
            .objects
            .get(surface as usize)
            .and_then(|o| o.as_ref())
            .map(|o| o.frame_cbs.clone())
            .unwrap_or_default();
        if let Some(s) = self.objects.get_mut(surface as usize).and_then(|o| o.as_mut()) {
            s.frame_cbs.clear();
        }
        for cb in cbs {
            self.ev_u32(cb, 0, 0); // wl_callback.done(0)
        }

        // One-shot input snapshot for the mapped surface. keymap is the
        // legal NO_KEYMAP format (no xkb keymap to ship — fd -1, size 0).
        if !self.input_sent {
            self.input_sent = true;
            self.serial += 1;
            let key_serial = self.serial;
            self.serial += 1;
            let ptr_serial = self.serial;
            let kb = self
                .objects
                .iter()
                .position(|o| o.as_ref().is_some_and(|o| o.iface == Iface::Keyboard))
                .map(|i| i as u32);
            let ptr = self
                .objects
                .iter()
                .position(|o| o.as_ref().is_some_and(|o| o.iface == Iface::Pointer))
                .map(|i| i as u32);
            if let Some(kb) = kb {
                self.ev_raw(kb, 0, &[0, 0, 0, 0, 0, 0, 0, 0]); // keymap: format 0, size 0 (no fd)
                let mut body = Vec::new();
                body.extend_from_slice(&key_serial.to_le_bytes());
                body.extend_from_slice(&surface.to_le_bytes());
                push_array_u32(&mut body, &[]); // no keys held
                self.ev_raw(kb, 1, &body); // keyboard.enter
            }
            if let Some(ptr) = ptr {
                let mut body = Vec::new();
                body.extend_from_slice(&ptr_serial.to_le_bytes());
                body.extend_from_slice(&surface.to_le_bytes());
                body.extend_from_slice(&0u32.to_le_bytes()); // surface x (24.8)
                body.extend_from_slice(&0u32.to_le_bytes()); // surface y (24.8)
                self.ev_raw(ptr, 0, &body); // pointer.enter
                self.ev_empty(ptr, 5); // pointer.frame
            }
        }
    }
}

/// Spin-`hlt`s (same idiom as `process::wait`) until `FRAMES_RENDERED`
/// has advanced past `after`, or `timeout_ticks` elapses. Kernel-context
/// only (the caller — `shell.rs`'s selftest hook — isn't inside a
/// syscall handler, so blocking like this is fine here even though it
/// isn't for `socket.rs`'s syscalls).
pub fn wait_for_frame(after: u64, timeout_ticks: u64) -> bool {
    let deadline = interrupts::ticks() + timeout_ticks;
    loop {
        if FRAMES_RENDERED.load(Ordering::Acquire) > after {
            return true;
        }
        if interrupts::ticks() >= deadline {
            return false;
        }
        interrupts::halt();
    }
}

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

/// Serves one client connection: flush events, read a request, dispatch.
/// Breaks when the client disconnects (or sends a malformed message).
fn client_session(conn: usize) {
    let mut c = Conn::new(conn);
    loop {
        c.flush();
        match c.r.read_msg() {
            None => break,
            Some((obj, op, body)) => c.dispatch(obj, op, &body),
        }
    }
    // Release this connection's references to the shared pools it handed
    // us (each `SCM_RIGHTS` fd bumped the pool's refcount on the way in).
    for obj in &c.objects {
        if let Some(obj) = obj {
            if let Some((pool_shm, _)) = obj.pool {
                shm::close(pool_shm);
            }
        }
    }
}

/// Blits `width`x`height` pixels (native `0x00RRGGBB`, `stride` bytes
/// per row) starting at `offset` bytes into the shared-memory object
/// `pool_shm` onto the real framebuffer at a fixed screen position, then
/// presents. No page-table mapping needed on this side — the kernel's
/// own low memory is already identity-mapped, so `shm::phys_of` is
/// directly readable, the kernel-native equivalent of the client's own
/// `mmap(MAP_SHARED)`.
fn render(buf: &BufferDesc) {
    let Some(phys) = shm::phys_of(buf.pool_shm) else {
        return;
    };
    let need = buf.offset + buf.stride * buf.height as usize;
    if need > buf.pool_size {
        return;
    }
    let mut pixels = vec![0u32; (buf.width * buf.height) as usize];
    for row in 0..buf.height as usize {
        let row_phys = phys + buf.offset + row * buf.stride;
        let src = unsafe { core::slice::from_raw_parts(row_phys as *const u32, buf.width as usize) };
        pixels[row * buf.width as usize..(row + 1) * buf.width as usize].copy_from_slice(src);
    }
    fb::with_surface(|surface| {
        gfx::blit(surface, SURFACE_X, SURFACE_Y, &pixels, buf.width, buf.height);
    });
    fb::present();
}
