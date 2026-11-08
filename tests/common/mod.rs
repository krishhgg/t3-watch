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
/// This build's engine revision, its daemon socket under `T3_WATCH_HOME`,
/// and its outbox directory.
pub const ENGINE: u64 = 15;
pub const SOCK: &str = "daemon-e15.sock";
pub const OUTBOX: &str = "outbox-v4";
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
    /// `t3_thread_wait` requests blocked right now, and the most at once.
    pub active_waits: usize,
    pub max_active_waits: usize,
    /// `requestId`s from `notifications/cancelled`.
    pub cancelled_ids: Vec<Value>,
    /// JSON-RPC responses the client POSTed (answers to server requests).
    pub client_responses: Vec<Value>,
    /// Requests sent to the client inside SSE answers to tool calls.
    pub server_requests: Vec<Value>,
    /// Fields merged into every `t3_thread_wait` result.
    pub wait_override: Map<String, Value>,
    /// Fields merged into every `t3_thread_read` thread (null removes).
    pub read_override: Map<String, Value>,
    /// HTTP statuses the refresh grant returns, in order, before it works.
    pub refresh_failures: VecDeque<u16>,
    /// Each good refresh issues token-rN / rt-rN and revokes the old pair.
    pub rotate_refresh: bool,
    pub rotations: u64,
    /// ("wait", request id) when a `t3_thread_wait` arrives and
    /// ("cancelled", request id) for each cancellation, in arrival order.
    pub events: Vec<(String, Value)>,
    /// Arguments of every `t3_thread_read`, in order.
    pub read_args: Vec<Value>,
    /// Arguments of every `t3_pending_request`, in order.
    pub pending_args: Vec<Value>,
    /// threadId -> activity timeline items (`view: activity`).
    pub activity: HashMap<String, Vec<Value>>,
    /// threadId -> message items (`view: messages`, and `itemId` reads).
    pub messages: HashMap<String, Vec<Value>>,
    /// threadId -> ids `t3_pending_request list` returns.
    pub pending_ids: HashMap<String, Vec<String>>,
    /// threadId -> pendingRequestCount in reads.
    pub pending_count: HashMap<String, u64>,
    /// threadId -> parentThreadId in reads (a delegated child).
    pub parent: HashMap<String, String>,
    /// Leave `resultRun` out of settled delegated-work waits (an old T3).
    pub omit_result_run: bool,
    /// Turn-mode probes (`timeoutMs: 1` with `untilDelegatedWorkSettled`).
    pub probes: usize,
    /// After the Nth `t3_thread_read` (1-based) has been answered, move the
    /// thread to (status, ordinal): a run that starts between two reads of
    /// one cycle.
    pub after_read: HashMap<usize, (String, String, u64)>,
    /// Reads of a delegated child that returned one of its assistant
    /// messages: T3 marks a child's final result delivered when it is read.
    pub child_final_reads: usize,
    /// requestId -> what `t3_pending_request read` returns.
    pub pending_reads: HashMap<String, Value>,
    /// Fields merged into every turn-mode probe answer (null removes).
    pub probe_override: Map<String, Value>,
    /// `completedAt` of the latest run in header reads' `recentRuns`.
    pub completed_at: Option<String>,
    /// The next this many long-polls never answer (a connection that died
    /// while the host slept); they end only when the client hangs up.
    pub hang_waits: usize,
    /// After the Nth `t3_thread_read` (1-based) has been answered, write
    /// this many ms to this suspend file before the answer goes out: the
    /// host slept while the answer, a snapshot from before, was on its way.
    pub suspend_after_read: HashMap<usize, (PathBuf, u64)>,
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
            active_waits: 0,
            max_active_waits: 0,
            cancelled_ids: Vec::new(),
            client_responses: Vec::new(),
            server_requests: Vec::new(),
            wait_override: Map::new(),
            read_override: Map::new(),
            refresh_failures: VecDeque::new(),
            rotate_refresh: false,
            rotations: 0,
            events: Vec::new(),
            read_args: Vec::new(),
            pending_args: Vec::new(),
            activity: HashMap::new(),
            messages: HashMap::new(),
            pending_ids: HashMap::new(),
            pending_count: HashMap::new(),
            parent: HashMap::new(),
            omit_result_run: false,
            probes: 0,
            after_read: HashMap::new(),
            child_final_reads: 0,
            pending_reads: HashMap::new(),
            probe_override: Map::new(),
            completed_at: None,
            hang_waits: 0,
            suspend_after_read: HashMap::new(),
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
                if let Some(status) = s.refresh_failures.pop_front() {
                    let code = if status == 400 {
                        "invalid_grant"
                    } else {
                        "temporarily_unavailable"
                    };
                    respond(&mut stream, status, &json_ct(), &json!({ "error": code }).to_string());
                    return;
                }
                let ok = s.issue_refresh.is_some() && f.get("refresh_token") == s.issue_refresh.as_ref();
                if ok && s.rotate_refresh {
                    s.rotations += 1;
                    let (access, rt) = (format!("token-r{}", s.rotations), format!("rt-r{}", s.rotations));
                    s.token = access.clone();
                    s.issue_refresh = Some(rt.clone());
                    let body = json!({ "access_token": access, "refresh_token": rt, "token_type": "Bearer", "expires_in": 3600 });
                    respond(&mut stream, 200, &json_ct(), &body.to_string());
                    return;
                }
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
            *s.calls.entry("exchange".into()).or_default() += 1;
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
            &[
                ("Content-Type", "application/json".into()),
                ("WWW-Authenticate", r#"Bearer error="invalid_token""#.into()),
            ],
            r#"{"error":"invalid_mcp_credential"}"#,
        );
        return;
    }
    let session = req.headers.get("mcp-session-id").cloned();
    // A strict 2025-06-18 server: every request after initialize must carry
    // the negotiated version.
    if s.protocol_version == "2025-06-18"
        && method != "initialize"
        && req.headers.get("mcp-protocol-version").map(String::as_str) != Some("2025-06-18")
    {
        *s.calls.entry("missing-version-header".into()).or_default() += 1;
        drop(s);
        respond(&mut stream, 400, &[], "missing MCP-Protocol-Version");
        return;
    }
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
    if method == "notifications/cancelled" {
        s.cancelled_ids.push(msg["params"]["requestId"].clone());
        s.events.push(("cancelled".into(), msg["params"]["requestId"].clone()));
        drop(s);
        respond(&mut stream, 202, &[], "");
        return;
    }
    if method.is_empty() && (msg.get("result").is_some() || msg.get("error").is_some()) {
        s.client_responses.push(msg.clone());
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
    let probe = tool == "t3_thread_wait" && args["timeoutMs"] == 1 && args["untilDelegatedWorkSettled"] == true;
    if probe {
        s.probes += 1;
    } else {
        *s.calls.entry(tool.clone()).or_default() += 1;
    }
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
    let server_requests = s.server_requests.clone();
    let tool_result = match tool.as_str() {
        "t3_thread_wait" if probe => match s.threads.get(&tid).cloned() {
            None => Err(()),
            Some((status, ord)) => {
                let busy = is_active(&status) || s.delegated_pending.contains(&tid);
                let mut v = json!({ "threadId": tid, "runId": format!("run:thread:{tid}:ordinal:{ord}"),
                                    "status": status, "timedOut": busy });
                if !busy {
                    v["resultRun"] = json!({ "runId": format!("run:thread:{tid}:ordinal:{ord}"), "status": status });
                }
                Ok(merged(v, &s.probe_override))
            }
        },
        "t3_thread_wait" if s.hang_waits > 0 => {
            s.hang_waits -= 1;
            s.wait_args.push(args.clone());
            *s.calls.entry("hung-wait".into()).or_default() += 1;
            drop(s);
            while !client_gone(&stream) {
                thread::sleep(Duration::from_millis(50));
            }
            return;
        }
        "t3_thread_wait" => {
            s.wait_args.push(args.clone());
            s.events.push(("wait".into(), msg["id"].clone()));
            let delegated = args["untilDelegatedWorkSettled"] == true;
            let deadline = Instant::now() + Duration::from_millis(args["timeoutMs"].as_u64().unwrap_or(1000));
            s.active_waits += 1;
            s.max_active_waits = s.max_active_waits.max(s.active_waits);
            let answer = loop {
                match s.threads.get(&tid).cloned() {
                    None => break Err(()),
                    Some((status, ord)) => {
                        let busy = is_active(&status) || (delegated && s.delegated_pending.contains(&tid));
                        if !busy || Instant::now() >= deadline {
                            let mut v = json!({ "threadId": tid, "runId": format!("run:thread:{tid}:ordinal:{ord}"),
                                                "status": status, "timedOut": busy });
                            // Like T3: a settled delegated-work wait names the
                            // run holding the final answer.
                            if delegated && !busy && !s.omit_result_run {
                                v["resultRun"] =
                                    json!({ "runId": format!("run:thread:{tid}:ordinal:{ord}"), "status": status });
                            }
                            v = merged(v, &s.wait_override);
                            break Ok(v);
                        }
                        // Like a real server, notice a client that hung up.
                        if client_gone(&stream) {
                            s.active_waits -= 1;
                            *s.calls.entry("client-gone".into()).or_default() += 1;
                            return;
                        }
                        let left = deadline
                            .saturating_duration_since(Instant::now())
                            .min(Duration::from_millis(50));
                        s = cv.wait_timeout(s, left).unwrap().0;
                    }
                }
            };
            s.active_waits -= 1;
            if s.advance_after_wait.as_ref().is_some_and(|a| a.0 == tid) {
                let (_, status, ord, pending) = s.advance_after_wait.take().unwrap();
                s.threads.insert(tid.clone(), (status, ord));
                if pending {
                    s.delegated_pending.insert(tid.clone());
                }
            }
            answer
        }
        "t3_thread_read" => {
            s.read_args.push(args.clone());
            match s.threads.get(&tid).cloned() {
                None => Err(()),
                Some((status, ord)) => {
                    let run = format!("run:thread:{tid}:ordinal:{ord}");
                    let items: Vec<Value> = if let Some(item_id) = args["itemId"].as_str() {
                        // One message, paged by textOffset.
                        let max = args["maxCharsPerItem"].as_u64().unwrap_or(50_000) as usize;
                        let offset = args["textOffset"].as_u64().unwrap_or(0) as usize;
                        s.messages
                            .get(&tid)
                            .into_iter()
                            .flatten()
                            .filter(|m| m["itemId"] == item_id)
                            .map(|m| {
                                let text: Vec<char> = m["text"].as_str().unwrap_or("").chars().collect();
                                let end = (offset + max).min(text.len());
                                let mut m = m.clone();
                                m["text"] = json!(text[offset.min(end)..end].iter().collect::<String>());
                                m["textTruncated"] = json!(end < text.len());
                                if end < text.len() {
                                    m["nextTextOffset"] = json!(end);
                                }
                                m
                            })
                            .collect()
                    } else {
                        let after = args["afterPosition"].as_i64().unwrap_or(-1);
                        let source = match args["view"].as_str() {
                            Some("activity") => s.activity.get(&tid).cloned().unwrap_or_default(),
                            _ => s.messages.get(&tid).cloned().unwrap_or_default(),
                        };
                        let max = args["maxCharsPerItem"].as_u64().unwrap_or(50_000) as usize;
                        source
                            .into_iter()
                            .filter(|i| i["position"].as_i64().unwrap_or(0) > after)
                            .take(args["limit"].as_u64().unwrap_or(20) as usize)
                            .map(|mut i| {
                                let text: String = i["text"].as_str().unwrap_or("").chars().take(max).collect();
                                if text.chars().count() < i["text"].as_str().unwrap_or("").chars().count() {
                                    i["textTruncated"] = json!(true);
                                    i["nextTextOffset"] = json!(text.chars().count());
                                }
                                i["text"] = json!(text);
                                i
                            })
                            .collect()
                    };
                    let item_count = s
                        .activity
                        .get(&tid)
                        .map_or(0, Vec::len)
                        .max(s.messages.get(&tid).map_or(0, Vec::len));
                    let mut thread = json!({ "threadId": tid, "title": "mock thread", "status": status,
                                             "latestRunId": run, "runCount": ord, "itemCount": item_count,
                                             "activeRunId": if is_active(&status) { json!(run) } else { Value::Null },
                                             "pendingRequestCount": s.pending_count.get(&tid).copied().unwrap_or(0),
                                             "link": format!("<t3-thread://v2/env-mock/{tid}>") });
                    if let Some(parent) = s.parent.get(&tid) {
                        thread["parentThreadId"] = json!(parent);
                        thread["relationshipToParent"] = json!("subagent");
                        if items.iter().any(|i| i["type"] == "assistant_message") {
                            s.child_final_reads += 1;
                        }
                    }
                    let n = s.read_args.len();
                    if let Some((t, status, ord)) = s.after_read.remove(&n) {
                        s.threads.insert(t, (status, ord));
                    }
                    if let Some((file, ms)) = s.suspend_after_read.remove(&n) {
                        fs::write(file, ms.to_string()).unwrap();
                    }
                    let recent = match &s.completed_at {
                        Some(at) if !is_active(&status) => {
                            json!([{ "runId": run, "ordinal": ord, "status": status, "completedAt": at }])
                        }
                        _ => json!([]),
                    };
                    Ok(json!({ "thread": merged(thread, &s.read_override),
                               "recentRuns": recent, "items": items, "hasMore": false }))
                }
            }
        }
        "t3_pending_request" => {
            s.pending_args.push(args.clone());
            match args["action"].as_str() {
                Some("list") => Ok(json!({ "requestIds": s.pending_ids.get(&tid).cloned().unwrap_or_default() })),
                Some("read") => args["requestId"]
                    .as_str()
                    .and_then(|id| s.pending_reads.get(id).cloned())
                    .ok_or(()),
                _ => Err(()),
            }
        }
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
    send_result_with(&mut stream, &result, sse, None, &server_requests);
}

/// True once the client has closed its end of the connection.
fn client_gone(stream: &TcpStream) -> bool {
    let mut b = [0u8; 1];
    let _ = stream.set_nonblocking(true);
    let gone = match stream.peek(&mut b) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => e.kind() != std::io::ErrorKind::WouldBlock,
    };
    let _ = stream.set_nonblocking(false);
    gone
}

fn send_result(stream: &mut TcpStream, result: &Value, sse: bool, session: Option<&str>) {
    send_result_with(stream, result, sse, session, &[]);
}

fn send_result_with(stream: &mut TcpStream, result: &Value, sse: bool, session: Option<&str>, requests: &[Value]) {
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
        for r in requests {
            s.push_str(&format!("event: message\ndata: {r}\n\n"));
        }
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

/// How the receiver treats a request.
#[derive(Clone, Copy, PartialEq, Default)]
pub enum HookMode {
    /// Record it and answer from `script` (then 200).
    #[default]
    Normal,
    /// Accept the connection and never read or answer: the request was
    /// never processed.
    Blackhole,
    /// Record it, then close without answering: the receiver has the event
    /// but the sender never learns that.
    DropAfterRecord,
}

#[derive(Default)]
pub struct HookState {
    pub requests: Vec<HookReq>,
    /// Responses to give, in order; then 200.
    pub script: VecDeque<(u16, Vec<(String, String)>)>,
    pub mode: HookMode,
    /// Connections accepted and left hanging (Blackhole).
    pub held: Vec<TcpStream>,
    /// Delays before answering, in order, one per request (after it is
    /// recorded); then none.
    pub delays: VecDeque<Duration>,
}

pub struct MockHook {
    pub url: String,
    pub port: u16,
    pub state: Arc<Mutex<HookState>>,
}

impl MockHook {
    pub fn start() -> Self {
        Self::start_on(0)
    }

    /// A receiver on a given port (0: any), so a test can bring one up where
    /// an earlier webhook URL pointed.
    pub fn start_on(port: u16) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/hooks/secret-path-123");
        let state = Arc::new(Mutex::new(HookState::default()));
        let st = state.clone();
        thread::spawn(move || {
            for mut conn in listener.incoming().flatten() {
                let st = st.clone();
                thread::spawn(move || {
                    if st.lock().unwrap().mode == HookMode::Blackhole {
                        st.lock().unwrap().held.push(conn);
                        return;
                    }
                    let Some(req) = read_request(&mut conn) else { return };
                    let (status, headers, mode) = {
                        let mut s = st.lock().unwrap();
                        s.requests.push(HookReq {
                            path: req.path.clone(),
                            headers: req.headers.clone(),
                            body: req.body.clone(),
                            at: Instant::now(),
                        });
                        let (status, headers) = s.script.pop_front().unwrap_or((200, vec![]));
                        let delay = s.delays.pop_front();
                        drop(s);
                        if let Some(d) = delay {
                            thread::sleep(d);
                        }
                        (status, headers, st.lock().unwrap().mode)
                    };
                    if mode == HookMode::DropAfterRecord {
                        let _ = conn.shutdown(std::net::Shutdown::Both);
                        return;
                    }
                    let hs: Vec<(&str, String)> = headers.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
                    respond(&mut conn, status, &hs, "receiver body that must never be echoed");
                });
            }
        });
        MockHook { url, port, state }
    }

    pub fn set_mode(&self, mode: HookMode) {
        let mut s = self.state.lock().unwrap();
        s.mode = mode;
        if mode != HookMode::Blackhole {
            s.held.clear();
        }
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
            .env_remove("T3_WATCH_CODEX_SOCKET")
            .env("CODEX_HOME", self.home.join("no-codex"))
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
st='{d}/stale/'"$last"
if [ -f "$st" ]; then
  read status ord next_status next_ord suspend_file suspend_ms < "$st"
  rm -f "$st"
  printf '%s %s\n' "$next_status" "$next_ord" > "$f"
  printf '%s' "$suspend_ms" > "$suspend_file"
fi
m='{d}/messages/'"$last"
msgs='[]'
if [ -f "$m" ]; then msgs=$(cat "$m"); fi
printf '{{"thread":{{"threadId":"%s","title":"mock local","status":"%s","latestRunId":"run:thread:%s:ordinal:%s","pendingRequestCount":0}},"messages":%s}}\n' "$last" "$status" "$last" "$ord" "$msgs"
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

    /// The next `t3` call for this thread prints `then` (a snapshot from
    /// before a suspend), while the thread moves to `now` and the suspend
    /// file to `suspend_ms` before the output goes out.
    pub fn stale_once(&self, id: &str, then: (&str, u64), now: (&str, u64), suspend: &Path, suspend_ms: u64) {
        fs::create_dir_all(self.dir.join("stale")).unwrap();
        let tmp = self.dir.join("stale-tmp");
        fs::write(
            &tmp,
            format!(
                "{} {} {} {} {} {}\n",
                then.0,
                then.1,
                now.0,
                now.1,
                suspend.display(),
                suspend_ms
            ),
        )
        .unwrap();
        fs::rename(tmp, self.dir.join("stale").join(id)).unwrap();
    }

    /// What `t3 thread show --messages` prints for this thread.
    pub fn set_messages(&self, id: &str, messages: &Value) {
        fs::create_dir_all(self.dir.join("messages")).unwrap();
        fs::write(self.dir.join("messages").join(id), messages.to_string()).unwrap();
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

/// Number of threads in process `pid`, from the OS.
pub fn thread_count(pid: u32) -> usize {
    if cfg!(target_os = "linux") {
        fs::read_dir(format!("/proc/{pid}/task"))
            .map(|d| d.count())
            .unwrap_or(0)
    } else {
        let out = Command::new("ps")
            .args(["-M", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).lines().count().saturating_sub(1)
    }
}

/// Open file descriptors of process `pid`, from the OS.
pub fn fd_count(pid: u32) -> usize {
    if cfg!(target_os = "linux") {
        fs::read_dir(format!("/proc/{pid}/fd")).map(|d| d.count()).unwrap_or(0)
    } else {
        let out = Command::new("lsof")
            .args(["-n", "-P", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .skip(1)
            .filter(|l| {
                l.split_whitespace()
                    .nth(3)
                    .is_some_and(|fd| fd.chars().next().is_some_and(|c| c.is_ascii_digit()))
            })
            .count()
    }
}

/// TCP connections of process `pid` to `port`, from the OS.
pub fn tcp_connections_to(pid: u32, port: u16) -> usize {
    if cfg!(target_os = "linux") {
        // Count socket inodes of the process that /proc/net/tcp shows connected to the port.
        let inodes: std::collections::HashSet<String> = fs::read_dir(format!("/proc/{pid}/fd"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| fs::read_link(e.path()).ok())
            .filter_map(|l| {
                l.to_str()
                    .and_then(|s| s.strip_prefix("socket:["))
                    .map(|s| s.trim_end_matches(']').to_string())
            })
            .collect();
        let table = fs::read_to_string("/proc/net/tcp").unwrap_or_default();
        table
            .lines()
            .skip(1)
            .filter(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                f.len() > 9 && f[2].ends_with(&format!(":{port:04X}")) && f[3] == "01" && inodes.contains(f[9])
            })
            .count()
    } else {
        let out = Command::new("lsof")
            .args([
                "-n",
                "-P",
                "-a",
                "-p",
                &pid.to_string(),
                &format!("-iTCP:{port}"),
                "-sTCP:ESTABLISHED",
            ])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).lines().skip(1).count()
    }
}

/// Writes an executable shell script.
pub fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
