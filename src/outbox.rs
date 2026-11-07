//! Durable webhook delivery (`--durable`).
//!
//! Before the first attempt, the notification is written to
//! `<state>/outbox-v3/<id>.json` (0600 in a 0700 directory, fsynced, renamed
//! into place). The waiting client then tries it inline. A 2xx removes the
//! entry; anything else leaves it for the sender, which runs inside the
//! daemon (or as `t3-watch outbox run`) and retries with exponential
//! backoff, capped at five minutes, until a 2xx or the entry's expiry, when
//! it moves to `dead/`. One sender at a time holds `sender.lock`.
//! A server's `Retry-After` is a floor, not a suggestion: it is stored as the
//! entry's `notBefore` (across the hand-off from the client to the sender
//! too) and neither the backoff cap nor `outbox run --once` sends earlier.
//!
//! Each entry has a lease, an flock on `<id>.lock`: whoever sends it (the
//! client inline, or the sender) holds the lease for the attempt and for
//! recording its result, and rereads the entry once it has it. So the two
//! never send one entry at the same time, and neither writes over the
//! other's newer state. The client records its webhook result as soon as
//! the webhook is done, whatever a hook running beside it is doing. A
//! process that dies drops its lease.
//!
//! How an entry leaves the outbox is written down, never inferred: before
//! an entry is removed (delivered, dropped) or moved to `dead/` (expired), a
//! receipt `receipts/<id>.json` records the disposition, under the entry's
//! lease. A client that finds its entry gone reads the receipt; a missing
//! entry without one is `unknown`, never "delivered".
//!
//! A receipt is final. Whoever holds an entry's lease checks for one before
//! sending: an entry that still exists next to a receipt (a crash between
//! the two writes) is only cleaned up as the receipt says, never sent, and
//! its receipt is never rewritten.
//!
//! Older builds keep their entries elsewhere, and their senders never look
//! here: builds before engine 4 in `outbox/` (no leases), engine-4 builds in
//! `outbox-v2/` (leases, and in some builds no receipts). Their entries are
//! moved here only once no client of theirs can still be sending them: an
//! `outbox/` entry after the 90 s inline grace, under that build's
//! `outbox/sender.lock`; an `outbox-v2/` entry under its own lease, once it
//! has been attempted or is 90 s old, and only if it has no receipt there
//! (one with a receipt is finished as its receipt says). Until then they
//! count as pending everywhere: in `outbox list`, `status`, the exit code of
//! `outbox run --once`, and the daemon's decision to stay up.
//!
//! Every attempt for an entry carries the same `X-T3-Watch-Id` and
//! `Idempotency-Key`, so a receiver that got a request but whose answer was
//! lost (or a sender that crashed between the 2xx and removing the entry)
//! sees a duplicate it can drop, never a different event.
//!
//! An entry holds the webhook URL, headers and signing secret, protected the
//! same way as the OAuth tokens in `machines.json`. `outbox list` never
//! prints them.

use crate::config;
use crate::notify::{self, Webhook};
use crate::trace;
use crate::util::{self, Jitter};
use serde_json::{json, Value};
use std::fs::{self, File};
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
const BASE_DELAY: Duration = Duration::from_secs(2);
const MAX_DELAY: Duration = Duration::from_secs(300);
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);
/// Between writing an entry and its first inline attempt (which may wait
/// for earlier progress deliveries) the sender leaves it alone this long;
/// past that it assumes the client is gone. Inline attempts themselves are
/// covered by the lease.
const INLINE_GRACE: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: String,
    pub event: String,
    pub body: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub secret: Option<String>,
    pub created_ms: u64,
    pub expires_ms: u64,
    /// Every POST so far, inline ones included (sent as X-T3-Watch-Attempt).
    pub attempts: u32,
    /// The sender's own retries, which set the backoff.
    pub retries: u32,
    pub next_attempt_ms: u64,
    /// The receiver's `Retry-After`, as an absolute time. Never sent before.
    pub not_before_ms: Option<u64>,
    pub last_error: Option<String>,
    pub last_status: Option<u16>,
    pub last_attempt_ms: Option<u64>,
}

/// What the client's inline attempts ended with.
pub struct InlineFailure {
    pub attempts: u32,
    pub error: String,
    pub status: Option<u16>,
    pub not_before_ms: Option<u64>,
}

impl Entry {
    fn to_json(&self) -> Value {
        json!({
            "version": 1,
            "id": self.id,
            "event": self.event,
            "body": self.body,
            "url": self.url,
            "headers": self.headers.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
            "secret": self.secret,
            "createdMs": self.created_ms,
            "expiresMs": self.expires_ms,
            "attempts": self.attempts,
            "retries": self.retries,
            "nextAttemptMs": self.next_attempt_ms,
            "notBeforeMs": self.not_before_ms,
            "lastError": self.last_error,
            "lastStatus": self.last_status,
            "lastAttemptMs": self.last_attempt_ms,
        })
    }

    fn from_json(v: &Value) -> Option<Entry> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let n = |k: &str| v.get(k).and_then(Value::as_u64);
        let headers = v
            .get("headers")?
            .as_array()?
            .iter()
            .map(|h| Some((h.get(0)?.as_str()?.to_string(), h.get(1)?.as_str()?.to_string())))
            .collect::<Option<Vec<_>>>()?;
        Some(Entry {
            id: s("id").filter(|id| valid_id(id))?,
            event: s("event")?,
            body: s("body")?,
            url: s("url")?,
            headers,
            secret: s("secret"),
            created_ms: n("createdMs")?,
            expires_ms: n("expiresMs")?,
            attempts: n("attempts").unwrap_or(0) as u32,
            retries: n("retries").unwrap_or(0) as u32,
            next_attempt_ms: n("nextAttemptMs").unwrap_or(0),
            not_before_ms: n("notBeforeMs"),
            last_error: s("lastError"),
            last_status: n("lastStatus").map(|s| s as u16),
            last_attempt_ms: n("lastAttemptMs"),
        })
    }

    fn webhook(&self) -> Webhook {
        Webhook {
            url: self.url.clone(),
            headers: self.headers.clone(),
            secret: self.secret.as_ref().map(|s| s.as_bytes().to_vec()),
        }
    }

    /// What `outbox list` shows: no URL path or query, header or secret.
    pub fn public_json(&self) -> Value {
        json!({
            "id": self.id,
            "event": self.event,
            "destination": util::parse_url(&self.url).map(|u| u.origin),
            "createdAt": util::rfc3339_ms(self.created_ms),
            "expiresAt": util::rfc3339_ms(self.expires_ms),
            "attempts": self.attempts,
            "retries": self.retries,
            "nextAttemptAt": util::rfc3339_ms(self.next_attempt_ms),
            "notBefore": self.not_before_ms.map(util::rfc3339_ms),
            "lastError": self.last_error,
            "lastStatus": self.last_status,
            "lastAttemptAt": self.last_attempt_ms.map(util::rfc3339_ms),
            "inFlight": matches!(try_lease(&self.id), Ok(None)),
        })
    }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn dir() -> PathBuf {
    config::outbox_dir()
}

fn path_of(id: &str) -> PathBuf {
    dir().join(format!("{id}.json"))
}

fn lock_of(id: &str) -> PathBuf {
    dir().join(format!("{id}.lock"))
}

/// The exclusive right to send one entry and record the result. Dropped
/// (or the process gone), the lease is free again.
pub struct Lease {
    _file: File,
}

/// Takes the entry's lease if nobody holds it. `Ok(None)`: someone does.
pub fn try_lease(id: &str) -> Result<Option<Lease>, String> {
    if !valid_id(id) {
        return Err("not an outbox entry id".into());
    }
    util::ensure_private_dir(&dir()).map_err(|e| format!("cannot use the outbox directory: {e}"))?;
    let f = util::open_lock_file(&lock_of(id)).map_err(|e| format!("cannot open the entry lease: {e}"))?;
    match util::try_flock(&f) {
        Ok(true) => Ok(Some(Lease { _file: f })),
        Ok(false) => Ok(None),
        Err(e) => Err(format!("cannot take the entry lease: {e}")),
    }
}

fn save(e: &Entry) -> Result<(), String> {
    util::ensure_private_dir(&config::state_dir())
        .map_err(|err| format!("cannot prepare the state directory: {err}"))?;
    util::ensure_private_dir(&dir()).map_err(|err| format!("cannot use the outbox directory: {err}"))?;
    util::write_private(&path_of(&e.id), e.to_json().to_string().as_bytes())
        .map_err(|err| format!("cannot write outbox entry: {err}"))
}

pub fn load(id: &str) -> Option<Entry> {
    let text = fs::read_to_string(path_of(id)).ok()?;
    Entry::from_json(&serde_json::from_str(&text).ok()?)
}

/// Writes the entry before the first delivery attempt, so a crash from here
/// on loses nothing.
pub fn enqueue(w: &Webhook, event: &str, id: &str, body: &[u8], max_age: Duration) -> Result<(), String> {
    let now = util::now_ms();
    let secret = match &w.secret {
        Some(s) => Some(String::from_utf8(s.clone()).map_err(|_| "the webhook secret is not UTF-8".to_string())?),
        None => None,
    };
    save(&Entry {
        id: id.to_string(),
        event: event.to_string(),
        body: String::from_utf8_lossy(body).into_owned(),
        url: w.url.clone(),
        headers: w.headers.clone(),
        secret,
        created_ms: now,
        expires_ms: now + max_age.as_millis() as u64,
        attempts: 0,
        retries: 0,
        next_attempt_ms: now + INLINE_GRACE.as_millis() as u64,
        not_before_ms: None,
        last_error: None,
        last_status: None,
        last_attempt_ms: None,
    })?;
    trace::record(
        "outbox.created",
        json!({ "id": id, "event": event, "createdAt": util::rfc3339_ms(now) }),
    );
    Ok(())
}

/// Whether the client may still make its inline attempts on an entry whose
/// lease it holds: not when the sender already took it over (it has its own
/// schedule) or the receiver asked to wait.
pub fn inline_allowed(e: &Entry) -> bool {
    e.retries == 0 && e.not_before_ms.is_none_or(|t| t <= util::now_ms())
}

/// The inline attempts failed: hand the entry to the sender right away.
/// Call with the entry's lease held.
pub fn after_inline_failure(id: &str, f: &InlineFailure) -> Result<Entry, String> {
    let mut e = load(id).ok_or("the outbox entry disappeared")?;
    e.attempts += f.attempts;
    e.last_error = Some(f.error.clone());
    e.last_status = f.status;
    e.last_attempt_ms = Some(util::now_ms());
    e.not_before_ms = f.not_before_ms;
    e.next_attempt_ms = (util::now_ms() + BASE_DELAY.as_millis() as u64).max(f.not_before_ms.unwrap_or(0));
    save(&e)?;
    trace::record(
        "outbox.queued",
        json!({
            "id": id,
            "attempts": e.attempts,
            "lastStatus": e.last_status,
            "lastError": e.last_error,
            "nextAttemptAt": util::rfc3339_ms(e.next_attempt_ms),
            "notBefore": e.not_before_ms.map(util::rfc3339_ms),
        }),
    );
    Ok(e)
}

/// Removes an entry and its lease file. Call with the lease held, after
/// writing its receipt: the entry goes first, so a sender that gets a fresh
/// lease file finds nothing.
fn remove(id: &str) {
    if valid_id(id) {
        let _ = fs::remove_file(path_of(id));
        let _ = fs::remove_file(lock_of(id));
    }
}

/// How an entry left the outbox.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Disposition {
    Delivered,
    Expired,
    Dropped,
}

impl Disposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Disposition::Delivered => "delivered",
            Disposition::Expired => "expired",
            Disposition::Dropped => "dropped",
        }
    }
}

const RECEIPT_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

fn receipt_path(id: &str) -> PathBuf {
    dir().join("receipts").join(format!("{id}.json"))
}

/// Records how an entry left the outbox, then removes it (or, expired,
/// moves it to `dead/`). Call with the lease held. The receipt is written
/// (fsynced) first, so a crash in between leaves both, never neither.
pub fn finish(e: &Entry, how: Disposition, status: Option<u16>, by: &str) {
    // A receipt is final: if one is already there, clean up as it says.
    if let Some(existing) = receipt(&e.id).and_then(|r| disposition_of(&r)) {
        cleanup(e, existing);
        trace::record(
            "outbox.reconciled",
            json!({ "id": e.id, "disposition": existing.as_str(), "wanted": how.as_str(), "by": by }),
        );
        return;
    }
    let receipt = json!({
        "id": e.id,
        "event": e.event,
        "disposition": how.as_str(),
        "at": util::rfc3339_ms(util::now_ms()),
        "attempts": e.attempts,
        "status": status,
        "by": by,
    });
    let written = util::ensure_private_dir(&dir().join("receipts"))
        .and_then(|_| util::write_private(&receipt_path(&e.id), receipt.to_string().as_bytes()));
    cleanup(e, how);
    trace::record(
        &format!("outbox.{}", how.as_str()),
        json!({ "id": e.id, "attempts": e.attempts, "status": status, "by": by,
                "receiptError": written.err().map(|err| err.to_string()) }),
    );
}

/// Removes an entry (expired: moves it to `dead/`) and its lease file.
fn cleanup(e: &Entry, how: Disposition) {
    if how == Disposition::Expired {
        let dead = dir().join("dead");
        if util::ensure_private_dir(&dead).is_ok() {
            let _ = fs::rename(path_of(&e.id), dead.join(format!("{}.json", e.id)));
        }
        let _ = fs::remove_file(lock_of(&e.id));
    } else {
        remove(&e.id);
    }
}

fn disposition_of(r: &Value) -> Option<Disposition> {
    match r.get("disposition").and_then(Value::as_str)? {
        "delivered" => Some(Disposition::Delivered),
        "expired" => Some(Disposition::Expired),
        "dropped" => Some(Disposition::Dropped),
        _ => None,
    }
}

/// Call with the entry's lease held, before sending it: if a receipt says
/// it is finished, cleans up as the receipt says (without touching the
/// receipt) and returns that disposition. Nothing may send such an entry.
pub fn reconcile(e: &Entry) -> Option<Disposition> {
    let how = receipt(&e.id).and_then(|r| disposition_of(&r))?;
    cleanup(e, how);
    trace::record("outbox.reconciled", json!({ "id": e.id, "disposition": how.as_str() }));
    Some(how)
}

/// The receipt of an entry that left the outbox, if one was written.
pub fn receipt(id: &str) -> Option<Value> {
    if !valid_id(id) {
        return None;
    }
    serde_json::from_str(&fs::read_to_string(receipt_path(id)).ok()?).ok()
}

/// What is on disk for an entry right now.
pub enum EntryState {
    Present(Box<Entry>),
    /// The file is there but cannot be read as an entry.
    Unreadable,
    Missing,
}

pub fn entry_state(id: &str) -> EntryState {
    match load(id) {
        Some(e) => EntryState::Present(Box::new(e)),
        None if path_of(id).exists() => EntryState::Unreadable,
        None => EntryState::Missing,
    }
}

/// Removes receipts past their time to live.
fn prune_receipts() {
    let cutoff = std::time::SystemTime::now() - RECEIPT_TTL;
    for f in fs::read_dir(dir().join("receipts")).into_iter().flatten().flatten() {
        if f.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < cutoff) {
            let _ = fs::remove_file(f.path());
        }
    }
}

/// Every readable entry, oldest first. Unreadable files are skipped (and
/// reported by `list`).
pub fn entries() -> (Vec<Entry>, Vec<String>) {
    let mut out = Vec::new();
    let mut bad = Vec::new();
    let Ok(rd) = fs::read_dir(dir()) else {
        return (out, bad);
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_suffix(".json") else {
            continue;
        };
        if name.starts_with('.') || !valid_id(id) {
            continue;
        }
        match load(id) {
            Some(entry) => out.push(entry),
            None => bad.push(name),
        }
    }
    out.sort_by_key(|e| e.created_ms);
    (out, bad)
}

fn read_entry(path: &std::path::Path) -> Option<Entry> {
    fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| Entry::from_json(&v))
}

/// Valid entry files in an older build's outbox directory.
fn legacy_files(dir: &std::path::Path) -> Vec<(String, PathBuf)> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|f| {
            let name = f.file_name().to_string_lossy().into_owned();
            let id = name
                .strip_suffix(".json")
                .filter(|id| valid_id(id) && !name.starts_with('.'))?;
            Some((id.to_string(), f.path()))
        })
        .collect()
}

fn v2_receipt(id: &str) -> Option<Value> {
    let path = config::legacy_v2_outbox_dir()
        .join("receipts")
        .join(format!("{id}.json"));
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Entries older builds left in `outbox/` and `outbox-v2/` that are still
/// to be sent (an `outbox-v2/` entry with a receipt there is finished).
pub fn legacy_entries() -> Vec<Entry> {
    let v1 = legacy_files(&config::legacy_outbox_dir());
    let v2 = legacy_files(&config::legacy_v2_outbox_dir())
        .into_iter()
        .filter(|(id, _)| v2_receipt(id).is_none());
    let mut out: Vec<Entry> = v1.into_iter().chain(v2).filter_map(|(_, p)| read_entry(&p)).collect();
    out.sort_by_key(|e| e.created_ms);
    out
}

/// Everything still to be sent: this outbox's entries and the legacy ones
/// waiting for migration. The daemon stays up while this is not zero.
pub fn pending_count() -> usize {
    entries().0.len() + legacy_entries().len()
}

/// When a legacy entry may be migrated (no client of its build can still
/// be sending it).
fn legacy_eligible_ms(e: &Entry) -> u64 {
    e.created_ms + INLINE_GRACE.as_millis() as u64
}

/// Moves entries builds before engine 4 left in `outbox/` into this outbox,
/// under their own sender lock so none of their senders is mid-pass. Only
/// entries past the inline grace: those builds had no leases, and one of
/// their clients may still be sending a younger entry.
fn migrate_v1() -> Vec<Value> {
    let legacy = config::legacy_outbox_dir();
    let mut out = Vec::new();
    let files = legacy_files(&legacy);
    if files.is_empty() {
        return out;
    }
    let Ok(lock) = util::open_lock_file(&legacy.join("sender.lock")) else {
        return out;
    };
    if !util::try_flock(&lock).unwrap_or(false) {
        return out;
    }
    let now = util::now_ms();
    for (id, path) in files {
        let Some(e) = read_entry(&path) else {
            continue;
        };
        if now < legacy_eligible_ms(&e) {
            out.push(json!({ "id": id, "legacy": "outbox", "deferred": true,
                             "eligibleAt": util::rfc3339_ms(legacy_eligible_ms(&e)),
                             "reason": "left by an older build less than 90 s ago; its client may still be sending it" }));
            continue;
        }
        adopt(e, &path, &mut out, "outbox");
    }
    out
}

/// Moves entries engine-4 builds left in `outbox-v2/` into this outbox. Each
/// only under its own lease (so no process of that build is working on it),
/// and only once its client is done with it: attempted, or past the inline
/// grace. An entry with a receipt there is finished as the receipt says.
fn migrate_v2() -> Vec<Value> {
    let legacy = config::legacy_v2_outbox_dir();
    let mut out = Vec::new();
    let now = util::now_ms();
    for (id, path) in legacy_files(&legacy) {
        let lock_path = legacy.join(format!("{id}.lock"));
        let Ok(lock) = util::open_lock_file(&lock_path) else {
            continue;
        };
        if !util::try_flock(&lock).unwrap_or(false) {
            out.push(json!({ "id": id, "legacy": "outbox-v2", "inFlight": true,
                             "reason": "a process of an older build is sending this entry now" }));
            continue;
        }
        let Some(e) = read_entry(&path) else {
            continue;
        };
        if let Some(r) = v2_receipt(&id) {
            // Finished there: clean up as its receipt says, never resend.
            if r.get("disposition").and_then(Value::as_str) == Some("expired") {
                let dead = legacy.join("dead");
                if util::ensure_private_dir(&dead).is_ok() {
                    let _ = fs::rename(&path, dead.join(format!("{id}.json")));
                }
            } else {
                let _ = fs::remove_file(&path);
            }
            let _ = fs::remove_file(&lock_path);
            trace::record(
                "outbox.reconciled",
                json!({ "id": id, "legacy": "outbox-v2",
                                                      "disposition": r.get("disposition") }),
            );
            continue;
        }
        if e.attempts == 0 && e.retries == 0 && now < legacy_eligible_ms(&e) {
            out.push(json!({ "id": id, "legacy": "outbox-v2", "deferred": true,
                             "eligibleAt": util::rfc3339_ms(legacy_eligible_ms(&e)),
                             "reason": "an older build's client may still be about to send it" }));
            continue;
        }
        adopt(e, &path, &mut out, "outbox-v2");
        let _ = fs::remove_file(&lock_path);
    }
    out
}

/// Takes over a legacy entry: saves it here (unless already here) and
/// removes the old file.
fn adopt(mut e: Entry, path: &std::path::Path, out: &mut Vec<Value>, from: &str) {
    if load(&e.id).is_none() {
        e.next_attempt_ms = e.next_attempt_ms.min(util::now_ms());
        if save(&e).is_err() {
            return;
        }
    }
    let _ = fs::remove_file(path);
    trace::record("outbox.migrated", json!({ "id": e.id, "from": from }));
    out.push(json!({ "id": e.id, "migrated": true, "from": from }));
}

fn backoff(attempts: u32, jitter: &mut Jitter) -> Duration {
    jitter.backoff(attempts.saturating_sub(1), BASE_DELAY, MAX_DELAY)
}

/// One pass over the outbox: sends what is due (with `force`, everything
/// the receiver has not asked to hold back with `Retry-After`), moves
/// expired entries to `dead/`, and leaves alone any entry whose lease
/// someone else holds. Returns per-entry results. Holds the sender lock for
/// the pass; if another sender has it, does nothing.
pub fn pass(force: bool) -> Result<Vec<Value>, String> {
    let mut results = migrate_v1();
    results.extend(migrate_v2());
    if !dir().is_dir() {
        return Ok(results);
    }
    util::ensure_private_dir(&dir()).map_err(|e| format!("cannot use the outbox directory: {e}"))?;
    let lock =
        util::open_lock_file(&dir().join("sender.lock")).map_err(|e| format!("cannot open the sender lock: {e}"))?;
    if !util::try_flock(&lock).map_err(|e| format!("cannot lock the outbox: {e}"))? {
        results.push(json!({ "skipped": "another sender is running" }));
        return Ok(results);
    }
    let mut jitter = Jitter::new();
    for listed in entries().0 {
        let lease = match try_lease(&listed.id) {
            Ok(Some(l)) => l,
            Ok(None) => {
                results.push(json!({ "id": listed.id, "inFlight": true,
                                     "reason": "another process is sending this entry now" }));
                continue;
            }
            Err(e) => {
                results.push(json!({ "id": listed.id, "error": e }));
                continue;
            }
        };
        // Reread under the lease: whoever held it may have changed it.
        let Some(mut e) = load(&listed.id) else {
            drop(lease);
            continue;
        };
        // Finished already (a crash left the entry beside its receipt).
        if let Some(how) = reconcile(&e) {
            results.push(json!({ "id": e.id, "reconciled": how.as_str() }));
            continue;
        }
        let now = util::now_ms();
        if now >= e.expires_ms {
            finish(&e, Disposition::Expired, e.last_status, "outbox sender");
            results.push(json!({ "id": e.id, "deadLettered": true, "attempts": e.attempts }));
            continue;
        }
        if let Some(t) = e.not_before_ms.filter(|t| now < *t) {
            results.push(json!({ "id": e.id, "deferred": true, "notBefore": util::rfc3339_ms(t),
                                 "reason": "the receiver asked for this delay (Retry-After)" }));
            continue;
        }
        if !force && now < e.next_attempt_ms {
            continue;
        }
        let w = e.webhook();
        e.attempts += 1;
        e.retries += 1;
        let a = notify::post_once(
            &notify::agent_for(&w),
            &w,
            &e.event,
            &e.id,
            e.body.as_bytes(),
            ATTEMPT_TIMEOUT,
            e.attempts,
        );
        let now = util::now_ms();
        e.last_attempt_ms = Some(now);
        trace::record(
            "outbox.attempt",
            json!({ "id": e.id, "event": e.event, "attempt": e.attempts, "status": a.status,
                    "error": a.error, "retryAfterMs": a.retry_after.map(|d| d.as_millis() as u64) }),
        );
        if a.delivered {
            finish(&e, Disposition::Delivered, a.status, "outbox sender");
            results.push(json!({ "id": e.id, "delivered": true, "attempts": e.attempts, "status": a.status }));
            continue;
        }
        e.last_error = a.error;
        e.last_status = a.status;
        // Only our own backoff is capped; the receiver's delay is a floor.
        e.not_before_ms = a.retry_after.map(|d| now + d.as_millis() as u64);
        let own = now + backoff(e.retries, &mut jitter).min(MAX_DELAY).as_millis() as u64;
        e.next_attempt_ms = own.max(e.not_before_ms.unwrap_or(0));
        let saved = save(&e);
        results.push(json!({
            "id": e.id,
            "delivered": false,
            "attempts": e.attempts,
            "status": e.last_status,
            "error": e.last_error,
            "nextAttemptAt": util::rfc3339_ms(e.next_attempt_ms),
            "saveError": saved.err(),
        }));
    }
    prune_receipts();
    // Lease files whose entry is gone (a crash between the two removals).
    for f in fs::read_dir(dir()).into_iter().flatten().flatten() {
        let name = f.file_name().to_string_lossy().into_owned();
        if let Some(id) = name.strip_suffix(".lock").filter(|id| valid_id(id)) {
            if load(id).is_none() {
                if let Ok(Some(_lease)) = try_lease(id) {
                    if load(id).is_none() {
                        let _ = fs::remove_file(f.path());
                    }
                }
            }
        }
    }
    Ok(results)
}

/// The supervised sender: passes forever, every couple of seconds. Logs a
/// line per attempt through `log` (never URLs or secrets).
pub fn run_forever(log: &dyn Fn(&str)) -> ! {
    loop {
        match pass(false) {
            Ok(results) => {
                for r in results
                    .iter()
                    .filter(|r| r.get("id").is_some() && r.get("deferred").is_none() && r.get("inFlight").is_none())
                {
                    log(&format!("outbox: {r}"));
                }
            }
            Err(e) => log(&format!("outbox: {e}")),
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

pub fn summary_json() -> Value {
    let (entries, bad) = entries();
    let legacy = legacy_entries();
    let oldest = entries.iter().chain(&legacy).map(|e| e.created_ms).min();
    json!({
        "pending": entries.len() + legacy.len(),
        "legacyPending": legacy.len(),
        "unreadable": bad.len(),
        "oldestCreatedAt": oldest.map(util::rfc3339_ms),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_round_trip_and_hide_secrets_in_listings() {
        let e = Entry {
            id: "00ff".into(),
            event: "settled".into(),
            body: "{\"event\":\"settled\"}".into(),
            url: "https://hooks.example/path/s3cr3t-path?token=abc".into(),
            headers: vec![("Authorization".into(), "Bearer xyz".into())],
            secret: Some("signing-key".into()),
            created_ms: 1,
            expires_ms: 2,
            attempts: 3,
            retries: 1,
            next_attempt_ms: 4,
            not_before_ms: Some(5),
            last_attempt_ms: Some(6),
            last_error: Some("host not found".into()),
            last_status: None,
        };
        assert_eq!(Entry::from_json(&e.to_json()), Some(e.clone()));
        let public = e.public_json().to_string();
        for secret in ["s3cr3t-path", "token=abc", "Bearer xyz", "signing-key"] {
            assert!(!public.contains(secret), "{public}");
        }
        assert!(public.contains("https://hooks.example"));
        assert!(!valid_id("../x") && !valid_id("") && valid_id("abc123"));
    }
}
