//! What this binary is, for the daemon handshake and for diagnostics.
//!
//! `ENGINE` is the revision of the result semantics: what a wait means, what
//! counts as completion, how the daemon and the client talk. It changes
//! whenever a daemon of one revision could report something a client of
//! another would read differently, and both sides refuse a peer whose
//! revision differs. The package version alone cannot do this: builds with
//! different semantics have shipped under the same version.
//!
//! `CAPABILITIES` names what a daemon must do for this client's results and
//! notifications to mean what they say. The client requires every one of
//! them in the daemon's `accepted` line, on top of the matching engine, so a
//! daemon that kept an engine number but lacks a behavior (as engine-4
//! builds before receipts did) is refused rather than trusted.
//!
//! `REV` is the source revision the binary was built from, set at build time
//! with `T3_WATCH_BUILD_REV=<git commit>` (release packages always set it);
//! `dev` otherwise.

use serde_json::{json, Value};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const ENGINE: u32 = 10;
pub const CAPABILITIES: &[&str] = &[
    // Outbox entries leave only with a written receipt; absence is never
    // delivery, a valid receipt is never overwritten, and one without the
    // entry's id and a known disposition is never trusted.
    "outbox-receipts",
    // Entries older builds left are pending until delivered, and those of
    // leased older stores are delivered in place, never adopted by age.
    "outbox-legacy-drain",
    "outbox-legacy-in-place",
    // Undelivered engine-4 entries stay where their clients look until the
    // trace shows those clients are done with them.
    "outbox-legacy-retain",
    // Retained engine-4 entries are held back unexpired, with no receipt
    // beside them, so older senders leave them alone too.
    "outbox-legacy-retain-held",
    // Receipts beside held engine-4 entries are taken back in checked steps
    // that never leave an entry more exposed to an older sender than
    // before, and repaired after a crash.
    "outbox-legacy-checked-convert",
    // Deadlines count suspended time; stalled polls restart on resume, and
    // answers or observations from before a resume are discarded.
    "suspend-aware",
    "suspend-invalidation",
];
pub const REV: &str = match option_env!("T3_WATCH_BUILD_REV") {
    Some(r) => r,
    None => "dev",
};

pub fn json() -> Value {
    json!({ "version": VERSION, "engine": ENGINE, "rev": REV, "capabilities": CAPABILITIES })
}

pub fn line() -> String {
    format!("t3-watch {VERSION} (engine {ENGINE}, rev {REV})")
}
