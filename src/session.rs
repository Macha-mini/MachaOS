//! Desktop session persistence: the set of open windows (app, position,
//! cell size, minimized/maximized state) and the "remembered" geometry of
//! previously closed apps is written to `/system/desktop.session` on the
//! mounted FAT32 volume whenever it changes and restored at boot, so the
//! desktop comes back the way the user left it.
//!
//! The file is a plain line-based text format, one entry per line
//! (`#`-prefixed lines are comments):
//!
//! ```text
//! # MachaOS desktop session
//! window terminal 100 80 80 24 0 0
//! window notepad 300 200 60 12 1 0
//! remembered calculator 140 100 0 0 0
//! ```
//!
//! Fields for a `window` entry: `app x y cols rows minimized maximized`;
//! for a `remembered` entry: `app x y cols rows maximized` (booleans are
//! `0`/`1`, and `cols`/`rows` are `0` for fixed-pixel apps that have no
//! console cell grid). Parsing is lenient: unknown lines are skipped and
//! a malformed file just restores an empty session.

use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

use crate::fat;

/// One restored window or remembered geometry entry.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionWindow {
    /// App name as written to the session file (see `wm::AppId`).
    pub app: String,
    pub x: i32,
    pub y: i32,
    pub cols: usize,
    pub rows: usize,
    pub minimized: bool,
    pub maximized: bool,
}

/// The complete persisted desktop session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionData {
    /// Windows that were open when the session was saved.
    pub windows: Vec<SessionWindow>,
    /// "Where was this app last left" entries for closed apps.
    pub remembered: Vec<SessionWindow>,
}

const SESSION_PATH: &str = "/system/desktop.session";

/// Serializes a session to the on-disk text format.
pub fn serialize(data: &SessionData) -> String {
    let mut text = String::from("# MachaOS desktop session\n");
    for w in &data.windows {
        let _ = core::fmt::write(
            &mut text,
            format_args!(
                "window {} {} {} {} {} {} {}\n",
                w.app, w.x, w.y, w.cols, w.rows, w.minimized as u8, w.maximized as u8
            ),
        );
    }
    for w in &data.remembered {
        let _ = core::fmt::write(
            &mut text,
            format_args!(
                "remembered {} {} {} {} {} {}\n",
                w.app, w.x, w.y, w.cols, w.rows, w.maximized as u8
            ),
        );
    }
    text
}

/// Parses the on-disk text format back into a session. Malformed or
/// unknown lines are skipped; a completely unparseable file yields an
/// empty session rather than an error.
pub fn parse(text: &str) -> SessionData {
    let mut data = SessionData::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(kind) = parts.next() else { continue };
        match kind {
            "window" => {
                let Some(entry) = parse_entry(&mut parts, true) else {
                    continue;
                };
                data.windows.push(entry);
            }
            "remembered" => {
                let Some(entry) = parse_entry(&mut parts, false) else {
                    continue;
                };
                data.remembered.push(entry);
            }
            _ => continue,
        }
    }
    data
}

fn parse_entry(parts: &mut core::str::SplitWhitespace, window: bool) -> Option<SessionWindow> {
    let app = parts.next()?.to_string();
    let x = parts.next()?.parse().ok()?;
    let y = parts.next()?.parse().ok()?;
    let cols = parts.next()?.parse().ok()?;
    let rows = parts.next()?.parse().ok()?;
    if window {
        // window <app> <x> <y> <cols> <rows> <minimized> <maximized>
        let minimized = parse_bool(parts.next()?);
        let maximized = parse_bool(parts.next()?);
        Some(SessionWindow {
            app,
            x,
            y,
            cols,
            rows,
            minimized,
            maximized,
        })
    } else {
        // remembered <app> <x> <y> <cols> <rows> <maximized>
        let maximized = parse_bool(parts.next()?);
        Some(SessionWindow {
            app,
            x,
            y,
            cols,
            rows,
            minimized: false,
            maximized,
        })
    }
}

fn parse_bool(value: &str) -> bool {
    matches!(value, "1" | "true")
}

/// Saves a session to `/system/desktop.session`. Silently fails when no
/// volume is mounted or the write fails (the desktop just keeps its
/// in-memory session in that case).
pub fn save_to_disk(data: &SessionData) -> bool {
    let text = serialize(data);
    fat::write_file(SESSION_PATH, text.as_bytes()).is_ok()
}

/// Loads the persisted session from disk; an empty (or missing) file
/// restores an empty session.
pub fn load_from_disk() -> SessionData {
    match fat::read_file(SESSION_PATH) {
        Ok(bytes) => parse(core::str::from_utf8(&bytes).unwrap_or("")),
        Err(_) => SessionData::default(),
    }
}
