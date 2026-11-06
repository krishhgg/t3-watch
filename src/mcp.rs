//! Streamable HTTP MCP client (protocol 2025-03-26), just enough to call
//! `t3_thread_wait` and `t3_thread_read`.
//!
//! One `McpClient` exists per machine and is shared by every watch on it, so
//! all of them reuse one `Mcp-Session-Id` and one keep-alive connection pool.

use crate::util::lock;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

pub const PROTOCOL_VERSION: &str = "2025-03-26";

#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    /// Network trouble, Cloudflare 5xx/530, timeouts. Retry with backoff and
    /// never wake the caller for it.
    Transient { message: String, status: Option<u16> },
    /// 401/403 or a tool reporting missing permission. The caller exits 2.
    Auth(String),
    /// The thread does not exist (any more).
    NotFound(String),
    /// Anything else the server rejected outright.
    Fatal(String),
    /// The server no longer knows our Mcp-Session-Id. `call_tool` handles
    /// this by re-initializing; it never reaches the engine.
    Session { message: String, status: Option<u16> },
}

impl CallError {
    pub fn message(&self) -> &str {
        match self {
            CallError::Transient { message, .. } | CallError::Session { message, .. } => message,
            CallError::Auth(m) | CallError::NotFound(m) | CallError::Fatal(m) => m,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            CallError::Transient { .. } | CallError::Session { .. } => "transient",
            CallError::Auth(_) => "auth",
            CallError::NotFound(_) => "not_found",
            CallError::Fatal(_) => "fatal",
        }
    }
}

#[derive(Default)]
struct Session {
    id: Option<String>,
    ready: bool,
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
}

pub struct McpClient {
    pub name: String,
    endpoint: String,
    token: Mutex<String>,
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
    pub fn new(name: &str, origin: &str, token: &str) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(15)))
            .max_idle_connections_per_host(64)
            .max_idle_age(Duration::from_secs(300))
            .user_agent(concat!("t3-watch/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        McpClient {
            name: name.to_string(),
            endpoint: format!("{}/mcp", origin.trim_end_matches('/')),
            token: Mutex::new(token.to_string()),
            session: Mutex::new(Session::default()),
            init: Mutex::new(()),
            agent,
            next_id: AtomicU64::new(1),
            stats: Stats::default(),
        }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Swaps in a new token (after `auth login`) and drops the old session.
    pub fn set_token(&self, token: &str) {
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
        s.generation += 1;
    }

    pub fn session_id(&self) -> Option<String> {
        lock(&self.session).id.clone()
    }

    fn post(&self, body: &Value, session_id: Option<&str>, timeout: Duration) -> Result<Reply, CallError> {
        let token = lock(&self.token).clone();
        let mut req = self
            .agent
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("Authorization", format!("Bearer {token}"));
        if let Some(id) = session_id {
            req = req.header("Mcp-Session-Id", id);
        }
        let resp = req
            .config()
            .timeout_global(Some(timeout))
            .build()
            .send(body.to_string())
            .map_err(transport_error)?;

        let status = resp.status().as_u16();
        let header = |k: &str| resp.headers().get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
        let new_session = header("mcp-session-id");
        let content_type = header("content-type").unwrap_or_default().to_ascii_lowercase();
        let mut body_reader = resp.into_body().into_reader();

        match status {
            200..=299 => {}
            401 | 403 => {
                return Err(CallError::Auth(format!(
                    "{} rejected the token (HTTP {status}); run `t3-watch auth login {} --name {}` again",
                    self.endpoint,
                    self.endpoint.trim_end_matches("/mcp"),
                    self.name
                )))
            }
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
                read_sse_response(BufReader::new(body_reader), id)?
            }
            Some(id) => {
                let mut text = String::new();
                (&mut body_reader)
                    .take(16 << 20)
                    .read_to_string(&mut text)
                    .map_err(|e| CallError::Transient {
                        message: format!("reading response: {e}"),
                        status: None,
                    })?;
                let v: Value = serde_json::from_str(&text).map_err(|e| CallError::Transient {
                    message: format!("invalid JSON from server: {e}"),
                    status: None,
                })?;
                pick_response(v, id)
            }
        };
        if want_id.is_some() && message.is_none() {
            return Err(CallError::Transient {
                message: "response ended without a JSON-RPC result".into(),
                status: None,
            });
        }
        Ok(Reply {
            session_id: new_session,
            message,
        })
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
            let id = self.handshake().map_err(|e| match e {
                CallError::Session { message, status } => CallError::Transient { message, status },
                e => e,
            })?;
            let mut s = lock(&self.session);
            // A token change during the handshake makes this session stale.
            if s.generation == generation {
                s.id = id;
                s.ready = true;
                return Ok((s.id.clone(), s.generation));
            }
        }
    }

    fn handshake(&self) -> Result<Option<String>, CallError> {
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
            return Err(CallError::Fatal(format!("initialize failed: {err}")));
        }
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        self.post(&note, reply.session_id.as_deref(), Duration::from_secs(30))?;
        Ok(reply.session_id)
    }

    fn reset_session(&self, generation: u64) {
        let mut s = lock(&self.session);
        if s.generation == generation {
            s.id = None;
            s.ready = false;
            s.generation += 1;
            self.stats.session_resets.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Calls a tool and returns its JSON payload. An expired session is
    /// re-initialized once and the call retried; every other error goes back
    /// to the caller, which owns the backoff policy.
    pub fn call_tool(&self, name: &str, args: Value, timeout: Duration) -> Result<Value, CallError> {
        match name {
            "t3_thread_wait" => self.stats.thread_wait.fetch_add(1, Ordering::Relaxed),
            "t3_thread_read" => self.stats.thread_read.fetch_add(1, Ordering::Relaxed),
            _ => 0,
        };
        let mut attempt = 0;
        loop {
            let (session, generation) = self.ensure_session()?;
            let req = json!({
                "jsonrpc": "2.0",
                "id": self.next_id.fetch_add(1, Ordering::Relaxed),
                "method": "tools/call",
                "params": { "name": name, "arguments": args }
            });
            let result = self
                .post(&req, session.as_deref(), timeout)
                .and_then(|reply| tool_payload(reply.message.unwrap_or(Value::Null)));
            match result {
                Err(CallError::Session { message, status }) => {
                    self.reset_session(generation);
                    if attempt == 0 {
                        attempt += 1;
                        continue;
                    }
                    // A fresh session got the same answer, so this was never
                    // an expiry: a 400 is a bad request, a 404 a missing route.
                    return Err(match status {
                        Some(400) => CallError::Fatal(format!("{} rejected the request: {message}", self.endpoint)),
                        _ => CallError::Transient { message, status },
                    });
                }
                other => return other,
            }
        }
    }
}

fn transport_error(e: ureq::Error) -> CallError {
    CallError::Transient {
        message: format!("network: {e}"),
        status: None,
    }
}

fn read_snippet(r: &mut impl Read) -> String {
    let mut buf = Vec::new();
    let _ = r.take(2048).read_to_end(&mut buf);
    let s = String::from_utf8_lossy(&buf);
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect()
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

/// Reads `text/event-stream` until the event carrying our response arrives.
/// Notifications and other events on the stream are skipped.
pub fn read_sse_response(reader: impl BufRead, want_id: &Value) -> Result<Option<Value>, CallError> {
    let mut data = String::new();
    for line in reader.lines() {
        let line = line.map_err(|e| CallError::Transient {
            message: format!("reading event stream: {e}"),
            status: None,
        })?;
        let line = line.strip_suffix('\r').unwrap_or(&line);
        if line.is_empty() {
            if let Some(msg) = take_event(&mut data, want_id) {
                return Ok(Some(msg));
            }
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
        // `event:`, `id:`, `retry:` and `:` comment lines need no handling.
    }
    Ok(take_event(&mut data, want_id))
}

fn take_event(data: &mut String, want_id: &Value) -> Option<Value> {
    if data.is_empty() {
        return None;
    }
    let parsed = serde_json::from_str::<Value>(data).ok();
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
        let text = err.get("message").and_then(Value::as_str).unwrap_or("").to_string();
        let message = format!("JSON-RPC error: {err}");
        if text.to_ascii_lowercase().contains("session") {
            return Err(CallError::Session { message, status: None });
        }
        // Parse error, invalid request, unknown method, invalid params: our
        // request is wrong and retrying will not help. Anything else (internal
        // errors, relay timeouts) may pass.
        return Err(match err.get("code").and_then(Value::as_i64) {
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
            text.chars().take(200).collect::<String>()
        ))
    })
}

/// T3 reports tool failures as `{"code": "...", "message": "..."}`. Unknown
/// codes count as transient: a peer environment that is briefly unreachable
/// must not wake the caller.
pub fn classify_tool_error(text: &str) -> CallError {
    let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let code = v.get("code").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    let message = v.get("message").and_then(Value::as_str).unwrap_or(text).to_string();
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

    #[test]
    fn sse_skips_notifications_and_joins_data_lines() {
        let stream = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n\
                      : keepalive\n\n\
                      event: message\r\ndata: {\"jsonrpc\":\"2.0\",\r\ndata: \"id\":7,\"result\":{\"ok\":true}}\r\n\r\n";
        let msg = read_sse_response(stream.as_bytes(), &json!(7)).unwrap().unwrap();
        assert_eq!(msg["result"]["ok"], true);
    }

    #[test]
    fn sse_without_trailing_blank_line() {
        let stream = "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{}}";
        assert!(read_sse_response(stream.as_bytes(), &json!(3)).unwrap().is_some());
        assert!(
            read_sse_response("data: {\"id\":4,\"result\":{}}\n\n".as_bytes(), &json!(3))
                .unwrap()
                .is_none()
        );
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
}
