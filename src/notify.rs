//! Wake notifications for a finished wait, run by the waiting client (never
//! by the shared daemon): an HTTPS webhook and/or a local program.
//!
//! Secrets stay out of argv and out of output. The webhook URL, extra header
//! values and the signing secret come from environment variables or files;
//! they are read once, before the daemon or any hook is started, and the
//! variables are removed from this process so no child inherits them.
//! Results report attempts, HTTP status and an error category only, never a
//! URL, header, key or response body.

use crate::util::{self, Jitter};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsFd;
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NotifyOn {
    /// Only when the threads settled (default).
    Settled,
    /// Also on timeout and error, so an expired token cannot strand an
    /// unattended bot without a wake-up.
    All,
}

pub struct Webhook {
    url: String,
    headers: Vec<(String, String)>,
    secret: Option<Vec<u8>>,
}

pub struct Hook {
    program: String,
    args: Vec<String>,
    timeout: Duration,
}

pub struct Notify {
    webhook: Option<Webhook>,
    hook: Option<Hook>,
    on: NotifyOn,
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

fn read_secret_file(path: &str, what: &str) -> Result<String, String> {
    let p = Path::new(path);
    let mut f = fs::File::open(p).map_err(|e| format!("{what}: cannot open {}: {}", p.display(), e.kind()))?;
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
        let on = match a.notify_on.as_deref() {
            None | Some("settled") => NotifyOn::Settled,
            Some("all") => NotifyOn::All,
            Some(_) => return Err("--notify-on must be `settled` or `all`".into()),
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
        });
        if hook
            .as_ref()
            .is_some_and(|h| h.program.is_empty() || util::has_control_chars(&h.program))
        {
            return Err("--on-settle needs a program path".into());
        }
        Ok(Notify { webhook, hook, on })
    }

    pub fn is_empty(&self) -> bool {
        self.webhook.is_none() && self.hook.is_none()
    }

    pub fn applies_to(&self, event: &str) -> bool {
        match self.on {
            NotifyOn::Settled => event == "settled",
            NotifyOn::All => matches!(event, "settled" | "timeout" | "error"),
        }
    }

    /// Delivers `body` (the result JSON) for `event`. Returns `None` when
    /// nothing was configured for this event; otherwise a JSON summary with
    /// an overall `delivered` flag. The webhook and the hook run in parallel.
    pub fn deliver(&self, event: &str, body: &[u8]) -> Option<Value> {
        if self.is_empty() || !self.applies_to(event) {
            return None;
        }
        let id = util::hex(&util::random_bytes(16));
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
        let ok = |v: &Option<Value>| v.as_ref().is_none_or(|v| v["delivered"] == true);
        let mut out = json!({ "delivered": ok(&webhook) && ok(&hook), "id": id });
        if let Some(w) = webhook {
            out["webhook"] = w;
        }
        if let Some(h) = hook {
            out["onSettle"] = h;
        }
        Some(out)
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

fn post_webhook(w: &Webhook, event: &str, id: &str, body: &[u8]) -> Value {
    let https = w.url.starts_with("https://");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .https_only(https)
        .user_agent(concat!("t3-watch/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
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
        let ts = util::now_ms() / 1000;
        let mut req = agent
            .post(&w.url)
            .header("Content-Type", "application/json")
            .header("X-T3-Watch-Event", event)
            .header("X-T3-Watch-Id", id);
        if let Some(secret) = &w.secret {
            req = req
                .header("X-T3-Watch-Timestamp", ts.to_string())
                .header("X-T3-Watch-Signature", signature(secret, ts, id, body));
        }
        for (k, v) in &w.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let sent = req
            .config()
            .timeout_global(Some(remaining.min(ATTEMPT_TIMEOUT)))
            .build()
            .send(body);
        let mut delay = jitter.backoff(attempts - 1, Duration::from_secs(1), Duration::from_secs(8));
        match sent {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let wait_hint = retry_after(&resp);
                // Drain a little so the connection closes cleanly; never keep it.
                let _ = resp
                    .into_body()
                    .into_reader()
                    .take(64 * 1024)
                    .read_to_end(&mut Vec::new());
                last_status = Some(status);
                match status {
                    200..=299 => {
                        return json!({ "delivered": true, "attempts": attempts, "status": status });
                    }
                    408 | 429 | 500..=599 => {
                        last_error = format!("HTTP {status}");
                        if let Some(hint) = wait_hint {
                            delay = hint;
                        }
                    }
                    300..=399 => {
                        return json!({ "delivered": false, "attempts": attempts, "status": status,
                                       "error": format!("HTTP {status}: redirects are not followed") });
                    }
                    _ => {
                        return json!({ "delivered": false, "attempts": attempts, "status": status,
                                       "error": format!("HTTP {status} (not retried)") });
                    }
                }
            }
            Err(e) => last_error = util::describe_ureq_error(&e),
        }
        if attempts >= WEBHOOK_ATTEMPTS {
            break;
        }
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

/// Runs the hook with the result JSON on stdin. Its stdout and stderr go to
/// our stderr, never to the JSON on stdout. No shell is involved: `program`
/// and its arguments are passed as they are.
fn run_hook(h: &Hook, event: &str, body: &[u8]) -> Value {
    let to_stderr = || {
        std::io::stderr()
            .as_fd()
            .try_clone_to_owned()
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::null())
    };
    let mut child = match Command::new(&h.program)
        .args(&h.args)
        .env("T3_WATCH_EVENT", event)
        .stdin(Stdio::piped())
        .stdout(to_stderr())
        .stderr(to_stderr())
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
        a.notify_on = Some("all".into());
        let n = Notify::from_args(&a).unwrap();
        assert!(n.applies_to("timeout") && n.applies_to("error") && n.applies_to("settled"));
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
        };
        assert_eq!(run_hook(&ok, "settled", b"{\"a\":1}")["delivered"], true);
        assert_eq!(fs::read_to_string(&out).unwrap(), "{\"a\":1}");
        let slow = Hook {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
            timeout: Duration::from_millis(200),
        };
        let started = Instant::now();
        let v = run_hook(&slow, "settled", b"{}");
        assert_eq!(v["timedOut"], true);
        assert!(started.elapsed() < Duration::from_secs(5));
        let failing = Hook {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "exit 7".into()],
            timeout: Duration::from_secs(5),
        };
        assert_eq!(run_hook(&failing, "settled", b"{}")["exitCode"], 7);
        let missing = Hook {
            program: "/no/such/hook".into(),
            args: vec![],
            timeout: Duration::from_secs(5),
        };
        assert_eq!(run_hook(&missing, "settled", b"{}")["delivered"], false);
        fs::remove_dir_all(dir).unwrap();
    }
}
