//! Delivery trace: one JSON line per step that decides when a caller hears
//! about a thread, appended to `<state>/trace.jsonl` (0600) by the waiting
//! client and by the daemon.
//!
//! Steps: a wait starts (with the client's and the daemon's build), the
//! daemon accepts it, a run's completion is first proven, the result payload
//! ends, an event is prepared (and its outbox entry created), each HTTP
//! attempt with its answer and retry deadline, queueing, delivery, and the
//! exit code. Correlated with the run's `completedAt` and the receiver's own
//! log, the lines show where time went when a wake comes late or never.
//!
//! Best effort: tracing never fails, blocks or changes anything else. Lines
//! hold ids, statuses, timestamps and error categories only, never URLs,
//! headers, secrets or message text: threads appear as parsed ids, never as
//! the arguments given (a thread URL can carry a token in its query). The
//! file is rotated to `trace.jsonl.1` past 4 MiB, and anything at its path
//! that is not a regular file is left alone.

use crate::{config, util, version};
use serde_json::{json, Value};
use std::fs;
use std::io::Write;

const MAX_BYTES: u64 = 4 << 20;

/// Appends `{ "at", "kind", "pid", "rev", ...fields }`.
pub fn record(kind: &str, fields: Value) {
    // Unit tests run without an isolated state directory: never write to
    // the real one from them.
    #[cfg(test)]
    if std::env::var_os("T3_WATCH_HOME").is_none() {
        return;
    }
    let path = config::trace_path();
    if fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && m.len() > MAX_BYTES) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        let _ = fs::rename(&path, old);
    }
    let mut line = json!({
        "at": util::rfc3339_ms(util::now_ms()),
        "kind": kind,
        "pid": std::process::id(),
        "engine": version::ENGINE,
        "rev": version::REV,
    });
    if let (Value::Object(l), Value::Object(f)) = (&mut line, fields) {
        l.extend(f);
    }
    let mut text = line.to_string();
    text.push('\n');
    // Never blocks: a FIFO, device or symlink at the trace path is skipped
    // (see `util::open_log`), so tracing cannot hold up a wait or delivery.
    if let Ok(mut f) = util::open_log(&path) {
        // One write per line: appends of a few hundred bytes do not
        // interleave between processes.
        let _ = f.write_all(text.as_bytes());
    }
}
