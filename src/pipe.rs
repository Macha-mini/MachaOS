//! POSIX pipes (`pipe2`): a fixed-size in-memory ring buffer with
//! independent read/write ends, shared by reference count across
//! `fork`. The ends are counted separately (`readers`/`writers`) so a
//! reader sees EOF (read returns 0) only once every write end is
//! closed, and a write to a pipe whose readers are all gone returns 0
//! (the `EPIPE`/`SIGPIPE` situation, reported as a short write here).
//!
//! Blocking: a read on an empty pipe (with writers alive) and a write
//! to a full pipe (with readers alive) block the calling task on the
//! pipe's wait-queue key, the same `yield_blocked`/`wake_all`
//! machinery FUTEX_WAIT uses — safe because every access runs in
//! syscall context with IF cleared (single CPU, no preemption between
//! the state check and the block registration). The key is tagged with
//! bit 63 so it can never collide with a futex key (`cr3 ^ uaddr`).
//!
//! All state is reachable only from syscall handlers (never from the
//! kernel's own task context), so — like the futex wait-queue in
//! `task.rs` — it lives in a plain `static mut` with no lock; the
//! kernel's syscall serialization (IF=0, single CPU) is the lock.

use alloc::vec::Vec;

const PIPE_BUF_SIZE: usize = 65536;
const PIPE_WAKE_TAG: u64 = 1 << 63;

pub struct Pipe {
    buf: Vec<u8>,
    /// Ring-buffer positions; `wr == rd` means empty, one slot is kept
    /// unused to tell "full" apart from "empty".
    rd: usize,
    wr: usize,
    /// Number of open read ends / write ends (one each at creation,
    /// +1 per `fork`/`dup` of that end, -1 per close).
    readers: usize,
    writers: usize,
}

static mut PIPES: Vec<Option<Pipe>> = Vec::new();

fn table() -> &'static mut Vec<Option<Pipe>> {
    unsafe { &mut *core::ptr::addr_of_mut!(PIPES) }
}

fn pipe_key(id: usize) -> u64 {
    PIPE_WAKE_TAG | id as u64
}

/// Creates a pipe; returns its id. The caller installs the read and
/// write ends as two fds.
pub fn create() -> usize {
    let t = table();
    t.push(Some(Pipe {
        buf: alloc::vec![0u8; PIPE_BUF_SIZE],
        rd: 0,
        wr: 0,
        readers: 1,
        writers: 1,
    }));
    t.len() - 1
}

/// One more reference to `is_read`'s end (fork/dup of that fd).
pub fn dup_end(id: usize, is_read: bool) {
    if let Some(Some(p)) = table().get_mut(id) {
        if is_read {
            p.readers += 1;
        } else {
            p.writers += 1;
        }
    }
}

/// Drops one reference to `is_read`'s end; frees the pipe once both
/// ends are gone.
pub fn close_end(id: usize, is_read: bool) {
    let t = table();
    if let Some(Some(p)) = t.get_mut(id) {
        if is_read {
            p.readers = p.readers.saturating_sub(1);
        } else {
            p.writers = p.writers.saturating_sub(1);
        }
        if p.readers == 0 && p.writers == 0 {
            t[id] = None;
        }
    }
}

/// The number of bytes currently buffered (for diagnostics).
pub fn buffered(id: usize) -> usize {
    table()
        .get(id)
        .and_then(|s| s.as_ref())
        .map(|p| p.wr.wrapping_sub(p.rd) % PIPE_BUF_SIZE)
        .unwrap_or(0)
}

/// True if a read on the pipe would make progress (poll POLLIN): data is
/// buffered, or the pipe is empty with no writers left (EOF). A vanished
/// pipe reads as EOF too.
pub fn read_ready(id: usize) -> bool {
    match table().get(id).and_then(|s| s.as_ref()) {
        None => true,
        Some(p) => p.rd != p.wr || p.writers == 0,
    }
}

/// True if a write to the pipe would make progress (poll POLLOUT): the
/// buffer isn't full.
pub fn write_ready(id: usize) -> bool {
    table()
        .get(id)
        .and_then(|s| s.as_ref())
        .map(|p| p.rd.wrapping_sub(p.wr).wrapping_sub(1) % PIPE_BUF_SIZE != 0)
        .unwrap_or(false)
}

/// True if the write end has no readers left (poll POLLERR): the writer
/// would get EPIPE/SIGPIPE.
pub fn no_readers(id: usize) -> bool {
    table()
        .get(id)
        .and_then(|s| s.as_ref())
        .map(|p| p.readers == 0)
        .unwrap_or(true)
}

/// Reads up to `out.len()` bytes from the pipe. Blocks while the pipe
/// is empty and a write end is still open; returns 0 for EOF (empty,
/// no writers) or a vanished pipe. Wakes any writer blocked on a full
/// pipe once space is freed.
pub fn read(id: usize, out: &mut [u8]) -> usize {
    loop {
        let (avail, eof) = match table().get(id).and_then(|s| s.as_ref()) {
            None => return 0,
            Some(p) => {
                if p.rd == p.wr && p.writers == 0 {
                    (0, true)
                } else {
                    (p.wr.wrapping_sub(p.rd) % PIPE_BUF_SIZE, false)
                }
            }
        };
        if avail == 0 {
            if eof {
                return 0;
            }
            crate::task::yield_blocked(pipe_key(id));
            continue;
        }
        let n = avail.min(out.len());
        let p = table()[id].as_mut().unwrap();
        for i in 0..n {
            out[i] = p.buf[(p.rd + i) % PIPE_BUF_SIZE];
        }
        p.rd = (p.rd + n) % PIPE_BUF_SIZE;
        crate::task::wake_all(pipe_key(id), usize::MAX);
        return n;
    }
}

/// Appends `data` to the pipe, blocking while full and a read end is
/// still open. Returns the number of bytes written; 0 if every read
/// end is closed (the pipe is gone or dead) — the caller reports that
/// as `EPIPE`. Wakes any reader blocked on an empty pipe.
pub fn write(id: usize, data: &[u8]) -> usize {
    loop {
        let (space, dead) = match table().get(id).and_then(|s| s.as_ref()) {
            None => return 0,
            Some(p) => {
                if p.readers == 0 {
                    (0, true)
                } else {
                    let used = p.wr.wrapping_sub(p.rd) % PIPE_BUF_SIZE;
                    (PIPE_BUF_SIZE - 1 - used, false)
                }
            }
        };
        if dead {
            return 0;
        }
        if space == 0 {
            crate::task::yield_blocked(pipe_key(id));
            continue;
        }
        let n = space.min(data.len());
        let p = table()[id].as_mut().unwrap();
        for (i, &b) in data[..n].iter().enumerate() {
            p.buf[(p.wr + i) % PIPE_BUF_SIZE] = b;
        }
        p.wr = (p.wr + n) % PIPE_BUF_SIZE;
        crate::task::wake_all(pipe_key(id), usize::MAX);
        return n;
    }
}
