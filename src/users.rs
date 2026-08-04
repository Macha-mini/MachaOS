//! User concept: MachaOS has a single built-in user whose home
//! directory anchors the desktop, documents, and per-user data, in the
//! spirit of Windows' `C:\Users\<name>`. The filesystem layout is:
//!
//! ```text
//! /            system root
//!   bin/       executable programs (launcher scans this)
//!   system/    OS data (wallpaper, readme)
//!   users/
//!     macha/   home: Desktop, Documents, Downloads, Pictures, Music
//! ```

use alloc::format;
use alloc::string::String;

/// The only user account. A multi-user login screen is future work;
/// the layout already supports adding more entries under `/users`.
pub const USER: &str = "macha";

pub const USERS_DIR: &str = "/users";

/// Home directory of the current user, e.g. `/users/macha`.
pub fn home() -> String {
    home_of(USER)
}

/// Home directory of a named user, e.g. `/users/macha`.
pub fn home_of(user: &str) -> String {
    format!("{}/{}", USERS_DIR, user)
}

/// The user's desktop folder.
pub fn desktop() -> String {
    format!("{}/Desktop", home())
}

/// The user's documents folder.
pub fn documents() -> String {
    format!("{}/Documents", home())
}
