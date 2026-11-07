//! What this binary is, for the daemon handshake and for diagnostics.
//!
//! `ENGINE` is the revision of the result semantics: what a wait means, what
//! counts as completion, how the daemon and the client talk. It changes
//! whenever a daemon of one revision could report something a client of
//! another would read differently, and both sides refuse a peer whose
//! revision differs. The package version alone cannot do this: builds with
//! different semantics have shipped under the same version.
//!
//! `REV` is the source revision the binary was built from, set at build time
//! with `T3_WATCH_BUILD_REV=<git commit>` (release packages always set it);
//! `dev` otherwise.

use serde_json::{json, Value};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const ENGINE: u32 = 4;
pub const REV: &str = match option_env!("T3_WATCH_BUILD_REV") {
    Some(r) => r,
    None => "dev",
};

pub fn json() -> Value {
    json!({ "version": VERSION, "engine": ENGINE, "rev": REV })
}

pub fn line() -> String {
    format!("t3-watch {VERSION} (engine {ENGINE}, rev {REV})")
}
