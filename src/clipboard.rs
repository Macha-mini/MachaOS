//! System clipboard: a small global store that any app can write text
//! (or a raw RGBA image) into and read back from, shared across the
//! desktop (Notepad/editor, Terminal, and Paint).
//!
//! The clipboard holds whichever of the two payload types was written
//! last; pasting the "wrong" kind for an app is a no-op rather than a
//! conversion. It lives in kernel memory only — nothing is written to
//! disk, so it does not survive a reboot.

use alloc::string::String;
use alloc::vec::Vec;

use crate::sync::SpinLock;

/// The stored payload: either text or a raw `RGBA8888` image with its
/// width and height.
pub enum ClipboardData {
    Text(String),
    Image { width: u32, height: u32, pixels: Vec<u32> },
}

static CLIPBOARD: SpinLock<Option<ClipboardData>> = SpinLock::new(None);

/// Replaces the clipboard contents with `text`.
pub fn copy_text(text: String) {
    *CLIPBOARD.lock() = Some(ClipboardData::Text(text));
}

/// Returns a copy of the clipboard when it currently holds text.
pub fn paste_text() -> Option<String> {
    match CLIPBOARD.lock().as_ref()? {
        ClipboardData::Text(text) => Some(text.clone()),
        ClipboardData::Image { .. } => None,
    }
}

/// Replaces the clipboard contents with a raw RGBA image.
pub fn copy_image(width: u32, height: u32, pixels: Vec<u32>) {
    *CLIPBOARD.lock() = Some(ClipboardData::Image {
        width,
        height,
        pixels,
    });
}

/// Returns a copy of the clipboard when it currently holds an image.
pub fn paste_image() -> Option<(u32, u32, Vec<u32>)> {
    match CLIPBOARD.lock().as_ref()? {
        ClipboardData::Image {
            width,
            height,
            pixels,
        } => Some((*width, *height, pixels.clone())),
        ClipboardData::Text(_) => None,
    }
}

/// True when the clipboard holds text (so a paste key can beep or
/// ignore the key rather than silently doing nothing).
pub fn has_text() -> bool {
    matches!(CLIPBOARD.lock().as_ref(), Some(ClipboardData::Text(_)))
}
