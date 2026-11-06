//! Test fixtures: a mock T3 Streamable HTTP MCP server (with a mock OAuth
//! authorization server on the same origin), a mock webhook receiver, a mock
//! local `t3` CLI, and helpers to run the real `t3-watch` binary against them.

#![allow(dead_code)]

use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_t3-watch");
pub const TOKEN: &str = "test-token-read-only";

pub struct MockState {
    /// threadId -> (status, run ordinal)
    pub threads: HashMap<String, (String, u64)>,
    pub calls: HashMap<String, usize>,
    pub sse: bool,
    /// The bearer token the MCP endpoint accepts, plus any extras.
    pub token: String,
    pub extra_tokens: HashSet<String>,
    pub sessions: HashSet<String>,
    pub next_session: u64,
    /// Responses to return, in order, before handling MCP requests normally.
    pub fail_queue: VecDeque<(u16, String)>,
    pub wait_args: Vec<Value>,
    /// Answer every tools/call with this HTTP status, even on a valid session.
    pub reject_tools_with: Option<u16>,
    /// Threads whose delegated work has not settled yet.
    pub delegated_pending: HashSet<String>,
    /// Right after a `t3_thread_wait` for this thread computes its answer,
    /// move the thread to (status, ordinal, delegated pending): a new run
    /// that starts between the wait and the read.
    pub advance_after_wait: Option<(String, String, u64, bool)>,
    pub protocol_version: String,
    /// Merged into the protected resource / authorization server metadata.
    pub prm_override: Map<String, Value>,
    pub asm_override: Map<String, Value>,
    /// Path of the issuer below the origin, e.g. "/tenant".
    pub issuer_path: String,
    /// Refresh token handed out with the first access token.
    pub issue_refresh: Option<String>,
    /// Access token a valid refresh grant returns.
    pub refresh_to: Option<String>,
    /// Make the token endpoint fail with this body.
    pub token_error_body: Option<String>,
    /// Answer tool calls with an endless stream of SSE comments.
    pub sse_flood: Option<usize>,
    pub register_bodies: Vec<Value>,
    pub authorize_queries: Vec<HashMap<String, String>>,
    /// code -> (code_challenge, redirect_uri, client_id)
    pub codes: HashMap<String, (String, String, String)>,
}

impl Default for MockState {
    fn default() -> Self {
        MockState {
            threads: HashMap::new(),
            calls: HashMap::new(),
            sse: false,
            token: TOKEN.into(),
            extra_tokens: HashSet::new(),
            sessions: HashSet::new(),
            next_session: 0,
            fail_queue: VecDeque::new(),
            wait_args: Vec::new(),
            reject_tools_with: None,
            delegated_pending: HashSet::new(),
            advance_after_wait: None,
            protocol_version: "2025-03-26".into(),
            prm_override: Map::new(),
            asm_override: Map::new(),
            issuer_path: String::new(),
            issue_refresh: None,
            refresh_to: None,
            token_error_body: None,
            sse_flood: None,
            register_bodies: Vec::new(),
            authorize_queries: Vec::new(),
            codes: HashMap::new(),
        }
    }
}

pub struct MockMcp {
    pub url: String,
    pub state: Arc<(Mutex<MockState>, Condvar)>,
}

fn is_active(s: &str) -> bool {
    matches!(s, "preparing" | "queued" | "starting" | "running")
}

impl MockMcp {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let state = Arc::new((Mutex::new(MockState::default()), Condvar::new()));
        let (st, base) = (state.clone(), url.clone());
        thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let (st, base) = (st.clone(), base.clone());
                thread::spawn(move || handle(conn, &st, &base));
            }
        });
        MockMcp { url, state }
    }

    pub fn set_thread(&self, id: &str, status: &str, ordinal: u64) {
        let (m, cv) = &*self.state;
        m.lock().unwrap().threads.insert(id.into(), (status.into(), ordinal));
        cv.notify_all();
    }

    pub fn calls(&self, name: &str) -> usize {
        *self.state.0.lock().unwrap().calls.get(name).unwrap_or(&0)
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut MockState) -> R) -> R {
        let r = f(&mut self.state.0.lock().unwrap());
        self.state.1.notify_all();
        r
    }

    /// Forgets every Mcp-Session-Id, as a server restart would.
    pub fn expire_sessions(&self) {
        self.with(|s| s.sessions.clear());
    }
}

pub struct Req {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

pub fn read_request(stream: &mut TcpStream) -> Option<Req> {
    let mut r = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        r.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = vec![0; len];
    r.read_exact(&mut body).ok()?;
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    Some(Req {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn respond(stream: &mut TcpStream, status: u16, headers: &[(&str, String)], body: &str) {
    let mut s = format!(
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    s.push_str("\r\n");
    s.push_str(body);
    let _ = stream.write_all(s.as_bytes());
}

pub fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

pub fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (decode(k), decode(v)))
        .collect()
}

pub fn b64url(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..=c.len() {
            out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

pub fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

fn merged(mut base: Value, extra: &Map<String, Value>) -> Value {
    for (k, v) in extra {
        if v.is_null() {
            base.as_object_mut().unwrap().remove(k);
        } else {
            base[k] = v.clone();
        }
    }
    base
}

fn handle(mut stream: TcpStream, st: &Arc<(Mutex<MockState>, Condvar)>, base: &str) {
    let Some(req) = read_request(&mut stream) else {
        return;
    };
    let (m, cv) = &**st;
    let json_ct = || vec![("Content-Type", "application/json".to_string())];
    let issuer_path = m.lock().unwrap().issuer_path.clone();
    let asm_path = format!("/.well-known/oauth-authorization-server{issuer_path}");
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/.well-known/oauth-protected-resource/mcp") => {
            let body = json!({
                "resource": format!("{base}/mcp"),
                "authorization_servers": [format!("{base}{issuer_path}")],
                "scopes_supported": ["orchestration:read", "orchestration:operate"],
                "bearer_methods_supported": ["header"],
            });
            let body = merged(body, &m.lock().unwrap().prm_override);
            respond(&mut stream, 200, &json_ct(), &body.to_string());
        }
        ("GET", p) if p == asm_path => {
            let body = json!({
                "issuer": format!("{base}{issuer_path}"),
                "authorization_endpoint": format!("{base}/oauth/authorize"),
                "token_endpoint": format!("{base}/oauth/token"),
                "registration_endpoint": format!("{base}/oauth/register"),
                "response_types_supported": ["code"],
                "grant_types_supported": ["authorization_code", "refresh_token"],
                "code_challenge_methods_supported": ["S256"],
                "token_endpoint_auth_methods_supported": ["none"],
            });
            let body = merged(body, &m.lock().unwrap().asm_override);
            respond(&mut stream, 200, &json_ct(), &body.to_string());
        }
        ("POST", "/oauth/register") => {
            let v: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
            m.lock().unwrap().register_bodies.push(v.clone());
            let body = json!({ "client_id": "client-1", "redirect_uris": v["redirect_uris"], "token_endpoint_auth_method": "none" });
            respond(&mut stream, 201, &json_ct(), &body.to_string());
        }
        ("GET", "/oauth/authorize") => {
            let q = parse_query(&req.query);
            let mut s = m.lock().unwrap();
            s.authorize_queries.push(q.clone());
            if q.get("scope").map(String::as_str) != Some("orchestration:read") {
                respond(&mut stream, 400, &[], "bad scope");
                return;
            }
            let code = format!("code-{}", s.codes.len() + 1);
            s.codes.insert(
                code.clone(),
                (
                    q["code_challenge"].clone(),
                    q["redirect_uri"].clone(),
                    q["client_id"].clone(),
                ),
            );
            let iss = format!("{base}{issuer_path}").replace(':', "%3A").replace('/', "%2F");
            let loc = format!("{}?code={code}&state={}&iss={iss}", q["redirect_uri"], q["state"]);
            respond(&mut stream, 302, &[("Location", loc)], "");
        }
        ("POST", "/oauth/token") => {
            let f = parse_query(&String::from_utf8_lossy(&req.body));
            let mut s = m.lock().unwrap();
            if let Some(body) = s.token_error_body.clone() {
                respond(&mut stream, 400, &json_ct(), &body);
                return;
            }
            if f.get("grant_type").map(String::as_str) == Some("refresh_token") {
                *s.calls.entry("refresh".into()).or_default() += 1;
                let ok = s.issue_refresh.is_some() && f.get("refresh_token") == s.issue_refresh.as_ref();
                match (ok, s.refresh_to.clone()) {
                    (true, Some(new)) => {
                        let body = json!({ "access_token": new, "token_type": "Bearer", "expires_in": 3600, "scope": "orchestration:read" });
                        respond(&mut stream, 200, &json_ct(), &body.to_string());
                    }
                    _ => respond(&mut stream, 400, &json_ct(), r#"{"error":"invalid_grant"}"#),
                }
                return;
            }
            let ok = s
                .codes
                .get(f.get("code").map(String::as_str).unwrap_or(""))
                .is_some_and(|(challenge, redirect, client)| {
                    let verifier = f.get("code_verifier").cloned().unwrap_or_default();
                    let computed = b64url(ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes()).as_ref());
                    &computed == challenge
                        && f.get("redirect_uri") == Some(redirect)
                        && f.get("client_id") == Some(client)
                        && f.get("grant_type").map(String::as_str) == Some("authorization_code")
                });
            if ok {
                let mut body = json!({ "access_token": s.token, "token_type": "Bearer", "expires_in": 3600, "scope": "orchestration:read" });
                if let Some(rt) = &s.issue_refresh {
                    body["refresh_token"] = json!(rt);
                }
                respond(&mut stream, 200, &json_ct(), &body.to_string());
            } else {
                respond(&mut stream, 400, &json_ct(), r#"{"error":"invalid_grant"}"#);
            }
        }
        ("POST", "/mcp") => handle_mcp(stream, req, m, cv),
        _ => respond(&mut stream, 404, &[], "not found"),
    }
}

fn handle_mcp(mut stream: TcpStream, req: Req, m: &Mutex<MockState>, cv: &Condvar) {
    let msg: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    let method = msg["method"].as_str().unwrap_or("").to_string();
    let mut s = m.lock().unwrap();
    if let Some((status, body)) = s.fail_queue.pop_front() {
        *s.calls.entry(format!("failed:{status}")).or_default() += 1;
        drop(s);
        respond(&mut stream, status, &[("Content-Type", "text/html".into())], &body);
        return;
    }
    let auth = req.headers.get("authorization").cloned().unwrap_or_default();
    let presented = auth.strip_prefix("Bearer ").unwrap_or("");
    if presented != s.token && !s.extra_tokens.contains(presented) {
        *s.calls.entry("unauthorized".into()).or_default() += 1;
        drop(s);
        respond(
            &mut stream,
            401,
            &[("Content-Type", "application/json".into())],
            r#"{"error":"invalid_mcp_credential"}"#,
        );
        return;
    }
    let session = req.headers.get("mcp-session-id").cloned();
    if method == "initialize" {
        *s.calls.entry("initialize".into()).or_default() += 1;
        s.next_session += 1;
        let sid = format!("session-{}", s.next_session);
        s.sessions.insert(sid.clone());
        let (sse, version) = (s.sse, s.protocol_version.clone());
        drop(s);
        let result = json!({ "jsonrpc": "2.0", "id": msg["id"], "result": {
            "protocolVersion": version, "capabilities": { "tools": {} }, "serverInfo": { "name": "mock-t3", "version": "0" } } });
        send_result(&mut stream, &result, sse, Some(&sid));
        return;
    }
    if !session.as_ref().is_some_and(|sid| s.sessions.contains(sid)) {
        *s.calls.entry("expired-session".into()).or_default() += 1;
        drop(s);
        respond(&mut stream, 404, &[], "unknown session");
        return;
    }
    if method == "notifications/initialized" {
        *s.calls.entry("initialized".into()).or_default() += 1;
        drop(s);
        respond(&mut stream, 202, &[], "");
        return;
    }
    if let Some(status) = s.reject_tools_with {
        *s.calls.entry(format!("rejected:{status}")).or_default() += 1;
        drop(s);
        respond(&mut stream, status, &[], "invalid environmentId");
        return;
    }
    let tool = msg["params"]["name"].as_str().unwrap_or("").to_string();
    let args = msg["params"]["arguments"].clone();
    *s.calls.entry(tool.clone()).or_default() += 1;
    if let Some(n) = s.sse_flood {
        drop(s);
        let mut head = String::from("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n");
        head.push_str(": flood\n");
        let _ = stream.write_all(head.as_bytes());
        let chunk = ": keepalive keepalive keepalive keepalive\n".repeat(1000);
        let mut sent = 0;
        while sent < n {
            if stream.write_all(chunk.as_bytes()).is_err() {
                break;
            }
            sent += chunk.len();
        }
        return;
    }
    let tid = args["threadId"].as_str().unwrap_or("").to_string();
    let sse = s.sse;
    let tool_result = match tool.as_str() {
        "t3_thread_wait" => {
            s.wait_args.push(args.clone());
            let delegated = args["untilDelegatedWorkSettled"] == true;
            let deadline = Instant::now() + Duration::from_millis(args["timeoutMs"].as_u64().unwrap_or(1000));
            let answer = loop {
                match s.threads.get(&tid).cloned() {
                    None => break Err(()),
                    Some((status, ord)) => {
                        let busy = is_active(&status) || (delegated && s.delegated_pending.contains(&tid));
                        if !busy || Instant::now() >= deadline {
                            break Ok(
                                json!({ "threadId": tid, "runId": format!("run:thread:{tid}:ordinal:{ord}"),
                                             "status": status, "timedOut": busy }),
                            );
                        }
                        let left = deadline.saturating_duration_since(Instant::now());
                        s = cv.wait_timeout(s, left).unwrap().0;
                    }
                }
            };
            if s.advance_after_wait.as_ref().is_some_and(|a| a.0 == tid) {
                let (_, status, ord, pending) = s.advance_after_wait.take().unwrap();
                s.threads.insert(tid.clone(), (status, ord));
                if pending {
                    s.delegated_pending.insert(tid.clone());
                }
            }
            answer
        }
        "t3_thread_read" => match s.threads.get(&tid) {
            None => Err(()),
            Some((status, ord)) => Ok(json!({
                "thread": { "threadId": tid, "title": "mock thread", "status": status,
                            "latestRunId": format!("run:thread:{tid}:ordinal:{ord}"),
                            "link": format!("<t3-thread://v2/env-mock/{tid}>") },
                "recentRuns": [], "items": [], "hasMore": false })),
        },
        _ => Err(()),
    };
    drop(s);
    let result = match tool_result {
        Ok(v) => {
            json!({ "jsonrpc": "2.0", "id": msg["id"], "result": { "content": [{ "type": "text", "text": v.to_string() }] } })
        }
        Err(()) => {
            let err = json!({ "_tag": "OrchestratorMcpFailure", "code": "thread_not_found", "message": format!("Thread {tid} is no longer available.") });
            json!({ "jsonrpc": "2.0", "id": msg["id"], "result": { "isError": true, "content": [{ "type": "text", "text": err.to_string() }] } })
        }
    };
    send_result(&mut stream, &result, sse, None);
}

fn send_result(stream: &mut TcpStream, result: &Value, sse: bool, session: Option<&str>) {
    let mut headers = vec![];
    if let Some(sid) = session {
        headers.push(("Mcp-Session-Id", sid.to_string()));
    }
    if sse {
        let mut s = String::from(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n",
        );
        for (k, v) in &headers {
            s.push_str(&format!("{k}: {v}\r\n"));
        }
        s.push_str("\r\n");
        s.push_str(": keepalive\n\n");
        s.push_str("event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{\"level\":\"info\",\"data\":\"working\"}}\n\n");
        s.push_str(&format!("event: message\ndata: {result}\n\n"));
        let _ = stream.write_all(s.as_bytes());
    } else {
        headers.push(("Content-Type", "application/json".into()));
        respond(stream, 200, &headers, &result.to_string());
    }
}

// ---- webhook receiver --------------------------------------------------------

#[derive(Clone)]
pub struct HookReq {
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
    pub at: Instant,
}

#[derive(Default)]
pub struct HookState {
    pub requests: Vec<HookReq>,
    /// Responses to give, in order; then 200.
    pub script: VecDeque<(u16, Vec<(String, String)>)>,
}

pub struct MockHook {
    pub url: String,
    pub state: Arc<Mutex<HookState>>,
}

impl MockHook {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://127.0.0.1:{}/hooks/secret-path-123",
            listener.local_addr().unwrap().port()
        );
        let state = Arc::new(Mutex::new(HookState::default()));
        let st = state.clone();
        thread::spawn(move || {
            for mut conn in listener.incoming().flatten() {
                let Some(req) = read_request(&mut conn) else { continue };
                let (status, headers) = {
                    let mut s = st.lock().unwrap();
                    s.requests.push(HookReq {
                        path: req.path.clone(),
                        headers: req.headers.clone(),
                        body: req.body.clone(),
                        at: Instant::now(),
                    });
                    s.script.pop_front().unwrap_or((200, vec![]))
                };
                let hs: Vec<(&str, String)> = headers.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
                respond(&mut conn, status, &hs, "receiver body that must never be echoed");
            }
        });
        MockHook { url, state }
    }

    pub fn push(&self, status: u16, headers: &[(&str, &str)]) {
        self.state.lock().unwrap().script.push_back((
            status,
            headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        ));
    }

    pub fn requests(&self) -> Vec<HookReq> {
        self.state.lock().unwrap().requests.clone()
    }
}

// ---- environment -----------------------------------------------------------

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// An isolated T3_WATCH_HOME, so each test gets its own daemon and config.
/// Kept short because Unix socket paths are limited to ~104 bytes.
pub struct Env {
    pub home: PathBuf,
}

impl Env {
    pub fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let home = PathBuf::from(format!("/tmp/t3wt-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        Env { home }
    }

    pub fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env("T3_WATCH_HOME", &self.home)
            .env("T3_WATCH_IDLE_EXIT", "2s")
            .env_remove("T3_WATCH_SOCKET")
            .env_remove("T3_WATCH_NO_DAEMON")
            .env_remove("T3_WATCH_T3_BIN")
            .env_remove("T3_WATCH_MAX_WATCHES")
            .env_remove("T3_WATCH_MAX_CONNECTIONS")
            .env("T3CODE_HOME", self.home.join("no-t3"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    pub fn spawn(&self, args: &[&str]) -> Child {
        self.cmd(args).spawn().unwrap()
    }

    /// Saves a machine directly, skipping OAuth.
    pub fn add_machine(&self, name: &str, origin: &str, token: &str) {
        let cfg = json!({ "default": null, "machines": { name: { "origin": origin, "accessToken": token, "scope": "orchestration:read" } } });
        fs::write(self.home.join("machines.json"), cfg.to_string()).unwrap();
    }

    pub fn machines(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.home.join("machines.json")).unwrap()).unwrap()
    }

    pub fn status(&self) -> Value {
        let out = self.run(&["status"]);
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }

    /// Polls `t3-watch status` until `pred` holds.
    pub fn wait_status(&self, what: &str, timeout: Duration, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let s = self.status();
            if pred(&s) {
                return s;
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for {what}; last status: {s}");
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    pub fn subscribers(s: &Value) -> u64 {
        s["watches"]
            .as_array()
            .map(|w| w.iter().map(|w| w["subscribers"].as_u64().unwrap_or(0)).sum())
            .unwrap_or(0)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.cmd(&["stop"]).output();
        let _ = fs::remove_dir_all(&self.home);
    }
}

/// Waits for a child with a deadline, killing it on timeout.
pub fn finish(mut child: Child, timeout: Duration) -> (i32, Value, String) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let mut out = String::new();
            let mut err = String::new();
            if let Some(mut o) = child.stdout.take() {
                o.read_to_string(&mut out).unwrap();
            }
            if let Some(mut e) = child.stderr.take() {
                e.read_to_string(&mut err).unwrap();
            }
            let v = serde_json::from_str(out.trim()).unwrap_or(Value::Null);
            return (status.code().unwrap_or(-1), v, err);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("t3-watch did not exit within {timeout:?}");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Asserts the child is still running after `d`.
pub fn still_running(child: &mut Child, d: Duration) {
    thread::sleep(d);
    assert!(child.try_wait().unwrap().is_none(), "t3-watch exited early");
}

pub fn output_json(o: &Output) -> Value {
    serde_json::from_slice(&o.stdout)
        .unwrap_or_else(|_| panic!("stdout is not JSON: {}", String::from_utf8_lossy(&o.stdout)))
}

/// A fake `t3` CLI: reads `<dir>/threads/<id>` ("status ordinal") and counts
/// its invocations in `<dir>/calls`.
pub struct MockT3 {
    pub dir: PathBuf,
    pub bin: PathBuf,
}

impl MockT3 {
    pub fn new(env: &Env) -> Self {
        Self::in_dir(env.home.join("mock-t3"))
    }

    pub fn in_dir(dir: PathBuf) -> Self {
        fs::create_dir_all(dir.join("threads")).unwrap();
        let bin = dir.join("t3");
        let script = format!(
            r#"#!/bin/sh
echo call >> '{d}/calls'
for a; do last="$a"; done
f='{d}/threads/'"$last"
if [ ! -f "$f" ]; then echo "Thread $last is no longer available." >&2; exit 1; fi
read status ord < "$f"
printf '{{"thread":{{"threadId":"%s","title":"mock local","status":"%s","latestRunId":"run:thread:%s:ordinal:%s","pendingRequestCount":0}},"messages":[]}}\n' "$last" "$status" "$last" "$ord"
"#,
            d = dir.display()
        );
        fs::write(&bin, script).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        MockT3 { dir, bin }
    }

    pub fn set(&self, id: &str, status: &str, ordinal: u64) {
        let tmp = self.dir.join("tmp");
        fs::write(&tmp, format!("{status} {ordinal}\n")).unwrap();
        fs::rename(tmp, self.dir.join("threads").join(id)).unwrap();
    }

    pub fn calls(&self) -> usize {
        fs::read_to_string(self.dir.join("calls"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    pub fn bin_str(&self) -> &str {
        self.bin.to_str().unwrap()
    }
}

/// Minimal HTTP GET for driving the OAuth "browser" in tests. Returns
/// (status, Location header, body).
pub fn http_get_full(url: &str) -> (u16, Option<String>, String) {
    let rest = url.strip_prefix("http://").expect("http URL");
    let (host, path) = rest
        .split_once('/')
        .map(|(h, p)| (h, format!("/{p}")))
        .unwrap_or((rest, "/".into()));
    let mut s = TcpStream::connect(host).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status = resp.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let location = resp
        .lines()
        .find_map(|l| l.strip_prefix("Location: ").map(str::to_string));
    let body = resp
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, location, body)
}

pub fn http_get(url: &str) -> (u16, Option<String>) {
    let (s, l, _) = http_get_full(url);
    (s, l)
}

pub fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Writes an executable shell script.
pub fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
