//! Minimal VFS layer over `fat.rs`. `fat.rs` only knows whole-file
//! `read_file`/`write_file` (no partial I/O), so an open `FileHandle` here
//! is a private in-memory copy of the file's contents plus a cursor:
//! `read`/`write`/`seek` all operate on that copy, and the copy is written
//! back through `fat::write_file` on `close` (or drop) if it was modified.
//! This is the shared file-access layer for the per-process fd table
//! (`process::Process::fds`) that the future Linux ABI syscalls
//! (`openat`/`read`/`write`/`lseek`/`close`/`fstat`) will dispatch to.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::fat::{self, FatError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VfsError {
    NoDisk,
    NotFound,
    NotDir,
    IsDir,
    AlreadyExists,
    NotEmpty,
    InvalidName,
    OutOfSpace,
    Corrupt,
    Io,
}

impl From<FatError> for VfsError {
    fn from(e: FatError) -> Self {
        match e {
            FatError::NoDisk => VfsError::NoDisk,
            FatError::NotFound => VfsError::NotFound,
            FatError::NotDir => VfsError::NotDir,
            FatError::AlreadyExists => VfsError::AlreadyExists,
            FatError::NotEmpty => VfsError::NotEmpty,
            FatError::InvalidName => VfsError::InvalidName,
            FatError::OutOfSpace => VfsError::OutOfSpace,
            FatError::Corrupt => VfsError::Corrupt,
            FatError::InvalidVolume | FatError::Io(_) => VfsError::Io,
        }
    }
}

#[derive(Clone, Copy)]
pub enum SeekFrom {
    Start(u64),
    Current(i64),
    End(i64),
}

#[derive(Clone, Copy, Default)]
pub struct Stat {
    pub size: u64,
    pub is_dir: bool,
    /// Synthetic inode number — this filesystem has no real inode
    /// concept, but a real `ld.so` needs *some* stable, distinct
    /// per-path value here: it dedups a shared library it's about to
    /// load against every already-loaded map by comparing `(st_dev,
    /// st_ino)`, and every file reporting the same (previously always
    /// zero) pair made every open file look like the same one —
    /// `libc.so.6` collided with the main executable's own already-
    /// loaded map, so `ld.so` reused *that* instead of ever mapping
    /// `libc.so.6`'s real segments. A path hash is enough: it only needs
    /// to be stable and distinct per path within one boot, not globally
    /// unique or persistent, since nothing here ever compares it against
    /// a previous session's value.
    pub ino: u64,
}

/// FNV-1a over the normalized path — see `Stat::ino`'s doc comment for
/// why this exists. Never returns 0 (a few real-world callers, and this
/// kernel's own `st_ino == 0` bug this replaces, treat that as "no
/// inode"/invalid).
fn path_hash(path: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in path.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    if hash == 0 { 1 } else { hash }
}

pub const O_WRONLY: u32 = 1 << 0;
pub const O_RDWR: u32 = 1 << 1;
pub const O_CREAT: u32 = 1 << 2;
pub const O_TRUNC: u32 = 1 << 3;
pub const O_APPEND: u32 = 1 << 4;

/// One open file: a full in-memory copy of its FAT32 contents (or, for a
/// directory, an empty placeholder — directories aren't readable through
/// this layer yet) plus a byte cursor.
pub struct FileHandle {
    path: String,
    data: Vec<u8>,
    cursor: usize,
    dirty: bool,
    writable: bool,
    is_dir: bool,
}

impl FileHandle {
    /// Opens `path`. `O_CREAT` creates an absent file empty; `O_TRUNC`
    /// discards an existing file's content on open.
    pub fn open(path: &str, flags: u32) -> Result<FileHandle, VfsError> {
        if fat::is_dir(path).unwrap_or(false) {
            return Ok(FileHandle {
                path: path.to_string(),
                data: Vec::new(),
                cursor: 0,
                dirty: false,
                writable: false,
                is_dir: true,
            });
        }
        let writable = flags & (O_WRONLY | O_RDWR) != 0;
        let data = match fat::read_file(path) {
            Ok(bytes) => {
                if flags & O_TRUNC != 0 {
                    Vec::new()
                } else {
                    bytes
                }
            }
            Err(FatError::NotFound) if flags & O_CREAT != 0 => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let cursor = if flags & O_APPEND != 0 { data.len() } else { 0 };
        Ok(FileHandle {
            path: path.to_string(),
            data,
            cursor,
            dirty: false,
            writable,
            is_dir: false,
        })
    }

    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    /// Deep copy for `fork`: same file, same position, independent
    /// handle (a real fork shares the open-file description, but the
    /// VFS layer keeps the whole file in memory so a copy is equivalent
    /// for every caller this kernel runs).
    pub fn dup(&self) -> FileHandle {
        FileHandle {
            path: self.path.clone(),
            data: self.data.clone(),
            cursor: self.cursor,
            dirty: self.dirty,
            writable: self.writable,
            is_dir: self.is_dir,
        }
    }

    /// Copies up to `buf.len()` bytes starting at the cursor, advancing it.
    /// Returns 0 at EOF or for a directory handle.
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        if self.is_dir {
            return 0;
        }
        let avail = self.data.len().saturating_sub(self.cursor);
        let n = avail.min(buf.len());
        buf[..n].copy_from_slice(&self.data[self.cursor..self.cursor + n]);
        self.cursor += n;
        n
    }

    /// Writes `buf` at the cursor, growing the in-memory copy (zero-filling
    /// any gap) as needed, and marks the handle dirty so `flush`/`close`
    /// writes it back.
    pub fn write(&mut self, buf: &[u8]) -> Result<usize, VfsError> {
        if self.is_dir || !self.writable {
            return Err(VfsError::IsDir);
        }
        let end = self.cursor + buf.len();
        if end > self.data.len() {
            self.data.resize(end, 0);
        }
        self.data[self.cursor..end].copy_from_slice(buf);
        self.cursor = end;
        self.dirty = true;
        Ok(buf.len())
    }

    pub fn seek(&mut self, pos: SeekFrom) -> u64 {
        let base = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(p) => self.cursor as i64 + p,
            SeekFrom::End(p) => self.data.len() as i64 + p,
        };
        self.cursor = base.max(0) as usize;
        self.cursor as u64
    }

    pub fn stat(&self) -> Stat {
        Stat {
            size: self.data.len() as u64,
            is_dir: self.is_dir,
            ino: path_hash(&self.path),
        }
    }

    /// Writes the in-memory copy back to disk if it was modified.
    pub fn flush(&mut self) -> Result<(), VfsError> {
        if self.dirty {
            fat::write_file(&self.path, &self.data)?;
            self.dirty = false;
        }
        Ok(())
    }
}

impl Drop for FileHandle {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}
