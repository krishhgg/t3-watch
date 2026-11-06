//! Streamable HTTP MCP client (protocol 2025-03-26), just enough to call
//! `t3_thread_wait` and `t3_thread_read`.
//!
//! One `McpClient` exists per (origin, pairing) and is shared by every watch
//! on it, so they reuse one `Mcp-Session-Id` and one keep-alive pool. The
//! origin never changes after construction; only the access token can, when
//! a 401 is recovered by reloading machines.json or refreshing the token.

use crate::util::{self, lock};
use serde_json::{json, Value};
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

pub const PROTOCOL_VERSION: &str = "2025-03-26";
/// Versions whose `tools/call` behaves the same for our two tools.
pub const SUPPORTED_VERSIONS: &[&str] = &["2025-03-26", "2025-06-18"];

/// Upper bounds for one HTTP response, so a broken upstream cannot exhaust
/// the shared daemon's memory.
pub struct Limits {
    pub max_line: usize,
    pub max_event: usize,
    pub max_body: usize,
}

pub const LIMITS: Limits = Limits {
    max_line: 1 << 20,
    max_event: 8 << 20,
    max_body: 16 << 20,
};

#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    /// Network trouble, Cloudflare 5xx/530, timeouts. Retry with backoff and
    /// never wake the caller for it.
    Transient { message: String, status: Option<u16> },
    /// 401/403 that could not be recovered, or a tool reporting missing
    /// permission. The caller exits 2.
    Auth(String),
    /// The thread does not exist (any more).
    NotFound(String),
    /// Anything else the server rejected outright.
    Fatal(String),
    /// The server no longer knows our Mcp-Session-Id. `call_tool` handles
    /// this by re-initializing; it never reaches the engine.
    Session { message: String, status: Option<u16> },
    /// HTTP 401. `call_tool` tries to recover a token once, then reports
    /// `Auth`; it never reaches the engine.
    Unauthorized(String),
}

impl CallError {
    pub fn message(&self) -> &str {
        match self {
            CallError::Transient { message, .. } | CallError::Session { message, .. } => message,
            CallError::Auth(m) | CallError::NotFound(m) | CallError::Fatal(m) | CallError::Unauthorized(m) => m,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            CallError::Transient { .. } | CallError::Session { .. } => "transient",
            CallError::Auth(_) | CallError::Unauthorized(_) => "auth",
            CallError::NotFound(_) => "not_found",
            CallError::Fatal(_) => "fatal",
        }
    }

    fn transient(message: impl Into<String>) -> Self {
        CallError::Transient {
            message: message.into(),
            status: None,
        }
    }
}

#[derive(Default)]
struct Session {
    id: Option<String>,
    ready: bool,
    /// Negotiated protocol version, once initialized.
    version: Option<String>,
    /// Bumped on every reset so concurrent workers that all saw the same
    /// expired session re-initialize it once, not once each.
    generation: u64,
}

#[derive(Default)]
pub struct Stats {
    pub initialize: AtomicU64,
    pub thread_wait: AtomicU64,
    pub thread_read: AtomicU64,
    pub retries: AtomicU64,
    pub session_resets: AtomicU64,
    pub token_recoveries: AtomicU64,
}

/// Called on HTTP 401 with the token that was rejected. Returns a token to
/// retry with (reloaded from machines.json or refreshed), if there is one.
pub type TokenRecovery = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub struct McpClient {
    /// Machine name at creation time, for messages only.
    pub label: String,
    origin: String,
    endpoint: String,
    fingerprint: String,
    token: Mutex<String>,
    recovery: Option<TokenRecovery>,
    /// Serializes token recovery so one 401 storm triggers one refresh.
    recovering: Mutex<()>,
    /// Short-lived lock over the session state. Never held across I/O.
    session: Mutex<Session>,
    /// Held for the whole initialize handshake so concurrent callers wait
    /// for one handshake instead of each starting their own.
    init: Mutex<()>,
    agent: ureq::Agent,
    next_id: AtomicU64,
    pub stats: Stats,
}

/// A parsed HTTP response from the MCP endpoint.
struct Reply {
    session_id: Option<String>,
    message: Option<Value>,
}

impl McpClient {
    /// Fails unless `origin` is `https://`, or `http://` on loopback: the
    /// bearer token must never cross a network in clear text.
    pub fn new(
        label: &str,
        origin: &str,
        token: &str,
        fingerprint: &str,
        recovery: Option<TokenRecovery>,
    ) -> Result<Self, String> {
        let url = util::secure_url(origin, &format!("MCP origin of {label:?}"))?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .https_only(url.scheme == "https")
            .timeout_connect(Some(Duration::from_secs(15)))
            .max_idle_connections_per_host(64)
            .max_idle_age(Duration::from_secs(300))
            .user_agent(concat!("t3-watch/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Ok(McpClient {
            label: label.to_string(),
            endpoint: format!("{}/mcp", url.origin),
            origin: url.origin,
            fingerprint: fingerprint.to_string(),
            token: Mutex::new(token.to_string()),
            recovery,
            recovering: Mutex::new(()),
            session: Mutex::new(Session::default()),
            init: Mutex::new(()),
            agent,
            next_id: AtomicU64::new(1),
            stats: Stats::default(),
        })
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn session_id(&self) -> Option<String> {
        lock(&self.session).id.clone()
    }

    fn token(&self) -> String {
        lock(&self.token).clone()
    }

    /// Swaps in a new token and drops the old session.
    fn set_token(&self, token: &str) {
        {
            let mut t = lock(&self.token);
            if *t == token {
                return;
            }
            *t = token.to_string();
        }
        let mut s = lock(&self.session);
        s.id = None;
        s.ready = false;
        s.version = None;
        s.generation += 1;
    }

    fn post(&self, body: &Value, session_id: Option<&str>, timeout: Duration) -> Result<Reply, CallError> {
        let token = self.token();
        let version = lock(&self.session).version.clone();
        let mut req = self
            .agent
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Authorization", format!("Bearer {token}"));
        if let Some(id) = session_id {
            req = req.header("Mcp-Session-Id", id);
        }
        // 2025-06-18 requires the negotiated version on every later request.
        if let Some(v) = version.filter(|v| v.as_str() != "2025-03-26") {
            req = req.header("MCP-Protocol-Version", v);
        }
        let resp = req
            .config()
            .timeout_global(Some(timeout))
            .build()
            .send(body.to_string())
            .map_err(|e| CallError::transient(format!("network: {}", util::describe_ureq_error(&e))))?;

        let status = resp.status().as_u16();
        let header = |k: &str| resp.headers().get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
        let new_session = header("mcp-session-id").filter(|s| !util::has_control_chars(s) && s.len() <= 1024);
        let content_type = header("content-type").unwrap_or_default().to_ascii_lowercase();
        let mut body_reader = resp.into_body().into_reader();

        match status {
            200..=299 => {}
            401 => return Err(CallError::Unauthorized(self.auth_message(status))),
            403 => return Err(CallError::Auth(self.auth_message(status))),
            404 | 400 if session_id.is_some() => {
                // Most likely an expired or unknown Mcp-Session-Id.
                let snippet = read_snippet(&mut body_reader);
                return Err(CallError::Session {
                    message: format!("HTTP {status} for session: {snippet}"),
                    status: Some(status),
                });
            }
            404 | 408 | 425 | 429 | 500..=599 => {
                let snippet = read_snippet(&mut body_reader);
                return Err(CallError::Transient {
                    message: format!("HTTP {status}{}", cloudflare_hint(status, &snippet)),
                    status: Some(status),
                });
            }
            _ => {
                let snippet = read_snippet(&mut body_reader);
                return Err(CallError::Fatal(format!("HTTP {status}: {snippet}")));
            }
        }

        let want_id = body.get("id");
        let message = match want_id {
            None => None,
            Some(_) if status == 202 => None,
            Some(id) if content_type.starts_with("text/event-stream") => {
                read_sse_response(&mut body_reader, id, &LIMITS)?
            }
            Some(id) => {
                let mut bytes = Vec::new();
                (&mut body_reader)
                    .take(LIMITS.max_body as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|e| CallError::transient(format!("reading response: {:?}", e.kind())))?;
                if bytes.len() > LIMITS.max_body {
                    return Err(CallError::transient(format!(
                        "response larger than {} bytes",
                        LIMITS.max_body
                    )));
                }
                let v: Value = serde_json::from_slice(&bytes)
                    .map_err(|e| CallError::transient(format!("invalid JSON from server: {e}")))?;
                pick_response(v, id)
            }
        };
        if want_id.is_some() && message.is_none() {
            return Err(CallError::transient("response ended without a JSON-RPC result"));
        }
        if let Some(m) = &message {
            if m.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
                return Err(CallError::Fatal("server reply is not JSON-RPC 2.0".into()));
            }
        }
        Ok(Reply {
            session_id: new_session,
            message,
        })
    }

    fn auth_message(&self, status: u16) -> String {
        format!(
            "{} rejected the token (HTTP {status}); run `t3-watch auth login {} --name {}` again",
            self.endpoint, self.origin, self.label
        )
    }

    /// Returns the current session id and its generation, running the
    /// `initialize` handshake first if needed.
    fn ensure_session(&self) -> Result<(Option<String>, u64), CallError> {
        loop {
            let generation = {
                let s = lock(&self.session);
                if s.ready {
                    return Ok((s.id.clone(), s.generation));
                }
                s.generation
            };
            let _init = lock(&self.init);
            {
                // Another caller may have finished the handshake while we waited.
                let s = lock(&self.session);
                if s.ready || s.generation != generation {
                    continue;
                }
            }
            let (id, version) = self.handshake().map_err(|e| match e {
                CallError::Session { message, status } => CallError::Transient { message, status },
                e => e,
            })?;
            let mut s = lock(&self.session);
            // A token change during the handshake makes this session stale.
            if s.generation == generation {
                s.id = id;
                s.version = Some(version);
                s.ready = true;
                return Ok((s.id.clone(), s.generation));
            }
        }
    }

    fn handshake(&self) -> Result<(Option<String>, String), CallError> {
        self.stats.initialize.fetch_add(1, Ordering::Relaxed);
        let init = json!({
            "jsonrpc": "2.0",
            "id": self.next_id.fetch_add(1, Ordering::Relaxed),
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "t3-watch", "version": env!("CARGO_PKG_VERSION") }
            }
        });
        let reply = self.post(&init, None, Duration::from_secs(30))?;
        let msg = reply.message.unwrap_or(Value::Null);
        if let Some(err) = msg.get("error") {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
            return Err(CallError::Fatal(format!("initialize failed (JSON-RPC error {code})")));
        }
        let version = check_protocol_version(&msg)?;
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        self.post(&note, reply.session_id.as_deref(), Duration::from_secs(30))?;
        Ok((reply.session_id, version))
    }

    fn reset_session(&self, generation: u64) {
        let mut s = lock(&self.session);
        if s.generation == generation {
            s.id = None;
            s.ready = false;
            s.version = None;
            s.generation += 1;
            self.stats.session_resets.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// After a 401 with `used`: if another worker already switched tokens,
    /// retry with that; otherwise ask the recovery hook (reload, then
    /// refresh). Returns whether a retry makes sense.
    fn recover_token(&self, used: &str) -> bool {
        let _one_at_a_time = lock(&self.recovering);
        if self.token() != used {
            return true;
        }
        let Some(recover) = &self.recovery else { return false };
        match recover(used) {
            Some(new) if new != used && !new.is_empty() => {
                self.stats.token_recoveries.fetch_add(1, Ordering::Relaxed);
                self.set_token(&new);
                true
            }
            _ => false,
        }
    }

    /// Calls a tool and returns its JSON payload. An expired session is
    /// re-initialized once and the call retried, and a 401 gets one token
    /// recovery; every other error goes back to the caller, which owns the
    /// backoff policy.
    pub fn call_tool(&self, name: &str, args: Value, timeout: Duration) -> Result<Value, CallError> {
        match name {
            "t3_thread_wait" => self.stats.thread_wait.fetch_add(1, Ordering::Relaxed),
            "t3_thread_read" => self.stats.thread_read.fetch_add(1, Ordering::Relaxed),
            _ => 0,
        };
        let mut session_retried = false;
        let mut auth_retried = false;
        loop {
            let used = self.token();
            let mut generation = None;
            let result = self.ensure_session().and_then(|(session, g)| {
                generation = Some(g);
                let req = json!({
                    "jsonrpc": "2.0",
                    "id": self.next_id.fetch_add(1, Ordering::Relaxed),
                    "method": "tools/call",
                    "params": { "name": name, "arguments": args }
                });
                self.post(&req, session.as_deref(), timeout)
                    .and_then(|reply| tool_payload(reply.message.unwrap_or(Value::Null)))
            });
            match result {
                Err(CallError::Session { message, status }) => {
                    if let Some(g) = generation {
                        self.reset_session(g);
                    }
                    if !session_retried {
                        session_retried = true;
                        continue;
                    }
                    // A fresh session got the same answer, so this was never
                    // an expiry: a 400 is a bad request, a 404 a missing route.
                    return Err(match status {
                        Some(400) => CallError::Fatal(format!("{} rejected the request: {message}", self.endpoint)),
                        _ => CallError::Transient { message, status },
                    });
                }
                Err(CallError::Unauthorized(message)) => {
                    if !auth_retried && self.recover_token(&used) {
                        auth_retried = true;
                        continue;
                    }
                    return Err(CallError::Auth(message));
                }
                other => return other,
            }
        }
    }
}

/// The server must answer `initialize` with a protocol version we speak.
pub fn check_protocol_version(msg: &Value) -> Result<String, CallError> {
    match msg.pointer("/result/protocolVersion").and_then(Value::as_str) {
        Some(v) if SUPPORTED_VERSIONS.contains(&v) => Ok(v.to_string()),
        Some(v) => Err(CallError::Fatal(format!(
            "server speaks MCP protocol {:?}; t3-watch supports {}",
            util::sanitize_text(v, 40),
            SUPPORTED_VERSIONS.join(", ")
        ))),
        None => Err(CallError::Fatal("initialize result has no protocolVersion".into())),
    }
}

fn read_snippet(r: &mut impl Read) -> String {
    let mut buf = Vec::new();
    let _ = r.take(2048).read_to_end(&mut buf);
    util::sanitize_text(&String::from_utf8_lossy(&buf), 200)
}

/// Names the usual Cloudflare tunnel failures so logs and timeout output say
/// what happened (a sleeping laptop shows up as 530 with error 1033).
fn cloudflare_hint(status: u16, body: &str) -> String {
    if body.contains("1033") {
        " (Cloudflare 1033: tunnel origin offline, e.g. the machine is asleep)".into()
    } else {
        match status {
            502 => " (bad gateway)".into(),
            504 => " (gateway timeout)".into(),
            524 => " (Cloudflare timeout waiting for origin)".into(),
            530 => " (Cloudflare origin unreachable)".into(),
            _ => String::new(),
        }
    }
}

/// Reads `text/event-stream` until the event carrying our response arrives,
/// skipping notifications and comments. Lines may end in LF, CRLF or CR.
/// Every line, every event's data and the whole stream are size-bounded.
pub fn read_sse_response(mut reader: impl Read, want_id: &Value, limits: &Limits) -> Result<Option<Value>, CallError> {
    let mut chunk = [0u8; 8192];
    let mut line: Vec<u8> = Vec::new();
    let mut data: Vec<u8> = Vec::new();
    let mut total = 0usize;
    let mut after_cr = false;
    loop {
        let n = match reader.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(CallError::transient(format!("reading event stream: {:?}", e.kind()))),
        };
        if n == 0 {
            break;
        }
        total += n;
        if total > limits.max_body {
            return Err(CallError::transient(format!(
                "event stream larger than {} bytes",
                limits.max_body
            )));
        }
        for &b in &chunk[..n] {
            if after_cr && b == b'\n' {
                after_cr = false;
                continue;
            }
            after_cr = b == b'\r';
            if b == b'\r' || b == b'\n' {
                if let Some(msg) = sse_line(&line, &mut data, want_id, limits)? {
                    return Ok(Some(msg));
                }
                line.clear();
            } else {
                if line.len() >= limits.max_line {
                    return Err(CallError::transient(format!(
                        "event stream line longer than {} bytes",
                        limits.max_line
                    )));
                }
                line.push(b);
            }
        }
    }
    if !line.is_empty() {
        if let Some(msg) = sse_line(&line, &mut data, want_id, limits)? {
            return Ok(Some(msg));
        }
    }
    Ok(dispatch_event(&mut data, want_id))
}

fn sse_line(line: &[u8], data: &mut Vec<u8>, want_id: &Value, limits: &Limits) -> Result<Option<Value>, CallError> {
    if line.is_empty() {
        return Ok(dispatch_event(data, want_id));
    }
    if let Some(rest) = line.strip_prefix(b"data:") {
        let rest = rest.strip_prefix(b" ").unwrap_or(rest);
        if data.len() + rest.len() + 1 > limits.max_event {
            return Err(CallError::transient(format!(
                "event stream event larger than {} bytes",
                limits.max_event
            )));
        }
        if !data.is_empty() {
            data.push(b'\n');
        }
        data.extend_from_slice(rest);
    }
    // `event:`, `id:`, `retry:` and `:` comment lines need no handling.
    Ok(None)
}

fn dispatch_event(data: &mut Vec<u8>, want_id: &Value) -> Option<Value> {
    if data.is_empty() {
        return None;
    }
    let parsed = serde_json::from_slice::<Value>(data).ok();
    data.clear();
    parsed.and_then(|v| pick_response(v, want_id))
}

/// Finds the JSON-RPC response with our id in a message or batch.
fn pick_response(v: Value, want_id: &Value) -> Option<Value> {
    match v {
        Value::Array(items) => items.into_iter().find_map(|i| pick_response(i, want_id)),
        Value::Object(_) if v.get("id") == Some(want_id) && (v.get("result").is_some() || v.get("error").is_some()) => {
            Some(v)
        }
        _ => None,
    }
}

/// Unwraps a `tools/call` result into the tool's JSON payload, preferring
/// `structuredContent` and falling back to the first text block.
pub fn tool_payload(msg: Value) -> Result<Value, CallError> {
    if let Some(err) = msg.get("error") {
        let text = err.get("message").and_then(Value::as_str).unwrap_or("");
        let code = err.get("code").and_then(Value::as_i64);
        let message = format!(
            "JSON-RPC error {}: {}",
            code.unwrap_or(0),
            util::sanitize_text(text, 200)
        );
        if text.to_ascii_lowercase().contains("session") {
            return Err(CallError::Session { message, status: None });
        }
        // Parse error, invalid request, unknown method, invalid params: our
        // request is wrong and retrying will not help. Anything else (internal
        // errors, relay timeouts) may pass.
        return Err(match code {
            Some(-32700 | -32600 | -32601 | -32602) => CallError::Fatal(message),
            _ => CallError::Transient { message, status: None },
        });
    }
    let result = msg
        .get("result")
        .ok_or_else(|| CallError::Fatal("response has no result".into()))?;
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|c| c.iter().find_map(|i| i.get("text").and_then(Value::as_str)))
        .unwrap_or("");
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(classify_tool_error(text));
    }
    if let Some(sc) = result.get("structuredContent").filter(|v| v.is_object()) {
        return Ok(sc.clone());
    }
    serde_json::from_str(text).map_err(|_| {
        CallError::Fatal(format!(
            "tool returned non-JSON text: {}",
            util::sanitize_text(text, 200)
        ))
    })
}

/// T3 reports tool failures as `{"code": "...", "message": "..."}`. Unknown
/// codes count as transient: a peer environment that is briefly unreachable
/// must not wake the caller.
pub fn classify_tool_error(text: &str) -> CallError {
    let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let code = v.get("code").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    let message = util::sanitize_text(v.get("message").and_then(Value::as_str).unwrap_or(text), 300);
    let lower = format!("{code} {}", message.to_ascii_lowercase());
    if code.contains("not_found") || lower.contains("no longer available") || lower.contains("does not exist") {
        CallError::NotFound(message)
    } else if ["unauthorized", "forbidden", "permission", "scope", "credential"]
        .iter()
        .any(|w| lower.contains(w))
    {
        CallError::Auth(message)
    } else if code.contains("invalid") || code.contains("validation") {
        CallError::Fatal(message)
    } else {
        CallError::Transient { message, status: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: Limits = Limits {
        max_line: 64,
        max_event: 128,
        max_body: 1024,
    };

    #[test]
    fn sse_skips_notifications_and_joins_data_lines() {
        let stream = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n\
                      : keepalive\n\n\
                      event: message\r\ndata: {\"jsonrpc\":\"2.0\",\r\ndata: \"id\":7,\"result\":{\"ok\":true}}\r\n\r\n";
        let msg = read_sse_response(stream.as_bytes(), &json!(7), &LIMITS)
            .unwrap()
            .unwrap();
        assert_eq!(msg["result"]["ok"], true);
    }

    #[test]
    fn sse_accepts_bare_cr_line_endings() {
        let stream = "data: {\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{}}\r\r";
        assert!(read_sse_response(stream.as_bytes(), &json!(5), &LIMITS)
            .unwrap()
            .is_some());
    }

    #[test]
    fn sse_without_trailing_blank_line() {
        let stream = "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{}}";
        assert!(read_sse_response(stream.as_bytes(), &json!(3), &LIMITS)
            .unwrap()
            .is_some());
        assert!(
            read_sse_response("data: {\"id\":4,\"result\":{}}\n\n".as_bytes(), &json!(3), &LIMITS)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn sse_rejects_an_overlong_line() {
        let stream = format!("data: {}\n\n", "x".repeat(200));
        let err = read_sse_response(stream.as_bytes(), &json!(1), &SMALL).unwrap_err();
        assert!(err.message().contains("line longer"), "{err:?}");
    }

    #[test]
    fn sse_rejects_an_overlong_event() {
        let stream = "data: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n".repeat(5);
        let err = read_sse_response(stream.as_bytes(), &json!(1), &SMALL).unwrap_err();
        assert!(err.message().contains("event larger"), "{err:?}");
    }

    #[test]
    fn sse_rejects_an_endless_stream_of_comments() {
        let stream = ": keepalive\n".repeat(200);
        let err = read_sse_response(stream.as_bytes(), &json!(1), &SMALL).unwrap_err();
        assert!(err.message().contains("stream larger"), "{err:?}");
        assert!(matches!(err, CallError::Transient { .. }));
    }

    #[test]
    fn payload_prefers_structured_content() {
        let msg =
            json!({"result": {"content": [{"type": "text", "text": "{\"a\":1}"}], "structuredContent": {"a": 2}}});
        assert_eq!(tool_payload(msg).unwrap()["a"], 2);
        let msg = json!({"result": {"content": [{"type": "text", "text": "{\"a\":1}"}]}});
        assert_eq!(tool_payload(msg).unwrap()["a"], 1);
    }

    #[test]
    fn tool_errors_are_classified() {
        let not_found = r#"{"_tag":"OrchestratorMcpFailure","code":"thread_not_found","message":"Thread x is no longer available."}"#;
        let msg = json!({"result": {"isError": true, "content": [{"type": "text", "text": not_found}]}});
        assert!(matches!(tool_payload(msg), Err(CallError::NotFound(_))));
        assert!(matches!(
            classify_tool_error(r#"{"code":"insufficient_scope"}"#),
            CallError::Auth(_)
        ));
        assert!(matches!(
            classify_tool_error(r#"{"code":"environment_unreachable","message":"peer offline"}"#),
            CallError::Transient { .. }
        ));
        assert!(matches!(
            classify_tool_error("plain text failure"),
            CallError::Transient { .. }
        ));
        // A tool message that merely mentions a session is not an expiry.
        let crashed = r#"{"code":"provider_error","message":"Codex session crashed"}"#;
        let msg = json!({"result": {"isError": true, "content": [{"type": "text", "text": crashed}]}});
        assert!(matches!(tool_payload(msg), Err(CallError::Transient { .. })));
    }

    #[test]
    fn json_rpc_errors_are_classified() {
        let err = |code: i64, m: &str| json!({"jsonrpc": "2.0", "id": 1, "error": {"code": code, "message": m}});
        assert!(matches!(
            tool_payload(err(-32602, "bad params")),
            Err(CallError::Fatal(_))
        ));
        assert!(matches!(
            tool_payload(err(-32603, "internal")),
            Err(CallError::Transient { .. })
        ));
        assert!(matches!(
            tool_payload(err(-32001, "Session not found")),
            Err(CallError::Session { .. })
        ));
    }

    #[test]
    fn protocol_version_must_be_supported() {
        let init = |v: Value| json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": v}});
        assert_eq!(
            check_protocol_version(&init(json!("2025-03-26"))).unwrap(),
            "2025-03-26"
        );
        assert_eq!(
            check_protocol_version(&init(json!("2025-06-18"))).unwrap(),
            "2025-06-18"
        );
        assert!(matches!(
            check_protocol_version(&init(json!("1999-01-01"))),
            Err(CallError::Fatal(_))
        ));
        assert!(check_protocol_version(&json!({"result": {}})).is_err());
    }

    #[test]
    fn plain_http_origins_are_refused_off_loopback() {
        assert!(McpClient::new("m", "http://relay.example", "t", "f", None).is_err());
        assert!(McpClient::new("m", "http://127.0.0.1:3773", "t", "f", None).is_ok());
        let c = McpClient::new("m", "https://relay.example/", "t", "f", None).unwrap();
        assert_eq!(c.endpoint(), "https://relay.example/mcp");
    }

    #[test]
    fn recovery_is_tried_once_and_only_with_a_new_token() {
        let c = McpClient::new(
            "m",
            "http://127.0.0.1:9",
            "old",
            "f",
            Some(Box::new(|_used: &str| Some("new".into()))),
        )
        .unwrap();
        assert!(c.recover_token("old"));
        assert_eq!(c.token(), "new");
        // A caller that still holds "old" sees the token already moved on.
        assert!(c.recover_token("old"));
        let none = McpClient::new(
            "m",
            "http://127.0.0.1:9",
            "t",
            "f",
            Some(Box::new(|used: &str| Some(used.to_string()))),
        )
        .unwrap();
        assert!(!none.recover_token("t"), "same token again is not a recovery");
        let absent = McpClient::new("m", "http://127.0.0.1:9", "t", "f", None).unwrap();
        assert!(!absent.recover_token("t"));
    }
}
