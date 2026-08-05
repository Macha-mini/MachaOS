//! Trash can: "deleting" a file moves it to `/system/Trash/` instead of
//! physically erasing it, and records the original path in a small text
//! sidecar so the entry can be restored later. Emptying the trash
//! physically deletes what's inside.
//!
//! Layout on the mounted FAT32 volume:
//!
//! ```text
//! /system/Trash/
//!   <name>          the trashed file or empty directory
//!   trash.meta      "original-path\n" per entry, in the same order as
//!                   the files listed by `list()`
//! ```
//!
//! Names are kept as-is when they land in the trash; a collision with
//! an existing trashed entry is resolved by appending a numeric suffix
//! (`name (2).txt`), matching the explorer's New-Folder dedup style.
//! The meta file is the only place the original location lives, so it
//! must be written before the file is moved and read back to restore.

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

use crate::fat;

pub const TRASH_PATH: &str = "/system/Trash";
const META_PATH: &str = "/system/Trash/trash.meta";

/// Ensures the trash directory exists, creating it (and its parent
/// `/system`) if needed. Returns false only when the volume is
/// unavailable.
pub fn ensure_trash() -> bool {
    if fat::is_dir(TRASH_PATH).unwrap_or(false) {
        return true;
    }
    // `/system` usually exists (settings/session live there), but be
    // safe when it doesn't.
    if !fat::is_dir("/system").unwrap_or(false) {
        let _ = fat::make_dir("/system");
    }
    fat::make_dir(TRASH_PATH).is_ok()
}

/// Moves `path` (a file or empty directory) into the trash, recording
/// its original location. Returns the name it now has in the trash, or
/// an error message when the volume/trash is unavailable.
pub fn trash_file(path: &str) -> Result<String, &'static str> {
    if !ensure_trash() {
        return Err("no volume or cannot create trash");
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.is_empty() {
        return Err("invalid path");
    }
    // Deduplicate against what's already in the trash.
    let existing = list_names();
    let mut target_name = name.to_string();
    let mut suffix = 2;
    while existing.iter().any(|n| n == &target_name) {
        let (stem, ext) = split_ext(name);
        target_name = if ext.is_empty() {
            format!("{} ({})", stem, suffix)
        } else {
            format!("{} ({}).{}", stem, suffix, ext)
        };
        suffix += 1;
    }

    // Record the original path *before* moving (the meta file is the
    // only record of it).
    let mut meta = read_meta();
    meta.push(path.to_string());
    if fat::write_file(META_PATH, meta.join("\n").as_bytes()).is_err() {
        return Err("cannot write trash meta");
    }

    let target = format!("{}/{}", TRASH_PATH, target_name);
    match fat::move_file(path, &target) {
        Ok(()) => Ok(target_name),
        Err(_) => {
            // Roll the meta entry back so it doesn't point at a file
            // that was never moved.
            meta.pop();
            let _ = fat::write_file(META_PATH, meta.join("\n").as_bytes());
            Err("move into trash failed")
        }
    }
}

/// Restores the `index`-th entry (as listed by `list`) to its original
/// location. Returns the restored path, or an error message.
pub fn restore(index: usize) -> Result<String, &'static str> {
    let names = list_names();
    let meta = read_meta();
    let Some(name) = names.get(index) else {
        return Err("no such trash entry");
    };
    let original = match meta.get(index) {
        Some(original) => original.clone(),
        None => return Err("trash meta is missing this entry"),
    };
    // Re-create the original directory if it no longer exists.
    if let Some(parent) = original.rsplit_once('/').map(|(p, _)| p) {
        if !parent.is_empty() && !fat::is_dir(parent).unwrap_or(false) {
            let _ = fat::make_dir(parent);
        }
    }
    let in_trash = format!("{}/{}", TRASH_PATH, name);
    if fat::move_file(&in_trash, &original).is_err() {
        return Err("restore failed");
    }
    // Drop the meta line for this entry.
    let mut new_meta = meta;
    new_meta.remove(index);
    let _ = fat::write_file(META_PATH, new_meta.join("\n").as_bytes());
    Ok(original.clone())
}

/// Physically deletes everything in the trash.
pub fn empty_trash() -> Result<(), &'static str> {
    if !fat::is_dir(TRASH_PATH).unwrap_or(false) {
        return Ok(());
    }
    let names = list_names();
    for name in &names {
        let _ = fat::remove(&format!("{}/{}", TRASH_PATH, name));
    }
    let _ = fat::remove(META_PATH);
    Ok(())
}

/// Returns the names of the entries currently in the trash, in the
/// order their original paths appear in the meta file (when the meta
/// file is missing or shorter, the remaining names are still listed).
pub fn list_names() -> Vec<String> {
    let Ok(entries) = fat::list_dir(TRASH_PATH) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter(|e| e.name != "trash.meta")
        .map(|e| e.name)
        .collect()
}

/// Returns the original paths of the trashed entries, one per entry in
/// `list_names` order.
pub fn list_original_paths() -> Vec<String> {
    read_meta()
}

fn read_meta() -> Vec<String> {
    match fat::read_file(META_PATH) {
        Ok(data) => core::str::from_utf8(&data)
            .unwrap_or("")
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Splits a file name into (stem, extension-without-dot); `("name", "")`
/// when there is no dot. A leading dot (hidden files) is part of the
/// stem.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    }
}
