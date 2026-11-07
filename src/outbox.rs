//! Durable webhook delivery (`--durable`).
//!
//! Before the first attempt, the notification is written to
//! `<state>/outbox/<id>.json` (0600 in a 0700 directory, fsynced, renamed
//! into place). The waiting client then tries it inline. A 2xx removes the
//! entry; anything else leaves it for the sender, which runs inside the
//! daemon (or as `t3-watch outbox run`) and retries with exponential
//! backoff, capped at five minutes, until a 2xx or the entry's expiry, when
//! it moves to `outbox/dead/`. One sender at a time holds `outbox/sender.lock`.
//! A server's `Retry-After` is a floor, not a suggestion: it is stored as the
//! entry's `notBefore` (across the hand-off from the client to the sender
//! too) and neither the backoff cap nor `outbox run --once` sends earlier.
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
use crate::util::{self, Jitter};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
const BASE_DELAY: Duration = Duration::from_secs(2);
const MAX_DELAY: Duration = Duration::from_secs(300);
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);
/// While the client makes its inline attempts the sender leaves the entry
/// alone; if the client dies, the sender takes over after this.
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

fn save(e: &Entry) -> Result<(), String> {
    util::ensure_private_dir(&config::state_dir())
        .map_err(|err| format!("cannot prepare the state directory: {err}"))?;
    util::write_private(&path_of(&e.id), e.to_json().to_string().as_bytes())
        .map_err(|err| format!("cannot write outbox entry: {err}"))
}

fn load(id: &str) -> Option<Entry> {
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
    })
}

/// The inline attempts failed: hand the entry to the sender right away.
pub fn after_inline_failure(id: &str, f: &InlineFailure) -> Result<(), String> {
    let mut e = load(id).ok_or("the outbox entry disappeared")?;
    e.attempts += f.attempts;
    e.last_error = Some(f.error.clone());
    e.last_status = f.status;
    e.not_before_ms = f.not_before_ms;
    e.next_attempt_ms = (util::now_ms() + BASE_DELAY.as_millis() as u64).max(f.not_before_ms.unwrap_or(0));
    save(&e)
}

pub fn remove(id: &str) {
    if valid_id(id) {
        let _ = fs::remove_file(path_of(id));
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

pub fn pending_count() -> usize {
    entries().0.len()
}

fn dead_letter(e: &Entry) {
    let dead = dir().join("dead");
    if util::ensure_private_dir(&dead).is_ok() {
        let _ = fs::rename(path_of(&e.id), dead.join(format!("{}.json", e.id)));
    }
}

fn backoff(attempts: u32, jitter: &mut Jitter) -> Duration {
    jitter.backoff(attempts.saturating_sub(1), BASE_DELAY, MAX_DELAY)
}

/// One pass over the outbox: sends what is due (with `force`, everything
/// the receiver has not asked to hold back with `Retry-After`), moves
/// expired entries to `dead/`. Returns per-entry results. Holds the sender
/// lock for the pass; if another sender has it, does nothing.
pub fn pass(force: bool) -> Result<Vec<Value>, String> {
    if !dir().is_dir() {
        return Ok(Vec::new());
    }
    util::ensure_private_dir(&dir()).map_err(|e| format!("cannot use the outbox directory: {e}"))?;
    let lock =
        util::open_lock_file(&dir().join("sender.lock")).map_err(|e| format!("cannot open the sender lock: {e}"))?;
    if !util::try_flock(&lock).map_err(|e| format!("cannot lock the outbox: {e}"))? {
        return Ok(vec![json!({ "skipped": "another sender is running" })]);
    }
    let mut jitter = Jitter::new();
    let mut results = Vec::new();
    for mut e in entries().0 {
        let now = util::now_ms();
        if now >= e.expires_ms {
            dead_letter(&e);
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
        if a.delivered {
            remove(&e.id);
            results.push(json!({ "id": e.id, "delivered": true, "attempts": e.attempts, "status": a.status }));
            continue;
        }
        e.last_error = a.error;
        e.last_status = a.status;
        // Only our own backoff is capped; the receiver's delay is a floor.
        let now = util::now_ms();
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
                    .filter(|r| r.get("id").is_some() && r.get("deferred").is_none())
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
    json!({
        "pending": entries.len(),
        "unreadable": bad.len(),
        "oldestCreatedAt": entries.first().map(|e| util::rfc3339_ms(e.created_ms)),
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
