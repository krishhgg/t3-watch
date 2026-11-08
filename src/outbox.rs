//! Durable webhook delivery (`--durable`).
//!
//! Before the first attempt, the notification is written to
//! `<state>/outbox-v4/<id>.json` (0600 in a 0700 directory, fsynced, renamed
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
//! A receipt is final, and checked: it counts only if it names the entry's
//! id and a known disposition. Whoever holds an entry's lease looks for one
//! before sending. An entry beside a valid receipt (a crash between the two
//! writes) is only cleaned up as the receipt says, never sent, and the
//! receipt is never rewritten. An entry beside an invalid receipt is kept
//! and sent as usual, and the bad receipt is reported, never trusted.
//!
//! Older builds keep their entries elsewhere, and their senders never look
//! here:
//!
//! - Engine-4 and engine-5 builds use `outbox-v2/` and `outbox-v3/`, with the
//!   same entry format and per-entry leases. Their entries are never moved:
//!   a client of theirs may still be about to send one, and if it found its
//!   entry gone it would report the event delivered. They are delivered where
//!   they are, exactly as those builds' own senders would: under the entry's
//!   lease and that store's sender lock, after its `nextAttemptMs`, counting
//!   the attempt as a sender retry. An old client that comes back to it then
//!   either finds it still there with a retry recorded (and reports it
//!   queued, not delivered) or gone after a real 2xx (with a receipt).
//! - Engine-5 clients read a receipt when their entry is gone, so an
//!   `outbox-v3/` entry that expires or is dropped leaves as here. Engine-4
//!   clients (`outbox-v2/`) read a missing entry as delivered, receipt or
//!   not, so an `outbox-v2/` entry that leaves undelivered (expired,
//!   dropped) stays where it is (see `Store::retain`): a sender retry
//!   recorded, so such a client reports it queued; not expired, and held
//!   back until 2100, so no engine-4, engine-6 or engine-7 sender expires,
//!   sends or removes it; and no receipt beside it until it leaves. It goes
//!   only once the delivery trace, which those builds write too, shows that
//!   client is done with it: its `notify.result` for the entry, or the
//!   process that created the entry gone. Without that evidence it stays
//!   (`outbox list`, under `retained`); no age, absence or guess counts.
//!   A receipt an older build left beside such an entry is taken back in
//!   steps that never leave it more exposed than before (see
//!   `Store::convert`).
//!   An engine-5 sender moves such entries whatever their state; it is
//!   reported (`incompatibleSenders`) when its daemon is found.
//! - Builds before engine 4 used `outbox/`, without leases; their clients
//!   never treat a missing entry as delivered. Their entries move here after
//!   the 90 s inline grace, under their `outbox/sender.lock`.
//!
//! Until then all of these count as pending everywhere: in `outbox list`,
//! `status`, the exit code of `outbox run --once`, and the daemon's decision
//! to stay up.
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
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
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
const RECEIPT_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

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
    /// The sender's own retries, which set the backoff. Above zero, the
    /// client no longer attempts it inline.
    pub retries: u32,
    pub next_attempt_ms: u64,
    /// The receiver's `Retry-After`, as an absolute time. Never sent before.
    pub not_before_ms: Option<u64>,
    pub last_error: Option<String>,
    pub last_status: Option<u16>,
    pub last_attempt_ms: Option<u64>,
    /// An undelivered `outbox-v2/` entry kept for its engine-4 client (see
    /// `Store::retain`): how it left, and its real expiry.
    pub retained_as: Option<Disposition>,
    pub retained_expires_ms: Option<u64>,
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
        let mut v = json!({
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
        });
        if let Some(how) = self.retained_as {
            v["retainedAs"] = json!(how.as_str());
            v["retainedExpiresMs"] = json!(self.retained_expires_ms);
        }
        v
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
            retained_as: s("retainedAs").as_deref().and_then(Disposition::parse),
            retained_expires_ms: n("retainedExpiresMs"),
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
        self.public_json_in(&Store::current(), true)
    }

    /// `probe`: whether `inFlight` may be found out by trying the entry's
    /// lease, which opens (creates) its lock file. Never in a blocked
    /// outbox-v2: listing it must not touch it.
    fn public_json_in(&self, store: &Store, probe: bool) -> Value {
        json!({
            "id": self.id,
            "event": self.event,
            "store": store.label,
            "destination": util::parse_url(&self.url).map(|u| u.origin),
            "createdAt": util::rfc3339_ms(self.created_ms),
            "expiresAt": util::rfc3339_ms(self.retained_expires_ms.unwrap_or(self.expires_ms)),
            "retainedAs": self.retained_as.map(Disposition::as_str),
            "attempts": self.attempts,
            "retries": self.retries,
            "nextAttemptAt": util::rfc3339_ms(self.next_attempt_ms),
            "notBefore": self.not_before_ms.map(util::rfc3339_ms),
            "lastError": self.last_error,
            "lastStatus": self.last_status,
            "lastAttemptAt": self.last_attempt_ms.map(util::rfc3339_ms),
            "inFlight": if probe { json!(matches!(store.try_lease(&self.id), Ok(None))) } else { Value::Null },
        })
    }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The exclusive right to send one entry and record the result. Dropped
/// (or the process gone), the lease is free again.
pub struct Lease {
    _file: File,
}

/// How an entry left the outbox.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Disposition {
    Delivered,
    Expired,
    Dropped,
}

impl Disposition {
    fn parse(s: &str) -> Option<Disposition> {
        match s {
            "delivered" => Some(Disposition::Delivered),
            "expired" => Some(Disposition::Expired),
            "dropped" => Some(Disposition::Dropped),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Disposition::Delivered => "delivered",
            Disposition::Expired => "expired",
            Disposition::Dropped => "dropped",
        }
    }
}

/// What the receipt of an entry says.
pub enum ReceiptStatus {
    None,
    /// A receipt for this id with a known disposition.
    Valid(Disposition, Value),
    /// A receipt is there but cannot be trusted (wrong id, unknown or missing
    /// disposition, not JSON): the event must not be treated as finished.
    Invalid(String),
}

/// What is on disk for an entry right now.
pub enum EntryState {
    Present(Box<Entry>),
    /// The file is there but cannot be read as an entry.
    Unreadable,
    Missing,
}

/// Why an undelivered `outbox-v2/` entry is still there.
const RETAINED: &str = "an engine-4 client reads a missing entry as delivered; kept, held back from \
                        engine-4, engine-6 and engine-7 senders (not engine 5's, see incompatibleSenders), \
                        until the trace shows the client that made it is done with it";

/// What an entry still beside its receipt is exposed to.
const AT_RISK: &str = "an engine-5 or engine-6 sender would remove this entry beside its receipt, and its \
                       engine-4 client would then report the event delivered; stop such senders until it is \
                       taken back";

/// Where a retained entry's expiry and next attempt are set, so that no
/// build reading `outbox-v2/` treats it as expired or due:
/// 2100-01-01T00:00:00Z.
const RETAINED_UNTIL_MS: u64 = 4_102_444_800_000;

/// How often the supervised sender reads the trace again for retained
/// entries (`outbox run --once` and `outbox drop` always do).
const RETIRE_RESCAN: Duration = Duration::from_secs(30);

/// One outbox directory: this build's, or an older build's leased one.
struct Store {
    dir: PathBuf,
    label: &'static str,
    /// Clients of the build that writes here report an entry they find gone
    /// as delivered (engine 4, `outbox-v2/`).
    absence_is_delivery: bool,
    /// A trace scan made before this (ms since the epoch) is read again.
    trace_fresh_after_ms: u64,
}

impl Store {
    fn current() -> Store {
        Store {
            dir: config::outbox_dir(),
            label: "outbox-v4",
            absence_is_delivery: false,
            trace_fresh_after_ms: 0,
        }
    }

    /// Older builds' stores with the same format and leases, delivered in
    /// place.
    fn leased_legacy(trace_fresh_after_ms: u64) -> [Store; 2] {
        [
            Store {
                dir: config::legacy_v2_outbox_dir(),
                label: "outbox-v2",
                absence_is_delivery: true,
                trace_fresh_after_ms,
            },
            Store {
                dir: config::legacy_v3_outbox_dir(),
                label: "outbox-v3",
                absence_is_delivery: false,
                trace_fresh_after_ms,
            },
        ]
    }

    fn path_of(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn lock_of(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.lock"))
    }

    fn receipt_path(&self, id: &str) -> PathBuf {
        self.dir.join("receipts").join(format!("{id}.json"))
    }

    fn try_lease(&self, id: &str) -> Result<Option<Lease>, String> {
        if !valid_id(id) {
            return Err("not an outbox entry id".into());
        }
        util::ensure_private_dir(&self.dir).map_err(|e| format!("cannot use the outbox directory: {e}"))?;
        let f = util::open_lock_file(&self.lock_of(id)).map_err(|e| format!("cannot open the entry lease: {e}"))?;
        match util::try_flock(&f) {
            Ok(true) => Ok(Some(Lease { _file: f })),
            Ok(false) => Ok(None),
            Err(e) => Err(format!("cannot take the entry lease: {e}")),
        }
    }

    fn save(&self, e: &Entry) -> Result<(), String> {
        util::ensure_private_dir(&config::state_dir())
            .map_err(|err| format!("cannot prepare the state directory: {err}"))?;
        util::ensure_private_dir(&self.dir).map_err(|err| format!("cannot use the outbox directory: {err}"))?;
        util::write_private(&self.path_of(&e.id), e.to_json().to_string().as_bytes())
            .map_err(|err| format!("cannot write outbox entry: {err}"))
    }

    fn load(&self, id: &str) -> Option<Entry> {
        read_entry(&self.path_of(id))
    }

    fn entry_state(&self, id: &str) -> EntryState {
        match self.load(id) {
            Some(e) => EntryState::Present(Box::new(e)),
            None if self.path_of(id).exists() => EntryState::Unreadable,
            None => EntryState::Missing,
        }
    }

    fn receipt_status(&self, id: &str) -> ReceiptStatus {
        if !valid_id(id) {
            return ReceiptStatus::None;
        }
        let Ok(text) = fs::read_to_string(self.receipt_path(id)) else {
            return ReceiptStatus::None;
        };
        let Ok(r) = serde_json::from_str::<Value>(&text) else {
            return ReceiptStatus::Invalid("the receipt is not JSON".into());
        };
        if r.get("id").and_then(Value::as_str) != Some(id) {
            return ReceiptStatus::Invalid("the receipt names another entry, or none".into());
        }
        match r
            .get("disposition")
            .and_then(Value::as_str)
            .and_then(Disposition::parse)
        {
            Some(how) => ReceiptStatus::Valid(how, r),
            None => ReceiptStatus::Invalid("the receipt has no known disposition".into()),
        }
    }

    /// Removes an entry and its lease file. Call with the lease held, after
    /// writing its receipt: the entry goes first, so a sender that gets a
    /// fresh lease file finds nothing.
    fn remove(&self, id: &str) {
        if valid_id(id) {
            let _ = fs::remove_file(self.path_of(id));
            let _ = fs::remove_file(self.lock_of(id));
        }
    }

    /// Whether the client that made this entry is done with it, by the
    /// delivery trace: its `notify.result` for the entry, or the process
    /// that created the entry no longer running. Nothing else counts.
    fn client_done(&self, id: &str) -> bool {
        let mut cache = TRACE_SCAN.lock().unwrap_or_else(|e| e.into_inner());
        if cache.as_ref().is_none_or(|s| s.at_ms < self.trace_fresh_after_ms) {
            *cache = Some(scan_trace());
        }
        cache.as_ref().is_some_and(|scan| {
            scan.done.contains(id) || scan.creators.get(id).is_some_and(|pid| !util::process_running(*pid))
        })
    }

    /// An undelivered entry leaving a store whose clients read absence as
    /// delivery, while the client that made it may still look for it.
    fn must_keep(&self, e: &Entry, how: Disposition) -> bool {
        self.absence_is_delivery && how != Disposition::Delivered && !self.client_done(&e.id)
    }

    /// Keeps an undelivered entry where an engine-4 client looks, in a state
    /// that client reads as not delivered and that every older sender
    /// leaves alone. The client finds it there with a sender retry recorded,
    /// so it reports the event queued and does not send it itself. It is
    /// not expired and is held back (`notBefore`, next attempt) until 2100,
    /// so engine-4 senders (which expire first, then honour `notBefore`,
    /// even forced) and engine-6 and engine-7 senders neither dead-letter
    /// nor send it. How it left, and its real expiry, are kept in the entry
    /// (`retainedAs`, `retainedExpiresMs`), never as a receipt here: those
    /// senders clean up an entry beside a receipt. Call with the lease held.
    /// A failed save is returned, never ignored.
    fn retain(&self, e: &Entry, how: Disposition) -> Result<(), String> {
        let mut kept = e.clone();
        kept.retries = kept.retries.max(1);
        kept.retained_as = Some(e.retained_as.unwrap_or(how));
        kept.retained_expires_ms = Some(e.retained_expires_ms.unwrap_or(e.expires_ms));
        kept.expires_ms = RETAINED_UNTIL_MS;
        kept.not_before_ms = Some(RETAINED_UNTIL_MS);
        kept.next_attempt_ms = RETAINED_UNTIL_MS;
        if kept == *e {
            return Ok(());
        }
        let saved = self.save(&kept);
        trace::record(
            "outbox.retained",
            json!({ "id": e.id, "store": self.label, "disposition": how.as_str(),
                    "saveError": saved.as_ref().err() }),
        );
        saved.map_err(|why| format!("cannot hold the entry for its engine-4 client: {why}"))
    }

    /// A retained entry whose client is done with it leaves as it would
    /// have: with its real expiry back, a receipt, and expired to `dead/` or
    /// dropped removed. Call with the lease held.
    fn retire(&self, e: &Entry, how: Disposition) {
        let mut back = e.clone();
        back.expires_ms = e.retained_expires_ms.unwrap_or(e.expires_ms);
        back.next_attempt_ms = back.expires_ms;
        back.not_before_ms = None;
        back.retained_as = None;
        back.retained_expires_ms = None;
        // Its client is done with it: whatever happens from here is safe.
        let _ = self.save(&back);
        let _ = self.finish(&back, how, back.last_status, "outbox sender");
    }

    /// Removes an entry (expired: moves it to `dead/`) and its lease file.
    fn cleanup(&self, e: &Entry, how: Disposition) {
        if how == Disposition::Expired {
            let dead = self.dir.join("dead");
            if util::ensure_private_dir(&dead).is_ok() {
                let _ = fs::rename(self.path_of(&e.id), dead.join(format!("{}.json", e.id)));
            }
            let _ = fs::remove_file(self.lock_of(&e.id));
        } else {
            self.remove(&e.id);
        }
    }

    /// An entry beside a valid receipt that must stay for its engine-4
    /// client (an older build expired or dropped it, or a conversion was
    /// cut short): held, then the receipt taken back until it leaves, since
    /// an engine-6 sender removes an entry beside a receipt. Call with the
    /// store's sender lock and the entry's lease held.
    ///
    /// No older sender can be kept out across a crash of this process (its
    /// locks die with it, and a sender that already opened a lock file keeps
    /// it), so the steps are ordered so that every state they pass through
    /// is no more exposed than the one before, under each older build's
    /// rules: holding the entry first ends what an engine-4 sender would do
    /// (dead-letter it once expired) and leaves only what an engine-6 sender
    /// would already have done (clean up beside the receipt); taking the
    /// receipt back then ends that too. Each step is checked and a failure
    /// returned; whatever is left is repaired on the next pass.
    fn convert(&self, e: &Entry, how: Disposition) -> Result<(), String> {
        self.retain(e, how)?;
        util::test_fault("before-receipt-unlink");
        match fs::remove_file(self.receipt_path(&e.id)) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(format!(
                    "cannot take back the receipt beside an entry held for its engine-4 client: {err}"
                ))
            }
        }
        trace::record(
            "outbox.converted",
            json!({ "id": e.id, "store": self.label, "disposition": how.as_str() }),
        );
        Ok(())
    }

    /// A receipt beside an entry its engine-4 client may still read, which
    /// must be taken back (see `convert`).
    fn needs_conversion(&self, e: &Entry) -> Option<Disposition> {
        if !self.absence_is_delivery {
            return None;
        }
        match self.receipt_status(&e.id) {
            ReceiptStatus::Valid(how, _)
                if how != Disposition::Delivered && (e.retained_as.is_some() || !self.client_done(&e.id)) =>
            {
                Some(how)
            }
            _ => None,
        }
    }

    /// Before anything else in a pass over `outbox-v2/`, holds every entry
    /// that must be kept for its engine-4 client: one beside an undelivered
    /// receipt, an expired one, or one an older build moved while held. Each
    /// hold is a single atomic write that ends what an engine-4 sender would
    /// do with the entry, so doing them all first means that a pass cut
    /// short later (taking a receipt back takes two writes) never leaves an
    /// entry it had not reached yet in the state an engine-4 sender
    /// dead-letters. Returns the ids it could not hold (reported), and the
    /// leases of entries whose receipt is still to be taken back: kept, so
    /// that each entry's hold and receipt removal happen under one lease.
    fn hold_first(&self, results: &mut Vec<Value>) -> (HashSet<String>, HashMap<String, Lease>) {
        let mut failed = HashSet::new();
        let mut kept = HashMap::new();
        if !self.absence_is_delivery {
            return (failed, kept);
        }
        for listed in self.entries().0 {
            let Ok(Some(lease)) = self.try_lease(&listed.id) else {
                continue;
            };
            let Some(e) = self.load(&listed.id) else {
                continue;
            };
            let converting = self.needs_conversion(&e);
            let how = if let Some(how) = converting {
                how
            } else if e.retained_as.is_none()
                && !self.finished(&e.id)
                && (e.not_before_ms == Some(RETAINED_UNTIL_MS)
                    || (util::now_ms() >= e.expires_ms && self.must_keep(&e, Disposition::Expired)))
            {
                Disposition::Expired
            } else {
                continue;
            };
            if let Err(why) = self.retain(&e, how) {
                results.push(json!({ "id": e.id, "store": self.label, "conversionError": why,
                                     "atRisk": AT_RISK }));
                failed.insert(e.id.clone());
            } else if converting.is_some() {
                kept.insert(e.id.clone(), lease);
            }
        }
        (failed, kept)
    }

    /// Whether a `sender.lock` directory left by the 0.7.0 fence stands. No
    /// leased older build can open it, so it keeps their senders out; it
    /// stays until nothing here is left to hold or take back.
    fn old_fence(&self) -> bool {
        self.absence_is_delivery && fs::symlink_metadata(self.dir.join("sender.lock")).is_ok_and(|m| m.is_dir())
    }

    /// Entries that still need holding or their receipt taken back.
    fn unsettled_legacy(&self) -> usize {
        self.entries()
            .0
            .iter()
            .filter(|e| {
                self.needs_conversion(e).is_some()
                    || (e.retained_as.is_none()
                        && !self.finished(&e.id)
                        && util::now_ms() >= e.expires_ms
                        && self.must_keep(e, Disposition::Expired))
            })
            .count()
    }

    /// Lowers a 0.7.0 fence: the directory removed and the lock file it
    /// moved aside put back, so every sender can lock this store again. Call
    /// holding the flock on `sender.fenced.lock`, once nothing is left to
    /// hold or take back.
    fn clear_old_fence(&self) {
        let lock = self.dir.join("sender.lock");
        if fs::symlink_metadata(&lock).is_ok_and(|m| m.is_dir()) && fs::remove_dir(&lock).is_ok() {
            let _ = fs::rename(self.dir.join("sender.fenced.lock"), &lock);
            trace::record("outbox.fence", json!({ "store": self.label, "cleared": true }));
        }
    }

    /// Records how an entry left the store, then removes it (or, expired,
    /// moves it to `dead/`). Call with the lease held. The receipt is written
    /// (fsynced) first, so a crash in between leaves both, never neither. A
    /// valid receipt already there is final: cleanup follows it instead.
    /// An undelivered entry an engine-4 client may still look for is
    /// retained instead (see `retain`).
    fn finish(&self, e: &Entry, how: Disposition, status: Option<u16>, by: &str) -> Result<(), String> {
        if let ReceiptStatus::Valid(existing, _) = self.receipt_status(&e.id) {
            if self.must_keep(e, existing) {
                return Err(format!(
                    "already {} beside its receipt; the next outbox pass holds it for its engine-4 client",
                    existing.as_str()
                ));
            }
            self.cleanup(e, existing);
            trace::record(
                "outbox.reconciled",
                json!({ "id": e.id, "store": self.label, "disposition": existing.as_str(),
                        "wanted": how.as_str(), "by": by }),
            );
            return Ok(());
        }
        if self.must_keep(e, how) {
            return self.retain(e, how);
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
        let written = util::ensure_private_dir(&self.dir.join("receipts"))
            .and_then(|_| util::write_private(&self.receipt_path(&e.id), receipt.to_string().as_bytes()));
        self.cleanup(e, how);
        trace::record(
            &format!("outbox.{}", how.as_str()),
            json!({ "id": e.id, "store": self.label, "attempts": e.attempts, "status": status, "by": by,
                    "receiptError": written.err().map(|err| err.to_string()) }),
        );
        Ok(())
    }

    /// Call with the entry's lease held, before sending it. A valid receipt
    /// means it is finished: cleaned up as the receipt says (the receipt
    /// untouched), and nothing may send it. An invalid receipt is reported
    /// and ignored: the event stays and is sent.
    fn reconcile(&self, e: &Entry) -> Result<Option<Disposition>, String> {
        match self.receipt_status(&e.id) {
            ReceiptStatus::None => Ok(None),
            ReceiptStatus::Valid(how, _) => {
                // One its engine-4 client may still read stays (see
                // `convert`, which `send_pass` runs before this).
                if self.must_keep(e, how) {
                    return Ok(Some(how));
                }
                self.cleanup(e, how);
                trace::record(
                    "outbox.reconciled",
                    json!({ "id": e.id, "store": self.label, "disposition": how.as_str() }),
                );
                Ok(Some(how))
            }
            ReceiptStatus::Invalid(why) => {
                trace::record(
                    "outbox.receipt_invalid",
                    json!({ "id": e.id, "store": self.label, "reason": why }),
                );
                Err(why)
            }
        }
    }

    /// Every readable entry, oldest first, and the names of unreadable ones.
    fn entries(&self) -> (Vec<Entry>, Vec<String>) {
        let mut out = Vec::new();
        let mut bad = Vec::new();
        for (id, path) in entry_files(&self.dir) {
            match read_entry(&path) {
                Some(entry) => out.push(entry),
                None => bad.push(format!("{id}.json")),
            }
        }
        out.sort_by_key(|e| e.created_ms);
        (out, bad)
    }

    /// A valid receipt is beside the entry: it is finished (an invalid
    /// receipt does not count).
    fn finished(&self, id: &str) -> bool {
        matches!(self.receipt_status(id), ReceiptStatus::Valid(..))
    }

    /// Entries still to be sent: all but finished and retained ones.
    fn pending(&self) -> Vec<Entry> {
        self.entries()
            .0
            .into_iter()
            .filter(|e| e.retained_as.is_none() && !self.finished(&e.id))
            .collect()
    }

    /// Undelivered entries kept in place for engine-4 clients, with how
    /// they left.
    fn retained(&self) -> Vec<(Entry, Disposition)> {
        if !self.absence_is_delivery {
            return Vec::new();
        }
        self.entries()
            .0
            .into_iter()
            .filter_map(|e| e.retained_as.map(|how| (e, how)))
            .collect()
    }

    /// Removes receipts past their TTL, except those of entries still kept
    /// beside them: without its receipt a retained entry would look pending.
    fn prune_receipts(&self) {
        let cutoff = std::time::SystemTime::now() - RECEIPT_TTL;
        for f in fs::read_dir(self.dir.join("receipts")).into_iter().flatten().flatten() {
            let name = f.file_name().to_string_lossy().into_owned();
            if name.strip_suffix(".json").is_some_and(|id| self.path_of(id).exists()) {
                continue;
            }
            if f.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < cutoff) {
                let _ = fs::remove_file(f.path());
            }
        }
    }

    /// One pass over this store under its sender lock: sends what is due
    /// (with `force`, everything the receiver has not asked to hold back),
    /// moves expired entries to `dead/`, and leaves alone any entry whose
    /// lease someone else holds.
    fn send_pass(&self, force: bool, results: &mut Vec<Value>) -> Result<(), String> {
        if !self.dir.is_dir() {
            return Ok(());
        }
        util::ensure_private_dir(&self.dir).map_err(|e| format!("cannot use the outbox directory: {e}"))?;
        // While a 0.7.0 fence stands it stays, and senders of this build
        // lock the file it moved aside; it comes down only once nothing is
        // left to hold or take back.
        let fenced = self.old_fence();
        let lock_name = if fenced { "sender.fenced.lock" } else { "sender.lock" };
        let lock =
            util::open_lock_file(&self.dir.join(lock_name)).map_err(|e| format!("cannot open the sender lock: {e}"))?;
        if !util::try_flock(&lock).map_err(|e| format!("cannot lock the outbox: {e}"))? {
            // In outbox-v2 that other sender may be an older build's: this
            // pass did not happen, which is an error, not an empty outbox.
            if fenced {
                return Err("another sender holds outbox-v2; its 0.7.0 fence stays up".into());
            }
            if self.absence_is_delivery {
                return Err("another sender holds outbox-v2; nothing there was handled".into());
            }
            results.push(json!({ "store": self.label, "skipped": "another sender is running" }));
            return Ok(());
        }
        let (held_failed, mut leases) = self.hold_first(results);
        let mut jitter = Jitter::new();
        for listed in self.entries().0 {
            if held_failed.contains(&listed.id) {
                continue;
            }
            // An entry held by `hold_first` keeps that lease through its
            // receipt removal; any other is leased here.
            let leased = match leases.remove(&listed.id) {
                Some(l) => Ok(Some(l)),
                None => self.try_lease(&listed.id),
            };
            let lease = match leased {
                Ok(Some(l)) => l,
                Ok(None) => {
                    results.push(json!({ "id": listed.id, "store": self.label, "inFlight": true,
                                         "reason": "another process is sending this entry now" }));
                    continue;
                }
                Err(e) => {
                    results.push(json!({ "id": listed.id, "store": self.label, "error": e }));
                    continue;
                }
            };
            // Reread under the lease: whoever held it may have changed it.
            let Some(mut e) = self.load(&listed.id) else {
                drop(lease);
                continue;
            };
            if let Some(how) = self.needs_conversion(&e) {
                results.push(match self.convert(&e, how) {
                    Ok(()) => json!({ "id": e.id, "store": self.label, "retained": how.as_str(),
                                      "converted": true, "reason": RETAINED }),
                    Err(why) => json!({ "id": e.id, "store": self.label, "conversionError": why,
                                        "atRisk": AT_RISK }),
                });
                continue;
            }
            // Held back as retained, without saying so: an older build
            // rewrote it (engine 5 moves retained entries into its own
            // store). Here it stays retained; anywhere else its engine-4
            // client has lost it already, and it is finished as expired.
            if e.retained_as.is_none() && e.not_before_ms == Some(RETAINED_UNTIL_MS) {
                if self.absence_is_delivery {
                    if let Err(why) = self.retain(&e, Disposition::Expired) {
                        results.push(json!({ "id": e.id, "store": self.label, "error": why }));
                        continue;
                    }
                    e = self.load(&e.id).unwrap_or(e);
                } else {
                    let mut back = e.clone();
                    back.not_before_ms = None;
                    let _ = self.finish(
                        &back,
                        Disposition::Expired,
                        e.last_status,
                        "outbox sender (held entry moved here)",
                    );
                    results.push(json!({ "id": e.id, "store": self.label, "deadLettered": true,
                                         "reason": "an entry held back for an engine-4 client, moved here by another build" }));
                    continue;
                }
            }
            if let Some(how) = e.retained_as {
                if self.client_done(&e.id) {
                    self.retire(&e, how);
                    results.push(json!({ "id": e.id, "store": self.label, "retired": how.as_str() }));
                } else {
                    results.push(json!({ "id": e.id, "store": self.label, "retained": how.as_str(),
                                         "reason": RETAINED }));
                }
                continue;
            }
            match self.reconcile(&e) {
                Ok(Some(how)) => {
                    results.push(if self.path_of(&e.id).exists() {
                        json!({ "id": e.id, "store": self.label, "retained": how.as_str(), "reason": RETAINED })
                    } else {
                        json!({ "id": e.id, "store": self.label, "reconciled": how.as_str() })
                    });
                    continue;
                }
                Ok(None) => {}
                Err(why) => results.push(json!({ "id": e.id, "store": self.label, "receiptError": why,
                                                  "note": "the receipt is not trusted; the event is kept and sent" })),
            }
            let now = util::now_ms();
            if now >= e.expires_ms {
                let finished = self.finish(&e, Disposition::Expired, e.last_status, "outbox sender");
                results.push(if let Err(why) = finished {
                    json!({ "id": e.id, "store": self.label, "error": why })
                } else if self.load(&e.id).is_some_and(|k| k.retained_as.is_some()) {
                    json!({ "id": e.id, "store": self.label, "retained": "expired", "attempts": e.attempts,
                            "reason": RETAINED })
                } else {
                    json!({ "id": e.id, "store": self.label, "deadLettered": true, "attempts": e.attempts })
                });
                continue;
            }
            if let Some(t) = e.not_before_ms.filter(|t| now < *t) {
                results.push(json!({ "id": e.id, "store": self.label, "deferred": true,
                                     "notBefore": util::rfc3339_ms(t),
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
                json!({ "id": e.id, "store": self.label, "event": e.event, "attempt": e.attempts,
                        "status": a.status, "error": a.error,
                        "retryAfterMs": a.retry_after.map(|d| d.as_millis() as u64) }),
            );
            if a.delivered {
                let _ = self.finish(&e, Disposition::Delivered, a.status, "outbox sender");
                results.push(json!({ "id": e.id, "store": self.label, "delivered": true,
                                     "attempts": e.attempts, "status": a.status }));
                continue;
            }
            e.last_error = a.error;
            e.last_status = a.status;
            // Only our own backoff is capped; the receiver's delay is a floor.
            e.not_before_ms = a.retry_after.map(|d| now + d.as_millis() as u64);
            let own = now + backoff(e.retries, &mut jitter).min(MAX_DELAY).as_millis() as u64;
            e.next_attempt_ms = own.max(e.not_before_ms.unwrap_or(0));
            let saved = self.save(&e);
            results.push(json!({
                "id": e.id,
                "store": self.label,
                "delivered": false,
                "attempts": e.attempts,
                "status": e.last_status,
                "error": e.last_error,
                "nextAttemptAt": util::rfc3339_ms(e.next_attempt_ms),
                "saveError": saved.err(),
            }));
        }
        if fenced {
            let left = self.unsettled_legacy();
            if left > 0 {
                return Err(format!(
                    "{left} outbox-v2 entr{} still to hold or take a receipt back from; the 0.7.0 fence stays up",
                    if left == 1 { "y" } else { "ies" }
                ));
            }
            self.clear_old_fence();
        }
        self.prune_receipts();
        // Lease files whose entry is gone (a crash between the two removals).
        for f in fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let name = f.file_name().to_string_lossy().into_owned();
            if let Some(id) = name.strip_suffix(".lock").filter(|id| valid_id(id)) {
                if self.load(id).is_none() {
                    if let Ok(Some(_lease)) = self.try_lease(id) {
                        if self.load(id).is_none() {
                            let _ = fs::remove_file(f.path());
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// What the delivery trace (this build's and older builds') says about
/// entries' clients: the ids whose client recorded `notify.result` (it is
/// done with the entry), and the pid that created each entry
/// (`outbox.created`, `notify.prepared`).
struct TraceScan {
    at_ms: u64,
    done: HashSet<String>,
    creators: HashMap<String, u64>,
}

static TRACE_SCAN: Mutex<Option<TraceScan>> = Mutex::new(None);

fn scan_trace() -> TraceScan {
    let mut scan = TraceScan {
        at_ms: util::now_ms(),
        done: HashSet::new(),
        creators: HashMap::new(),
    };
    let current = config::trace_path();
    let mut rotated = current.as_os_str().to_owned();
    rotated.push(".1");
    for path in [PathBuf::from(rotated), current] {
        // Never follows a symlink or blocks on a FIFO.
        let Ok(f) = util::open_private_read(&path) else {
            continue;
        };
        for line in BufReader::new(f.take(32 << 20)).lines().map_while(Result::ok) {
            let done = line.contains("\"kind\":\"notify.result\"");
            if !done && !line.contains("\"kind\":\"outbox.created\"") && !line.contains("\"kind\":\"notify.prepared\"")
            {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(id) = v.get("id").and_then(Value::as_str).filter(|id| valid_id(id)) else {
                continue;
            };
            if done {
                scan.done.insert(id.to_string());
            } else if let Some(pid) = v.get("pid").and_then(Value::as_u64) {
                scan.creators.entry(id.to_string()).or_insert(pid);
            }
        }
    }
    scan
}

fn read_entry(path: &Path) -> Option<Entry> {
    fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| Entry::from_json(&v))
}

/// Entry files (`<hex id>.json`) in an outbox directory.
fn entry_files(dir: &Path) -> Vec<(String, PathBuf)> {
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

fn backoff(attempts: u32, jitter: &mut Jitter) -> Duration {
    jitter.backoff(attempts.saturating_sub(1), BASE_DELAY, MAX_DELAY)
}

// ---- this build's outbox, as the rest of t3-watch sees it ----------------

/// Takes the entry's lease if nobody holds it. `Ok(None)`: someone does.
pub fn try_lease(id: &str) -> Result<Option<Lease>, String> {
    Store::current().try_lease(id)
}

pub fn entry_state(id: &str) -> EntryState {
    Store::current().entry_state(id)
}

pub fn receipt_status(id: &str) -> ReceiptStatus {
    Store::current().receipt_status(id)
}

pub fn finish(e: &Entry, how: Disposition, status: Option<u16>, by: &str) {
    let _ = Store::current().finish(e, how, status, by);
}

/// See `Store::reconcile`: `Ok(Some(_))` means finished, never send it.
pub fn reconcile(e: &Entry) -> Result<Option<Disposition>, String> {
    Store::current().reconcile(e)
}

pub fn entries() -> (Vec<Entry>, Vec<String>) {
    Store::current().entries()
}

/// Writes the entry before the first delivery attempt, so a crash from here
/// on loses nothing.
pub fn enqueue(w: &Webhook, event: &str, id: &str, body: &[u8], max_age: Duration) -> Result<(), String> {
    let now = util::now_ms();
    let secret = match &w.secret {
        Some(s) => Some(String::from_utf8(s.clone()).map_err(|_| "the webhook secret is not UTF-8".to_string())?),
        None => None,
    };
    Store::current().save(&Entry {
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
        retained_as: None,
        retained_expires_ms: None,
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
    let store = Store::current();
    let mut e = store.load(id).ok_or("the outbox entry disappeared")?;
    e.attempts += f.attempts;
    e.last_error = Some(f.error.clone());
    e.last_status = f.status;
    e.last_attempt_ms = Some(util::now_ms());
    e.not_before_ms = f.not_before_ms;
    e.next_attempt_ms = (util::now_ms() + BASE_DELAY.as_millis() as u64).max(f.not_before_ms.unwrap_or(0));
    store.save(&e)?;
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

/// When a leaseless legacy entry may be migrated: a client of its build
/// sends inline before this, and never treats a missing entry as delivered.
fn legacy_eligible_ms(e: &Entry) -> u64 {
    e.created_ms + INLINE_GRACE.as_millis() as u64
}

/// Moves entries builds before engine 4 left in `outbox/` into this outbox,
/// under their own sender lock so none of their senders is mid-pass, once
/// past the inline grace. Those builds' clients report delivery only on
/// their own 2xx (a missing entry is a queueing error to them), so taking
/// an entry over can at worst repeat an event under its own id.
fn migrate_v1(results: &mut Vec<Value>) {
    let legacy = config::legacy_outbox_dir();
    let files = entry_files(&legacy);
    if files.is_empty() {
        return;
    }
    let Ok(lock) = util::open_lock_file(&legacy.join("sender.lock")) else {
        return;
    };
    if !util::try_flock(&lock).unwrap_or(false) {
        return;
    }
    let current = Store::current();
    let now = util::now_ms();
    for (id, path) in files {
        let Some(mut e) = read_entry(&path) else {
            continue;
        };
        if now < legacy_eligible_ms(&e) {
            results.push(json!({ "id": id, "legacy": "outbox", "deferred": true,
                                 "eligibleAt": util::rfc3339_ms(legacy_eligible_ms(&e)),
                                 "reason": "left by an older build less than 90 s ago; its client may still be sending it" }));
            continue;
        }
        if current.load(&e.id).is_none() {
            e.next_attempt_ms = e.next_attempt_ms.min(now);
            if current.save(&e).is_err() {
                continue;
            }
        }
        let _ = fs::remove_file(&path);
        trace::record("outbox.migrated", json!({ "id": e.id, "from": "outbox" }));
        results.push(json!({ "id": e.id, "migrated": true, "from": "outbox" }));
    }
}

/// Legacy entries with the store they live in, for `outbox list`.
pub fn legacy_public_json() -> Vec<Value> {
    let mut out: Vec<Value> = entry_files(&config::legacy_outbox_dir())
        .into_iter()
        .filter_map(|(_, p)| read_entry(&p))
        .map(|e| {
            let mut v = e.public_json_in(&Store::current(), true);
            v["store"] = json!("outbox");
            v
        })
        .collect();
    let probe_v2 = !legacy_v2_blocked();
    for store in Store::leased_legacy(0) {
        let probe = probe_v2 || !store.absence_is_delivery;
        out.extend(store.pending().iter().map(|e| e.public_json_in(&store, probe)));
    }
    out
}

/// Undelivered `outbox-v2/` entries kept where engine-4 clients look, for
/// `outbox list`.
pub fn retained_public_json() -> Vec<Value> {
    let mut out = Vec::new();
    let probe = !legacy_v2_blocked();
    for store in Store::leased_legacy(0) {
        for (e, how) in store.retained() {
            let mut v = e.public_json_in(&store, probe);
            v["disposition"] = json!(how.as_str());
            v["reason"] = json!(RETAINED);
            out.push(v);
        }
    }
    out
}

/// `outbox drop`: finishes the entry as dropped, in this build's outbox or
/// an older build's leased one. `None` when there is no such entry.
pub fn drop_entry(id: &str) -> Result<Option<Value>, String> {
    if !valid_id(id) {
        return Err("not an outbox entry id".into());
    }
    let mut stores = vec![Store::current()];
    stores.extend(Store::leased_legacy(util::now_ms()));
    for store in stores {
        if !store.path_of(id).exists() {
            continue;
        }
        if store.absence_is_delivery {
            if let Some(p) = legacy_v2_precondition() {
                return Err(format!(
                    "outbox-v2 is left untouched: {}",
                    p["reason"].as_str().unwrap_or("upgrade precondition not met")
                ));
            }
        }
        let Some(lease) = store.try_lease(id)? else {
            return Err("that entry is being delivered right now; try again".into());
        };
        let Some(e) = store.load(id) else {
            continue;
        };
        let finished = store.finish(&e, Disposition::Dropped, e.last_status, "outbox drop");
        let kept = store.path_of(id).exists();
        drop(lease);
        finished?;
        return Ok(Some(
            json!({ "store": store.label, "retained": kept.then_some(RETAINED) }),
        ));
    }
    Ok(None)
}

/// Everything still to be sent, this build's entries and older builds'. The
/// daemon stays up while this is not zero.
pub fn pending_count() -> usize {
    let (legacy, _) = legacy_pending();
    Store::current().pending().len() + legacy.len()
}

/// Older builds' entries this build is to send, and how many it leaves
/// alone because `outbox-v2/` is blocked (see `legacy_v2_precondition`): a
/// blocked store is not this build's to deliver, is reported as
/// `upgradePrecondition` instead, and does not keep the daemon up.
fn legacy_pending() -> (Vec<Entry>, usize) {
    let mut out: Vec<Entry> = entry_files(&config::legacy_outbox_dir())
        .into_iter()
        .filter_map(|(_, p)| read_entry(&p))
        .collect();
    let blocked = legacy_v2_blocked();
    let [v2, v3] = Store::leased_legacy(0);
    let v2_pending = v2.pending();
    let left = if blocked {
        v2_pending.len()
    } else {
        // A receipt still to take back is work too.
        out.extend(v2_pending);
        out.extend(v2.entries().0.into_iter().filter(|e| v2.needs_conversion(e).is_some()));
        0
    };
    out.extend(v3.pending());
    (out, left)
}

/// Where an operator records that no engine-5 or engine-6 sender can run
/// beside this build (`outbox legacy-ack --senders-stopped`).
fn legacy_ack_path() -> PathBuf {
    config::state_dir().join("outbox-v2-ack.json")
}

fn legacy_acknowledged() -> bool {
    util::open_private_read(&legacy_ack_path())
        .ok()
        .and_then(|f| serde_json::from_reader::<_, Value>(f.take(64 << 10)).ok())
        .is_some_and(|v| v["sendersStopped"] == true)
}

/// Whether `outbox-v2/` holds anything this build would act on: an entry
/// file, or a 0.7.0 fence.
fn legacy_v2_has_work() -> bool {
    let store = &Store::leased_legacy(0)[0];
    store.dir.is_dir() && (!entry_files(&store.dir).is_empty() || store.old_fence())
}

/// Why this build may not touch `outbox-v2/` now, if it may not. Engine-5
/// and engine-6 senders remove entries an engine-4 client still needs (a
/// held entry moved by engine 5, one beside a receipt removed by engine 6),
/// and nothing this build writes can stop them, so it touches nothing in
/// that store until an operator acknowledges that none runs. A daemon of
/// theirs answering at a default socket refuses even then. Their absence
/// there proves nothing: an `outbox run` without a daemon, or a daemon on a
/// custom `T3_WATCH_SOCKET`, cannot be seen from here.
pub fn legacy_v2_precondition() -> Option<Value> {
    if !legacy_v2_has_work() {
        return None;
    }
    let senders = incompatible_senders();
    let action = "stop every engine-5 (0.3.0) and engine-6 (0.4.0) sender with access to this state directory \
                  (daemons, including any on a custom T3_WATCH_SOCKET, and `t3-watch outbox run` without a \
                  daemon), then run `t3-watch outbox legacy-ack --senders-stopped`";
    if !senders.is_empty() {
        return Some(json!({
            "store": "outbox-v2",
            "reason": "an engine-5 or engine-6 sender is running",
            "incompatibleSenders": senders,
            "action": action,
        }));
    }
    if !legacy_acknowledged() {
        return Some(json!({
            "store": "outbox-v2",
            "reason": "no acknowledgement that engine-5 and engine-6 senders are stopped",
            "action": action,
        }));
    }
    None
}

fn legacy_v2_blocked() -> bool {
    legacy_v2_precondition().is_some()
}

/// `outbox legacy-ack`: records (or, with `revoke`, removes) the operator's
/// statement that no engine-5 or engine-6 sender can run beside this build.
/// Refused while one answers at a default socket.
pub fn legacy_acknowledge(revoke: bool) -> Result<Value, String> {
    let path = legacy_ack_path();
    if revoke {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot remove {}: {e}", path.display())),
        }
        trace::record("outbox.legacy_ack", json!({ "revoked": true }));
        return Ok(json!({ "acknowledged": false }));
    }
    let senders = incompatible_senders();
    if !senders.is_empty() {
        return Err(format!(
            "an engine-5 or engine-6 sender is still running: {}",
            Value::Array(senders)
        ));
    }
    let ack = json!({
        "sendersStopped": true,
        "at": util::rfc3339_ms(util::now_ms()),
        "statement": "no engine-5 (0.3.0) or engine-6 (0.4.0) sender, daemon or `outbox run`, runs with access \
                      to this state directory",
    });
    util::ensure_private_dir(&config::state_dir()).map_err(|e| format!("cannot prepare the state directory: {e}"))?;
    util::write_private(&path, ack.to_string().as_bytes()).map_err(|e| format!("cannot record it: {e}"))?;
    trace::record("outbox.legacy_ack", json!({ "revoked": false }));
    Ok(json!({ "acknowledged": true, "ack": ack }))
}

/// Receipts in `outbox-v2/` still to be taken back from beside entries held
/// for engine-4 clients (see `Store::convert`).
fn legacy_conversions_incomplete() -> usize {
    Store::leased_legacy(0)
        .iter()
        .map(|store| {
            store
                .entries()
                .0
                .iter()
                .filter(|e| store.needs_conversion(e).is_some())
                .count()
        })
        .sum()
}

/// One pass over every outbox: migrates what may be migrated from
/// `outbox/`, delivers older leased stores in place, then this build's.
/// Returns per-entry results.
pub fn pass(force: bool) -> Result<Vec<Value>, String> {
    let mut results = Vec::new();
    migrate_v1(&mut results);
    let now = util::now_ms();
    let fresh_after = if force {
        now
    } else {
        now.saturating_sub(RETIRE_RESCAN.as_millis() as u64)
    };
    for store in Store::leased_legacy(fresh_after) {
        if store.absence_is_delivery {
            // Nothing in outbox-v2 is touched (no lock file, hold, receipt
            // or fence change) unless it has something to act on and the
            // upgrade precondition holds. Checked in that order, so an
            // entry that appears after an empty look waits for the next
            // pass and its own check.
            if !legacy_v2_has_work() {
                continue;
            }
            if let Some(p) = legacy_v2_precondition() {
                results.push(json!({ "store": store.label, "upgradePrecondition": p }));
                continue;
            }
        }
        if let Err(e) = store.send_pass(force, &mut results) {
            results.push(json!({ "store": store.label, "error": e }));
        }
    }
    Store::current().send_pass(force, &mut results)?;
    Ok(results)
}

/// The supervised sender: passes forever, every couple of seconds. Logs a
/// line per attempt through `log` (never URLs or secrets).
pub fn run_forever(log: &dyn Fn(&str)) -> ! {
    let mut refused: Option<String> = None;
    loop {
        match pass(false) {
            Ok(results) => {
                let now = results
                    .iter()
                    .find_map(|r| r.get("upgradePrecondition"))
                    .map(Value::to_string);
                if now != refused {
                    if let Some(p) = &now {
                        log(&format!("outbox: outbox-v2 left untouched: {p}"));
                    }
                    refused = now;
                }
                for r in results.iter().filter(|r| {
                    r.get("id").is_some()
                        && r.get("deferred").is_none()
                        && r.get("inFlight").is_none()
                        && r.get("retained").is_none()
                }) {
                    log(&format!("outbox: {r}"));
                }
            }
            Err(e) => log(&format!("outbox: {e}")),
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Older senders whose own rules can remove an `outbox-v2/` entry an
/// engine-4 client still needs, which no state this build writes can stop:
/// engine 5 (`0.3.0`, never released) moves such entries whatever their
/// state, minding only the entry lease; engine 6 (`0.4.0`) removes one that
/// has a receipt beside it (`legacyConversionsIncomplete`). Found when a
/// daemon of theirs is alive at a default socket (holding its socket lock,
/// or accepting or queueing connections, see `util::daemon_alive`): where it would listen in
/// this environment (`$XDG_RUNTIME_DIR/t3-watch/` or the state directory),
/// and the state directory as well (a daemon started without
/// `$XDG_RUNTIME_DIR`). A daemon on a custom `T3_WATCH_SOCKET`, or an
/// `outbox run` without a daemon, is not found this way.
pub fn incompatible_senders() -> Vec<Value> {
    let risks = [
        (
            5,
            "moves outbox-v2 entries an engine-4 client may still look for, so that client would report them delivered",
        ),
        (
            6,
            "removes an outbox-v2 entry that has a receipt beside it, so its engine-4 client would report it delivered",
        ),
    ];
    let mut out = Vec::new();
    for (engine, risk) in risks {
        let mut places = vec![
            config::default_socket_path(engine),
            config::state_dir().join(format!("daemon-e{engine}.sock")),
        ];
        places.dedup();
        for sock in places {
            if util::daemon_alive(&sock) {
                out.push(json!({
                    "engine": engine,
                    "socket": sock.display().to_string(),
                    "risk": risk,
                    "action": "stop it with its own binary's `t3-watch stop` while engine-4 clients may be running",
                }));
            }
        }
    }
    out
}

pub fn summary_json() -> Value {
    let store = Store::current();
    // Both counts come from one inventory: entries written while this runs
    // are in it or not, never counted as pending but not as entries.
    let (entries, bad) = store.entries();
    let pending: Vec<&Entry> = entries.iter().filter(|e| !store.finished(&e.id)).collect();
    let (legacy, blocked) = legacy_pending();
    let oldest = pending.iter().copied().chain(&legacy).map(|e| e.created_ms).min();
    json!({
        "pending": pending.len() + legacy.len(),
        "legacyPending": legacy.len(),
        "legacyBlocked": blocked,
        "legacyRetained": retained_public_json().len(),
        "legacyConversionsIncomplete": legacy_conversions_incomplete(),
        "upgradePrecondition": legacy_v2_precondition(),
        "incompatibleSenders": incompatible_senders(),
        "finishedAwaitingCleanup": entries.len() - pending.len(),
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
            retained_as: None,
            retained_expires_ms: None,
        };
        assert_eq!(Entry::from_json(&e.to_json()), Some(e.clone()));
        let kept = Entry {
            retained_as: Some(Disposition::Dropped),
            retained_expires_ms: Some(2),
            ..e.clone()
        };
        assert_eq!(Entry::from_json(&kept.to_json()), Some(kept));
        let public = e.public_json().to_string();
        for secret in ["s3cr3t-path", "token=abc", "Bearer xyz", "signing-key"] {
            assert!(!public.contains(secret), "{public}");
        }
        assert!(public.contains("https://hooks.example"));
        assert!(!valid_id("../x") && !valid_id("") && valid_id("abc123"));
    }
}
