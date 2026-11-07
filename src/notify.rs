//! Wake notifications for a finished wait, run by the waiting client (never
//! by the shared daemon): an HTTPS webhook and/or a local program. With
//! `--durable`, a webhook that cannot be delivered within its inline budget
//! is kept in the outbox and retried by the daemon until it gets a 2xx (see
//! `outbox`).
//!
//! Secrets stay out of argv and out of output. The webhook URL, extra header
//! values and the signing secret come from environment variables or files;
//! they are read once, before the daemon or any hook is started, and the
//! variables are removed from this process so no child inherits them.
//! Results report attempts, HTTP status and an error category only, never a
//! URL, header, key or response body.

use crate::outbox;
use crate::util::{self, Jitter};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const WEBHOOK_BUDGET: Duration = Duration::from_secs(60);
pub const WEBHOOK_ATTEMPTS: u32 = 4;
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_SECRET_FILE: u64 = 64 * 1024;
const MAX_HEADER_VALUE: usize = 8 * 1024;
pub const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(60);

/// Notification classes for `--notify-on`. Final results are `settled`
/// (completed or failed), `needs_input`, `timeout` and `error`; progress is
/// `run_failed` (`--follow`) and `upstream` (`--notify-unreachable`).
pub const CLASSES: &[&str] = &["settled", "needs_input", "timeout", "error", "run_failed", "upstream"];
/// Without `--notify-on`: everything except timeout and error.
const DEFAULT_CLASSES: &[&str] = &["settled", "needs_input", "run_failed", "upstream"];

/// The `--notify-on` class an event belongs to.
fn class_of(event: &str) -> &str {
    match event {
        "upstream_unreachable" | "upstream_restored" => "upstream",
        other => other,
    }
}

#[derive(Clone)]
pub struct Webhook {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub secret: Option<Vec<u8>>,
}

pub struct Hook {
    program: String,
    args: Vec<String>,
    timeout: Duration,
    /// Where the hook's output goes (`hooks.log` in the state directory).
    log: std::path::PathBuf,
}

pub struct Notify {
    webhook: Option<Webhook>,
    hook: Option<Hook>,
    on: Vec<&'static str>,
    /// Keep undelivered webhooks in the outbox until this long after they
    /// were created. `None`: no outbox, inline attempts only.
    durable: Option<Duration>,
}

/// The notification options as given on the command line.
#[derive(Default)]
pub struct NotifyArgs {
    pub webhook_url: Option<String>,
    pub webhook_url_env: Option<String>,
    pub webhook_url_file: Option<String>,
    pub header_envs: Vec<String>,
    pub secret_env: Option<String>,
    pub secret_file: Option<String>,
    pub on_settle: Option<String>,
    pub on_settle_args: Vec<String>,
    pub on_settle_timeout: Option<Duration>,
    pub notify_on: Option<String>,
    pub durable: bool,
    pub durable_max_age: Option<Duration>,
}

fn read_env(var: &str, what: &str) -> Result<String, String> {
    if var.is_empty() || var.contains('=') || util::has_control_chars(var) {
        return Err(format!("{what}: invalid environment variable name"));
    }
    let v = std::env::var(var).map_err(|_| format!("{what}: environment variable {var} is not set"))?;
    // Keep it out of every process this one starts from now on.
    std::env::remove_var(var);
    Ok(v)
}

/// Reads a secret (a webhook URL or signing key) from a file the user
/// controls: after opening, the file must be a regular file owned by this
/// user with no group or other permissions. Symlinks are followed, but the
/// file they lead to is what gets checked. `O_NONBLOCK` keeps a FIFO from
/// hanging option processing before any wait deadline applies.
fn read_secret_file(path: &str, what: &str) -> Result<String, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let p = Path::new(path);
    let mut f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(p)
        .map_err(|e| format!("{what}: cannot open {}: {}", p.display(), e.kind()))?;
    let meta = f
        .metadata()
        .map_err(|e| format!("{what}: cannot inspect {}: {}", p.display(), e.kind()))?;
    if !meta.is_file() {
        return Err(format!("{what}: {} is not a regular file", p.display()));
    }
    use std::os::unix::fs::MetadataExt;
    if meta.uid() != util::geteuid() {
        return Err(format!(
            "{what}: {} is owned by uid {}, not by you",
            p.display(),
            meta.uid()
        ));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{what}: {} is readable by other users (mode {:o}); chmod 600 it",
            p.display(),
            meta.mode() & 0o777
        ));
    }
    let mut s = String::new();
    Read::by_ref(&mut f)
        .take(MAX_SECRET_FILE + 1)
        .read_to_string(&mut s)
        .map_err(|_| format!("{what}: {} is not readable UTF-8", p.display()))?;
    if s.len() as u64 > MAX_SECRET_FILE {
        return Err(format!(
            "{what}: {} is larger than {MAX_SECRET_FILE} bytes",
            p.display()
        ));
    }
    Ok(s.trim().to_string())
}

/// RFC 9110 token characters.
fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

const RESERVED_HEADERS: &[&str] = &[
    "idempotency-key",
    "host",
    "content-length",
    "content-type",
    "transfer-encoding",
    "connection",
    "user-agent",
];

fn header_value_ok(v: &str) -> bool {
    !v.is_empty() && v.len() <= MAX_HEADER_VALUE && !v.chars().any(|c| c.is_control() && c != '\t')
}

impl Notify {
    /// Validates the options and reads every referenced secret now.
    pub fn from_args(a: &NotifyArgs) -> Result<Notify, String> {
        let on: Vec<&'static str> = match a.notify_on.as_deref() {
            None => DEFAULT_CLASSES.to_vec(),
            Some("all") => CLASSES.to_vec(),
            Some(list) => {
                let mut on = Vec::new();
                for c in list.split(',').map(str::trim).filter(|c| !c.is_empty()) {
                    let Some(known) = CLASSES.iter().find(|k| **k == c) else {
                        return Err(format!(
                            "--notify-on takes `all` or a comma list of {}",
                            CLASSES.join(", ")
                        ));
                    };
                    on.push(*known);
                }
                if on.is_empty() {
                    return Err("--notify-on needs at least one event class".into());
                }
                on
            }
        };
        let sources = [&a.webhook_url, &a.webhook_url_env, &a.webhook_url_file]
            .iter()
            .filter(|s| s.is_some())
            .count();
        if sources > 1 {
            return Err("pass only one of --webhook-url, --webhook-url-env and --webhook-url-file".into());
        }
        let url = if let Some(u) = &a.webhook_url {
            Some(u.trim().to_string())
        } else if let Some(var) = &a.webhook_url_env {
            Some(read_env(var, "--webhook-url-env")?.trim().to_string())
        } else if let Some(path) = &a.webhook_url_file {
            Some(read_secret_file(path, "--webhook-url-file")?)
        } else {
            None
        };
        let has_extras = !a.header_envs.is_empty() || a.secret_env.is_some() || a.secret_file.is_some();
        if url.is_none() && has_extras {
            return Err("--webhook-header-env and --webhook-secret-* need a webhook URL".into());
        }
        let webhook = match url {
            None => None,
            Some(url) => {
                // The message never repeats the URL: it may embed a secret.
                util::secure_url(&url, "webhook URL")?;
                let mut headers = Vec::new();
                for spec in &a.header_envs {
                    let (name, var) = spec
                        .split_once('=')
                        .ok_or("--webhook-header-env takes HEADER=ENV_VAR")?;
                    if !is_header_name(name) {
                        return Err("--webhook-header-env: invalid header name".into());
                    }
                    let lower = name.to_ascii_lowercase();
                    if RESERVED_HEADERS.contains(&lower.as_str()) || lower.starts_with("x-t3-watch-") {
                        return Err(format!("--webhook-header-env: {name} is set by t3-watch itself"));
                    }
                    let value = read_env(var, "--webhook-header-env")?;
                    if !header_value_ok(&value) {
                        return Err(format!(
                            "--webhook-header-env: the value of {var} is empty, too long or contains CR, LF or NUL"
                        ));
                    }
                    headers.push((name.to_string(), value));
                }
                if a.secret_env.is_some() && a.secret_file.is_some() {
                    return Err("pass only one of --webhook-secret-env and --webhook-secret-file".into());
                }
                let secret = match (&a.secret_env, &a.secret_file) {
                    (Some(var), _) => Some(read_env(var, "--webhook-secret-env")?),
                    (_, Some(path)) => Some(read_secret_file(path, "--webhook-secret-file")?),
                    _ => None,
                };
                if secret.as_deref() == Some("") {
                    return Err("the webhook secret is empty".into());
                }
                Some(Webhook {
                    url,
                    headers,
                    secret: secret.map(String::into_bytes),
                })
            }
        };
        if a.on_settle.is_none() && !a.on_settle_args.is_empty() {
            return Err("--on-settle-arg needs --on-settle".into());
        }
        let hook = a.on_settle.as_ref().map(|program| Hook {
            program: program.clone(),
            args: a.on_settle_args.clone(),
            timeout: a.on_settle_timeout.unwrap_or(DEFAULT_HOOK_TIMEOUT),
            log: crate::config::state_dir().join("hooks.log"),
        });
        if hook
            .as_ref()
            .is_some_and(|h| h.program.is_empty() || util::has_control_chars(&h.program))
        {
            return Err("--on-settle needs a program path".into());
        }
        if a.durable && webhook.is_none() {
            return Err("--durable keeps webhook deliveries; it needs a webhook URL".into());
        }
        if a.durable_max_age.is_some() && !a.durable {
            return Err("--durable-max-age needs --durable".into());
        }
        let durable = a.durable.then(|| a.durable_max_age.unwrap_or(outbox::DEFAULT_MAX_AGE));
        Ok(Notify {
            webhook,
            hook,
            on,
            durable,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.webhook.is_none() && self.hook.is_none()
    }

    pub fn has_webhook(&self) -> bool {
        self.webhook.is_some()
    }

    pub fn applies_to(&self, event: &str) -> bool {
        self.on.contains(&class_of(event))
    }

    /// Delivers `body` (the result JSON) for `event`. Returns `None` when
    /// nothing was configured for this event; otherwise a JSON summary with
    /// an overall `delivered` flag. The webhook and the hook run in parallel.
    /// With `--durable` the webhook is written to the outbox first, so a
    /// crash during delivery loses nothing, and stays there until a 2xx.
    pub fn deliver(&self, event: &str, body: &[u8]) -> Option<Value> {
        if self.is_empty() || !self.applies_to(event) {
            return None;
        }
        let id = util::hex(&util::random_bytes(16));
        let queued = match (&self.webhook, self.durable) {
            (Some(w), Some(max_age)) => Some(outbox::enqueue(w, event, &id, body, max_age)),
            _ => None,
        };
        let (webhook, hook) = thread::scope(|scope| {
            let web = self.webhook.as_ref().map(|w| {
                let id = id.as_str();
                thread::Builder::new()
                    .name("webhook".into())
                    .spawn_scoped(scope, move || post_webhook(w, event, id, body))
            });
            let hook = self.hook.as_ref().map(|h| run_hook(h, event, body));
            let web = web.map(|spawned| match spawned {
                Ok(handle) => handle
                    .join()
                    .unwrap_or_else(|_| json!({"delivered": false, "error": "webhook thread panicked"})),
                // No thread to spare: deliver on this one.
                Err(_) => post_webhook(self.webhook.as_ref().unwrap(), event, &id, body),
            });
            (web, hook)
        });
        let mut webhook = webhook;
        if let (Some(queued), Some(w)) = (queued, webhook.as_mut()) {
            match queued {
                Ok(()) if w["delivered"] == true => outbox::remove(&id),
                Ok(()) => {
                    let attempts = w["attempts"].as_u64().unwrap_or(0) as u32;
                    let error = w["error"].as_str().unwrap_or("not delivered").to_string();
                    let status = w["status"].as_u64().map(|s| s as u16);
                    match outbox::after_inline_failure(&id, attempts, &error, status) {
                        Ok(()) => {
                            w["queued"] = json!(true);
                            w["outboxId"] = json!(id);
                        }
                        Err(e) => w["queueError"] = json!(e),
                    }
                }
                Err(e) => w["queueError"] = json!(e),
            }
        }
        let ok = |v: &Option<Value>| v.as_ref().is_none_or(|v| v["delivered"] == true);
        let mut out = json!({ "delivered": ok(&webhook) && ok(&hook), "id": id });
        if webhook.as_ref().is_some_and(|w| w["queued"] == true) {
            out["queued"] = json!(true);
        }
        if let Some(w) = webhook {
            out["webhook"] = w;
        }
        if let Some(h) = hook {
            out["onSettle"] = h;
        }
        Some(out)
    }

    /// One best-effort heartbeat POST (no retries, never queued): a missing
    /// heartbeat is the signal a receiver-side monitor looks for.
    pub fn heartbeat(&self, id: &str, body: &[u8]) -> bool {
        let Some(w) = &self.webhook else {
            return false;
        };
        post_once(&agent_for(w), w, "heartbeat", id, body, Duration::from_secs(10), 1).delivered
    }
}

/// `t=<unix secs>`-style signature over `<ts>.<id>.<raw body>`.
pub fn signature(secret: &[u8], ts: u64, id: &str, body: &[u8]) -> String {
    let mut msg = format!("{ts}.{id}.").into_bytes();
    msg.extend_from_slice(body);
    format!("sha256={}", util::hex(&util::hmac_sha256(secret, &msg)))
}

/// `Retry-After` in delta-seconds. HTTP dates are ignored (backoff applies).
fn retry_after(resp: &ureq::http::Response<ureq::Body>) -> Option<Duration> {
    resp.headers()
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

pub fn agent_for(w: &Webhook) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .https_only(w.url.starts_with("https://"))
        .user_agent(concat!("t3-watch/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// What one POST attempt got.
pub struct Attempt {
    pub delivered: bool,
    pub status: Option<u16>,
    pub error: Option<String>,
    pub retry_after: Option<Duration>,
    /// Redirects and most 4xx answers will not change by retrying inline.
    pub permanent: bool,
}

/// One signed POST. The same `id` goes out on every attempt as
/// `X-T3-Watch-Id` and `Idempotency-Key`, so a receiver can drop
/// duplicates (a retry after a lost response, or the outbox resending after
/// a crash). The timestamp and signature are fresh for each attempt.
pub fn post_once(
    agent: &ureq::Agent,
    w: &Webhook,
    event: &str,
    id: &str,
    body: &[u8],
    timeout: Duration,
    attempt: u32,
) -> Attempt {
    let ts = util::now_ms() / 1000;
    let mut req = agent
        .post(&w.url)
        .header("Content-Type", "application/json")
        .header("X-T3-Watch-Event", event)
        .header("X-T3-Watch-Id", id)
        .header("Idempotency-Key", id)
        .header("X-T3-Watch-Attempt", attempt.to_string());
    if let Some(secret) = &w.secret {
        req = req
            .header("X-T3-Watch-Timestamp", ts.to_string())
            .header("X-T3-Watch-Signature", signature(secret, ts, id, body));
    }
    for (k, v) in &w.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    match req.config().timeout_global(Some(timeout)).build().send(body) {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let retry_after = retry_after(&resp);
            // Drain a little so the connection closes cleanly; never keep it.
            let _ = resp
                .into_body()
                .into_reader()
                .take(64 * 1024)
                .read_to_end(&mut Vec::new());
            let (delivered, permanent, error) = match status {
                200..=299 => (true, false, None),
                408 | 429 | 500..=599 => (false, false, Some(format!("HTTP {status}"))),
                300..=399 => (false, true, Some(format!("HTTP {status}: redirects are not followed"))),
                _ => (false, true, Some(format!("HTTP {status} (not retried inline)"))),
            };
            Attempt {
                delivered,
                status: Some(status),
                error,
                retry_after,
                permanent,
            }
        }
        Err(e) => Attempt {
            delivered: false,
            status: None,
            error: Some(util::describe_ureq_error(&e)),
            retry_after: None,
            permanent: false,
        },
    }
}

fn post_webhook(w: &Webhook, event: &str, id: &str, body: &[u8]) -> Value {
    let agent = agent_for(w);
    let budget_end = Instant::now() + WEBHOOK_BUDGET;
    let mut jitter = Jitter::new();
    let mut attempts = 0;
    let mut last_error = String::from("no attempt fit in the time budget");
    let mut last_status: Option<u16> = None;
    while attempts < WEBHOOK_ATTEMPTS {
        let remaining = budget_end.saturating_duration_since(Instant::now());
        if remaining < Duration::from_millis(500) {
            break;
        }
        attempts += 1;
        let a = post_once(&agent, w, event, id, body, remaining.min(ATTEMPT_TIMEOUT), attempts);
        last_status = a.status.or(last_status);
        if a.delivered {
            return json!({ "delivered": true, "attempts": attempts, "status": a.status });
        }
        last_error = a.error.unwrap_or_default();
        if a.permanent {
            let mut out = json!({ "delivered": false, "attempts": attempts, "error": last_error });
            if let Some(s) = a.status {
                out["status"] = json!(s);
            }
            return out;
        }
        if attempts >= WEBHOOK_ATTEMPTS {
            break;
        }
        let delay = a
            .retry_after
            .unwrap_or_else(|| jitter.backoff(attempts - 1, Duration::from_secs(1), Duration::from_secs(8)));
        let remaining = budget_end.saturating_duration_since(Instant::now());
        if delay >= remaining {
            last_error = format!("{last_error}; the next retry would exceed the 60 s budget");
            break;
        }
        thread::sleep(delay);
    }
    let mut out = json!({ "delivered": false, "attempts": attempts, "error": last_error });
    if let Some(s) = last_status {
        out["status"] = json!(s);
    }
    out
}

fn status_json(status: ExitStatus) -> Value {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(0), _) => json!({ "delivered": true, "exitCode": 0 }),
        (Some(c), _) => json!({ "delivered": false, "exitCode": c }),
        (None, Some(sig)) => json!({ "delivered": false, "signal": sig }),
        _ => json!({ "delivered": false, "error": "unknown exit status" }),
    }
}

/// Where hook output goes: `hooks.log` in the state directory (0600),
/// appended. Never our stdout (the result JSON) and never our stderr: a
/// hook that leaves a background process holding an inherited pipe would
/// keep whoever reads our output (an agent harness) waiting long after
/// t3-watch has exited.
fn hook_log(path: &Path) -> Stdio {
    if fs::metadata(path).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = fs::remove_file(path);
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    let open = util::ensure_private_dir(dir).and_then(|_| {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
    });
    open.map(Stdio::from).unwrap_or_else(|_| Stdio::null())
}

/// Runs the hook with the result JSON on stdin and its output going to
/// `hooks.log`. No shell is involved: `program` and its arguments are
/// passed as they are.
fn run_hook(h: &Hook, event: &str, body: &[u8]) -> Value {
    let mut child = match Command::new(&h.program)
        .args(&h.args)
        .env("T3_WATCH_EVENT", event)
        .stdin(Stdio::piped())
        .stdout(hook_log(&h.log))
        .stderr(hook_log(&h.log))
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return json!({ "delivered": false, "error": format!("cannot start the hook: {}", e.kind()) }),
    };
    let stdin = child.stdin.take();
    let payload = body.to_vec();
    // A hook that does not read stdin must not block us: write from a thread.
    let writer = util::spawn_thread("hook-stdin", 64 * 1024, move || {
        if let Some(mut s) = stdin {
            let _ = s.write_all(&payload);
        }
    });
    if writer.is_err() {
        crate::local::kill_and_reap(&mut child);
        return json!({ "delivered": false, "error": "cannot start a thread for the hook's stdin" });
    }
    let deadline = Instant::now() + h.timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status_json(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                crate::local::kill_and_reap(&mut child);
                return json!({ "delivered": false, "timedOut": true,
                               "error": format!("the hook ran longer than {} s and was killed", h.timeout.as_secs_f64()) });
            }
            Err(e) => {
                crate::local::kill_and_reap(&mut child);
                return json!({ "delivered": false, "error": format!("waiting for the hook: {}", e.kind()) });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> NotifyArgs {
        NotifyArgs::default()
    }

    #[test]
    fn signature_is_hmac_over_ts_id_body() {
        let sig = signature(b"key", 1700000000, "abc", br#"{"event":"settled"}"#);
        let expected = util::hex(&util::hmac_sha256(b"key", br#"1700000000.abc.{"event":"settled"}"#));
        assert_eq!(sig, format!("sha256={expected}"));
    }

    #[test]
    fn webhook_urls_must_be_https_off_loopback() {
        let mut a = args();
        a.webhook_url = Some("http://hooks.example/x".into());
        let e = Notify::from_args(&a).err().unwrap();
        assert!(e.contains("https") && !e.contains("hooks.example"), "{e}");
        a.webhook_url = Some("http://127.0.0.1:9/hook".into());
        assert!(Notify::from_args(&a).is_ok());
        a.webhook_url = Some("https://hooks.example/x\r\nX: y".into());
        assert!(Notify::from_args(&a).is_err());
    }

    #[test]
    fn header_and_secret_options_are_validated() {
        let mut a = args();
        a.header_envs = vec!["Authorization=T3W_UNIT_H".into()];
        assert!(Notify::from_args(&a).is_err(), "headers need a URL");
        a.webhook_url = Some("https://hooks.example/x".into());
        a.header_envs = vec!["Bad Name=T3W_UNIT_H".into()];
        assert!(Notify::from_args(&a).err().unwrap().contains("header name"));
        a.header_envs = vec!["Content-Length=T3W_UNIT_H".into()];
        assert!(Notify::from_args(&a).err().unwrap().contains("set by t3-watch"));
        a.header_envs = vec!["X-T3-Watch-Id=T3W_UNIT_H".into()];
        assert!(Notify::from_args(&a).is_err());
        a.header_envs = vec!["Idempotency-Key=T3W_UNIT_H".into()];
        assert!(Notify::from_args(&a).is_err());
        a.header_envs = vec!["Authorization=T3W_UNIT_UNSET".into()];
        assert!(Notify::from_args(&a).err().unwrap().contains("not set"));
        a.header_envs = vec![];
        a.secret_env = Some("A".into());
        a.secret_file = Some("/x".into());
        assert!(Notify::from_args(&a).is_err());
        assert!(!header_value_ok("a\r\nb") && !header_value_ok("a\0") && header_value_ok("Bearer x\ty"));
    }

    #[test]
    fn hook_args_need_a_hook_and_notify_on_is_checked() {
        let mut a = args();
        a.on_settle_args = vec!["x".into()];
        assert!(Notify::from_args(&a).is_err());
        let mut a = args();
        a.notify_on = Some("sometimes".into());
        assert!(Notify::from_args(&a).is_err());
        let mut a = args();
        a.on_settle = Some("/bin/true".into());
        let n = Notify::from_args(&a).unwrap();
        assert!(n.applies_to("settled") && n.applies_to("needs_input") && n.applies_to("run_failed"));
        assert!(n.applies_to("upstream_unreachable") && !n.applies_to("timeout") && !n.applies_to("error"));
        a.notify_on = Some("all".into());
        let n = Notify::from_args(&a).unwrap();
        assert!(n.applies_to("timeout") && n.applies_to("error") && n.applies_to("settled"));
        a.notify_on = Some("needs_input, error".into());
        let n = Notify::from_args(&a).unwrap();
        assert!(n.applies_to("needs_input") && n.applies_to("error") && !n.applies_to("settled"));
        a.notify_on = Some(",".into());
        assert!(Notify::from_args(&a).is_err());
        let mut a = args();
        a.durable = true;
        assert!(Notify::from_args(&a).err().unwrap().contains("webhook URL"));
        let none = Notify::from_args(&args()).unwrap();
        assert!(none.is_empty());
        assert!(
            none.deliver("settled", b"{}").is_none(),
            "nothing configured, nothing sent"
        );
    }

    #[test]
    fn hook_gets_stdin_and_is_killed_on_timeout() {
        let dir = std::env::temp_dir().join(format!(
            "t3wn-{}-{}",
            std::process::id(),
            util::hex(&util::random_bytes(4))
        ));
        fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out");
        let ok = Hook {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), format!("cat > '{}'", out.display())],
            timeout: Duration::from_secs(5),
            log: dir.join("hooks.log"),
        };
        assert_eq!(run_hook(&ok, "settled", b"{\"a\":1}")["delivered"], true);
        assert_eq!(fs::read_to_string(&out).unwrap(), "{\"a\":1}");
        let slow = Hook {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
            timeout: Duration::from_millis(200),
            log: dir.join("hooks.log"),
        };
        let started = Instant::now();
        let v = run_hook(&slow, "settled", b"{}");
        assert_eq!(v["timedOut"], true);
        assert!(started.elapsed() < Duration::from_secs(5));
        let failing = Hook {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "echo to-the-log; echo err-to-the-log >&2; exit 7".into()],
            timeout: Duration::from_secs(5),
            log: dir.join("hooks.log"),
        };
        assert_eq!(run_hook(&failing, "settled", b"{}")["exitCode"], 7);
        let log = fs::read_to_string(dir.join("hooks.log")).unwrap();
        assert!(log.contains("to-the-log") && log.contains("err-to-the-log"), "{log}");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(dir.join("hooks.log")).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let missing = Hook {
            program: "/no/such/hook".into(),
            args: vec![],
            timeout: Duration::from_secs(5),
            log: dir.join("hooks.log"),
        };
        assert_eq!(run_hook(&missing, "settled", b"{}")["delivered"], false);
        fs::remove_dir_all(dir).unwrap();
    }
}
