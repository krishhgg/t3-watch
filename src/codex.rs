//! Native Codex threads (experimental; see `docs/native-sessions.md`),
//! watched read-only through the Codex app-server's documented JSON-RPC
//! protocol on its WebSocket-over-Unix-socket transport: the socket of
//! `codex app-server --listen unix://…`, such as the shared daemon's
//! control socket.
//!
//! Only `initialize`, `thread/read`, `thread/turns/list` and `hooks/list`
//! are sent, `thread/turns/list` with `itemsView: "notLoaded"` so that no
//! message text is read.
//! The app-server also sends every initialized connection each thread's
//! `thread/status/changed` notifications, without any subscription; they
//! are what tells one request from the next (see `record_requests`).
//! t3-watch never resumes, subscribes, starts, steers or interrupts a turn,
//! and never answers a server request. With codex-cli 0.166.0 a
//! `thread/resume` makes the caller a recipient of the owner's approval
//! requests, so a watcher that resumed could approve; the read-only calls
//! changed nothing on disk and delivered no request. The runtime status
//! (`active` with `waitingOnApproval` or `waitingOnUserInput`) comes from
//! the app-server that runs the thread, so the socket must be that one's.
//!
//! The app-server keeps no record of past requests. Codex's documented
//! `PermissionRequest` hook does: `t3-watch hook codex`, installed by the
//! user as an async hook (`--print-settings`), appends one line per approval
//! request (time, turn id, tool name) to
//! `<state>/native/codex/<session>.requests.jsonl`. An async hook cannot
//! approve, deny or change anything. The counting connection reconciles
//! that log with what it saw live (see `record_requests`), so a request that
//! opened and closed while nothing counted is still reported.

use crate::cancel::Cancel;
use crate::config;
use crate::mcp::CallError;
use crate::model::{RecoveredRequest, ThreadState};
use crate::util;
use serde_json::{json, Value};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Target prefix: `codex://threads/<id>`.
pub const CODEX_PREFIX: &str = "codex://threads/";
const CALL_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_MESSAGE: usize = 16 << 20;
const TURN_PAGE: u32 = 50;
/// How long a connection is kept after its thread's last wait ended while a
/// turn was running, so that requests opened in between are still counted.
const LINGER: Duration = Duration::from_secs(15 * 60);
/// How long `thread/turns/list` answering "list_turns is not supported
/// yet" is retried (a thread materializing its first message) before it
/// fails the wait.
const UNSUPPORTED_RETRY: Duration = Duration::from_secs(30);
/// How long a `hooks/list` answer is used before it is asked again.
const HOOKS_RECHECK: Duration = Duration::from_secs(60);
/// The most of a hook's stdin read (an `apply_patch` input can be large).
const MAX_HOOK_INPUT: u64 = 16 << 20;
/// The most of a hook log read at once, from the latest turn's first record.
const MAX_HOOK_READ: u64 = 4 << 20;
/// A hook record lands this long after its request was counted live, at
/// most, and is still matched to it.
const HOOK_LAG_MS: u64 = 60_000;
/// Recovered requests kept per turn.
const MAX_RECOVERED: usize = 100;

pub fn valid_thread_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn codex_thread_id(id: &str) -> String {
    format!("{CODEX_PREFIX}{id}")
}

/// The app-server socket to read from: `$T3_WATCH_CODEX_SOCKET`, else the
/// shared daemon's control socket under `$CODEX_HOME` (default `~/.codex`).
pub fn default_socket() -> PathBuf {
    if let Some(p) = std::env::var_os("T3_WATCH_CODEX_SOCKET").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("CODEX_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| config::home_dir().join(".codex"));
    home.join("app-server-control").join("app-server-control.sock")
}

// ---- a minimal WebSocket client (RFC 6455), text frames only -------------

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n =
            (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

struct Ws {
    s: UnixStream,
    buf: Vec<u8>,
}

impl Ws {
    fn connect(path: &Path) -> Result<Ws, String> {
        let s = UnixStream::connect(path)
            .map_err(|e| format!("cannot reach the Codex app-server at {}: {e}", path.display()))?;
        s.set_read_timeout(Some(Duration::from_millis(250)))
            .map_err(|e| e.to_string())?;
        s.set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| e.to_string())?;
        let key = base64(&util::random_bytes(16));
        let mut ws = Ws { s, buf: Vec::new() };
        let req = format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        ws.s.write_all(req.as_bytes())
            .map_err(|e| format!("cannot open the WebSocket: {e}"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ws.buf.windows(4).any(|w| w == b"\r\n\r\n") {
            if ws.buf.len() > 16 << 10 || Instant::now() > deadline {
                return Err("the Codex app-server did not complete the WebSocket handshake".into());
            }
            ws.fill()?;
        }
        let end = ws.buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&ws.buf[..end]).to_ascii_lowercase();
        ws.buf.drain(..end);
        let digest = ring::digest::digest(
            &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
            format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
        );
        let accept = base64(digest.as_ref()).to_ascii_lowercase();
        if !head.starts_with("http/1.1 101") || !head.contains(&format!("sec-websocket-accept: {accept}")) {
            return Err("the Codex app-server refused the WebSocket handshake".into());
        }
        Ok(ws)
    }

    fn set_read_timeout(&self, d: Duration) {
        let _ = self.s.set_read_timeout(Some(d));
    }

    /// Reads what is there into the buffer. A read timeout is not an error.
    fn fill(&mut self) -> Result<(), String> {
        let mut chunk = [0u8; 16 << 10];
        match self.s.read(&mut chunk) {
            Ok(0) => Err("the Codex app-server closed the connection".into()),
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                Ok(())
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(())
            }
            Err(e) => Err(format!("cannot read from the Codex app-server: {e}")),
        }
    }

    fn frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        let mut f = vec![0x80 | opcode];
        let n = payload.len();
        if n < 126 {
            f.push(0x80 | n as u8);
        } else if n < 65536 {
            f.push(0x80 | 126);
            f.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            f.push(0x80 | 127);
            f.extend_from_slice(&(n as u64).to_be_bytes());
        }
        let mask = util::random_bytes(4);
        f.extend_from_slice(&mask);
        f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.s
            .write_all(&f)
            .map_err(|e| format!("cannot write to the Codex app-server: {e}"))
    }

    fn send(&mut self, v: &Value) -> Result<(), String> {
        self.frame(0x1, v.to_string().as_bytes())
    }

    /// The next whole text message, before `deadline`; `Ok(None)` when it
    /// passes, or the operation is cancelled.
    fn recv(&mut self, deadline: Instant, cancel: &Cancel) -> Result<Option<Vec<u8>>, String> {
        let mut message = Vec::new();
        loop {
            // One frame: header, extended length, mask, payload.
            let have = |buf: &Vec<u8>, n: usize| buf.len() >= n;
            while !have(&self.buf, 2) {
                if cancel.is_cancelled() || Instant::now() > deadline {
                    return Ok(None);
                }
                self.fill()?;
            }
            let (b0, b1) = (self.buf[0], self.buf[1]);
            let mut at = 2;
            let mut len = u64::from(b1 & 0x7F);
            let ext = match len {
                126 => 2,
                127 => 8,
                _ => 0,
            };
            let masked = b1 & 0x80 != 0;
            let header = at + ext + if masked { 4 } else { 0 };
            while !have(&self.buf, header) {
                if cancel.is_cancelled() || Instant::now() > deadline {
                    return Ok(None);
                }
                self.fill()?;
            }
            if ext > 0 {
                len = self.buf[at..at + ext]
                    .iter()
                    .fold(0u64, |a, b| (a << 8) | u64::from(*b));
                at += ext;
            }
            if len as usize > MAX_MESSAGE || message.len() + len as usize > MAX_MESSAGE {
                return Err("a Codex app-server message is too large".into());
            }
            let mask: Option<[u8; 4]> =
                masked.then(|| [self.buf[at], self.buf[at + 1], self.buf[at + 2], self.buf[at + 3]]);
            if masked {
                at += 4;
            }
            let total = at + len as usize;
            while !have(&self.buf, total) {
                if cancel.is_cancelled() || Instant::now() > deadline {
                    return Ok(None);
                }
                self.fill()?;
            }
            let mut payload: Vec<u8> = self.buf[at..total].to_vec();
            self.buf.drain(..total);
            if let Some(m) = mask {
                payload.iter_mut().enumerate().for_each(|(i, b)| *b ^= m[i % 4]);
            }
            match b0 & 0x0F {
                // A pong that cannot be written means the peer is closing.
                // What it already sent is still read; the next read reports
                // the close.
                0x9 => {
                    let _ = self.frame(0xA, &payload);
                }
                0xA => {}
                0x8 => return Err("the Codex app-server closed the connection".into()),
                0x0..=0x2 => {
                    message.extend_from_slice(&payload);
                    if b0 & 0x80 != 0 {
                        return Ok(Some(message));
                    }
                }
                _ => return Err("an unknown WebSocket frame from the Codex app-server".into()),
            }
        }
    }
}

// ---- JSON-RPC, read-only ------------------------------------------------------

struct Conn {
    ws: Ws,
    next: u64,
    /// The watched thread: its status notifications are kept.
    thread: String,
    /// Its `thread/status/changed` statuses, in the order they arrived,
    /// not yet taken.
    events: Vec<Value>,
    /// The thread's counter lock, when this connection is the one that
    /// counts its requests (see `record_requests`).
    counter: Option<std::fs::File>,
    /// When this connection, holding the counter, last counted (Unix ms):
    /// a request it counts next opened after that. `None` before its first
    /// count, when a request still open may have opened at any time.
    counted_at: Option<u64>,
    /// The session whose hook log holds the thread's requests (`sessionId`
    /// from `thread/read`; the thread id for a thread of its own).
    session: String,
    /// Whether `hooks/list` lists a trusted, enabled `t3-watch hook codex`
    /// for this state directory, and when it was asked.
    hooks: Option<(bool, Instant)>,
}

fn transient(message: String) -> CallError {
    CallError::Transient { message, status: None }
}

impl Conn {
    fn open(socket: &Path, thread: &str, cancel: &Cancel) -> Result<Conn, CallError> {
        let ws = Ws::connect(socket).map_err(transient)?;
        let mut c = Conn {
            ws,
            next: 0,
            thread: thread.to_string(),
            events: Vec::new(),
            counter: None,
            counted_at: None,
            session: thread.to_string(),
            hooks: None,
        };
        c.call(
            "initialize",
            json!({ "clientInfo": { "name": "t3-watch", "title": "t3-watch (read-only watcher)",
                                     "version": crate::version::VERSION } }),
            cancel,
        )?;
        c.ws.send(&json!({ "jsonrpc": "2.0", "method": "initialized" }))
            .map_err(transient)?;
        Ok(c)
    }

    /// Whether the user installed and trusted `t3-watch hook codex` for this
    /// state directory, as `hooks/list` (read-only) lists the hooks for the
    /// thread's working directory. An app-server without `hooks/list`
    /// answers an error: no hook.
    fn hooks_active(&mut self, cwd: Option<&str>, cancel: &Cancel) -> Result<bool, CallError> {
        if let Some((active, at)) = self.hooks {
            if at.elapsed() < HOOKS_RECHECK {
                return Ok(active);
            }
        }
        let cwds: Vec<&str> = cwd.into_iter().collect();
        let active = match self.call("hooks/list", json!({ "cwds": cwds }), cancel) {
            Ok(v) => {
                let home = config::state_dir();
                let real = config::absolute(&home).unwrap_or_else(|_| home.clone());
                our_hook_listed(&v, &[home.display().to_string(), real.display().to_string()])
            }
            Err(CallError::Fatal(_) | CallError::NotFound(_)) => false,
            Err(e) => return Err(e),
        };
        self.hooks = Some((active, Instant::now()));
        Ok(active)
    }

    /// Keeps a status notification of the watched thread. Any other
    /// notification is dropped, and so is any server request: t3-watch never
    /// answers one, so it can never approve or decline anything.
    fn take_notification(&mut self, m: &Value) {
        if m["method"] == "thread/status/changed" && m["params"]["threadId"].as_str() == Some(self.thread.as_str()) {
            self.events.push(m["params"]["status"].clone());
            if self.events.len() > 10_000 {
                self.events.drain(..5_000);
            }
        }
    }

    /// Reads what has arrived, waiting at most `wait`, for a connection no
    /// wait is using. `Err` when it is closed.
    fn drain(&mut self, wait: Duration) -> Result<(), String> {
        let never = Cancel::new();
        self.ws.set_read_timeout(Duration::from_millis(20));
        let deadline = Instant::now() + wait;
        let result = loop {
            match self.ws.recv(deadline, &never) {
                Ok(Some(raw)) => {
                    if let Ok(m) = serde_json::from_slice::<Value>(&raw) {
                        self.take_notification(&m);
                    }
                }
                Ok(None) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        self.ws.set_read_timeout(Duration::from_millis(250));
        result
    }

    /// One request, answered before `CALL_TIMEOUT`. What arrives before the
    /// answer goes to `take_notification`.
    fn call(&mut self, method: &str, params: Value, cancel: &Cancel) -> Result<Value, CallError> {
        self.next += 1;
        let id = self.next;
        self.ws
            .send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .map_err(transient)?;
        let deadline = Instant::now() + CALL_TIMEOUT;
        loop {
            let Some(raw) = self.ws.recv(deadline, cancel).map_err(transient)? else {
                if cancel.is_cancelled() {
                    return Err(CallError::Cancelled);
                }
                return Err(transient(format!("the Codex app-server did not answer {method}")));
            };
            let Ok(m) = serde_json::from_slice::<Value>(&raw) else {
                continue;
            };
            if m.get("method").is_some() {
                self.take_notification(&m);
                continue;
            }
            if m.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(e) = m.get("error") {
                let text = e.get("message").and_then(Value::as_str).unwrap_or("error");
                let message = util::sanitize_text(text, 300);
                let lower = message.to_ascii_lowercase();
                return Err(
                    if lower.contains("not found") || lower.contains("no rollout") || lower.contains("unknown thread") {
                        CallError::NotFound(format!("Codex: {message}"))
                    } else {
                        CallError::Fatal(format!("Codex {method}: {message}"))
                    },
                );
            }
            return Ok(m.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

/// One watched Codex thread: a connection kept between polls, and the turn
/// count, which gives each turn an ordinal.
pub struct Watcher {
    socket: PathBuf,
    thread: String,
    conn: Mutex<Option<Conn>>,
    /// The latest turn id seen and how many turns there were then.
    counted: Mutex<Option<(String, u64)>>,
    /// The thread was active when last read: its connection is kept for a
    /// while after this watch ends (see `park`).
    active: AtomicBool,
    /// Since when `thread/turns/list` has answered "list_turns is not
    /// supported yet".
    unsupported_since: Mutex<Option<Instant>>,
}

impl Watcher {
    pub fn new(socket: PathBuf, thread: String) -> Self {
        Watcher {
            socket,
            thread,
            conn: Mutex::new(None),
            counted: Mutex::new(None),
            active: AtomicBool::new(false),
            unsupported_since: Mutex::new(None),
        }
    }

    /// The thread as the app-server reports it now.
    pub fn observe(&self, cancel: &Cancel) -> Result<ThreadState, CallError> {
        let observed_at = Instant::now();
        let mut guard = util::lock(&self.conn);
        if guard.is_none() {
            // A connection kept since an earlier wait has every status
            // notification since then; a new one starts from now.
            *guard = Some(match unpark(&self.socket, &self.thread) {
                Some(conn) => conn,
                None => Conn::open(&self.socket, &self.thread, cancel)?,
            });
        }
        let conn = guard.as_mut().expect("connected");
        let result = self.read(conn, cancel, observed_at);
        if matches!(result, Err(CallError::Transient { .. }) | Err(CallError::Cancelled)) {
            // Reconnect on the next poll; a fresh read has no gap to fill.
            *guard = None;
        }
        result
    }

    fn read(&self, conn: &mut Conn, cancel: &Cancel, observed_at: Instant) -> Result<ThreadState, CallError> {
        let thread = conn.call("thread/read", json!({ "threadId": self.thread }), cancel)?;
        let status = thread["thread"]["status"].clone();
        if let Some(s) = thread["thread"]["sessionId"].as_str().filter(|s| valid_thread_id(s)) {
            conn.session = s.to_string();
        }
        // Notifications that arrived before this read's answer, in order;
        // the read itself is the latest.
        let mut seen = std::mem::take(&mut conn.events);
        seen.push(status.clone());
        let hooks = conn.hooks_active(thread["thread"]["cwd"].as_str(), cancel)?;
        if conn.counter.is_none() {
            if let Some(lock) = take_counter(&self.thread) {
                conn.counter = Some(lock);
                conn.counted_at = None;
            }
        }
        self.active.store(status["type"] == "active", Ordering::Relaxed);
        let page = match conn.call(
            "thread/turns/list",
            json!({ "threadId": self.thread, "limit": TURN_PAGE, "sortDirection": "desc", "itemsView": "notLoaded" }),
            cancel,
        ) {
            // A thread with no user message yet has no turns to list:
            // codex-cli 0.166.0 answers "not materialized yet".
            Err(CallError::Fatal(m)) if m.contains("not materialized") => json!({ "data": [] }),
            // While the first message materializes the thread, 0.166.0 can
            // answer "list_turns is not supported yet" (1 run in 6): retried
            // for `UNSUPPORTED_RETRY`, with the reason in
            // `upstream.lastError`, then a failure. Never settled on.
            Err(CallError::Fatal(m)) if m.contains("list_turns is not supported yet") => {
                let since = *util::lock(&self.unsupported_since).get_or_insert_with(Instant::now);
                if since.elapsed() < UNSUPPORTED_RETRY {
                    return Err(transient(m));
                }
                return Err(CallError::Fatal(format!("{m} (for {} s)", since.elapsed().as_secs())));
            }
            page => {
                let page = page?;
                *util::lock(&self.unsupported_since) = None;
                page
            }
        };
        let turns: Vec<Value> = page["data"].as_array().cloned().unwrap_or_default();
        let latest = turns.first();
        let ordinal = match latest.and_then(|t| t["id"].as_str()) {
            None => 0,
            // The newest page holds every turn: that is the count.
            Some(id) if page["nextCursor"].is_null() => {
                *util::lock(&self.counted) = Some((id.to_string(), turns.len() as u64));
                turns.len() as u64
            }
            Some(id) => self.ordinal_of(conn, cancel, id, &turns)?,
        };
        let turn = latest.and_then(|t| t["id"].as_str()).filter(|t| valid_thread_id(t));
        let turn = turn.unwrap_or("none");
        let mut requests = if conn.counter.is_some() {
            let now = util::now_ms();
            let r = record_requests(&self.thread, &conn.session, Some(turn), &seen, conn.counted_at, hooks);
            conn.counted_at = Some(now);
            r
        } else {
            read_requests(&self.thread, turn, &status)
        };
        requests.recovery = Some(if hooks { "hook" } else { "none" });
        Ok(state_of(&self.thread, &status, latest, ordinal, requests, observed_at))
    }

    /// The latest turn's ordinal when there are more turns than one page:
    /// counted once by paging, then kept up by finding the previously latest
    /// turn in the newest page.
    fn ordinal_of(&self, conn: &mut Conn, cancel: &Cancel, latest: &str, newest: &[Value]) -> Result<u64, CallError> {
        let mut counted = util::lock(&self.counted);
        if let Some((id, n)) = counted.as_ref() {
            if id == latest {
                return Ok(*n);
            }
            if let Some(k) = newest.iter().position(|t| t["id"].as_str() == Some(id)) {
                let n = n + k as u64;
                *counted = Some((latest.to_string(), n));
                return Ok(n);
            }
        }
        let mut n = 0u64;
        let mut cursor = Value::Null;
        loop {
            let page = conn.call(
                "thread/turns/list",
                json!({ "threadId": self.thread, "limit": 100, "sortDirection": "asc",
                        "itemsView": "notLoaded", "cursor": cursor }),
                cancel,
            )?;
            n += page["data"].as_array().map_or(0, Vec::len) as u64;
            cursor = page["nextCursor"].clone();
            if cursor.is_null() || n > 1_000_000 {
                break;
            }
        }
        *counted = Some((latest.to_string(), n));
        Ok(n)
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let conn = self.conn.get_mut().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(conn) = conn.filter(|_| self.active.load(Ordering::Relaxed)) {
            park(self.socket.clone(), conn);
        }
    }
}

// ---- connections kept between waits -----------------------------------------

/// A connection whose watch ended while its thread's turn was running. A
/// bot that relayed a request waits again moments later; until then this
/// keeps reading the thread's status notifications into the request count,
/// so a request that opens in between is not mistaken for the one already
/// relayed. Dropped when the turn ends (the status is no longer `active`),
/// the connection closes, or after `LINGER`.
struct Parked {
    socket: PathBuf,
    conn: Conn,
    since: Instant,
}

/// Kept connections. A drain round reads them in place, under this lock,
/// so a wait that starts meanwhile waits for the round and then finds its
/// thread's connection here: none is ever out of `unpark`'s sight.
static PARKED: Mutex<Vec<Parked>> = Mutex::new(Vec::new());
static DRAINING: AtomicBool = AtomicBool::new(false);
/// How many there are, readable without waiting for a drain round.
static KEPT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Waits wanting `PARKED`: the drain lets each through before its next
/// round, so none waits longer than one round.
static WAITERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn lock_parked() -> std::sync::MutexGuard<'static, Vec<Parked>> {
    WAITERS.fetch_add(1, Ordering::SeqCst);
    let g = util::lock(&PARKED);
    WAITERS.fetch_sub(1, Ordering::SeqCst);
    g
}

/// Keeps `conn`. With one already kept for the thread, the one holding the
/// thread's counter stays and the other is closed (the newer one when
/// neither holds it): the count must never lose its counting connection.
fn park(socket: PathBuf, conn: Conn) {
    let mut parked = lock_parked();
    match parked
        .iter()
        .position(|p| p.socket == socket && p.conn.thread == conn.thread)
    {
        Some(at) if parked[at].conn.counter.is_some() && conn.counter.is_none() => drop(conn),
        Some(at) => {
            parked[at] = Parked {
                socket,
                conn,
                since: Instant::now(),
            }
        }
        None => parked.push(Parked {
            socket,
            conn,
            since: Instant::now(),
        }),
    }
    KEPT.store(parked.len(), Ordering::SeqCst);
    drop(parked);
    if !DRAINING.swap(true, Ordering::SeqCst) && util::spawn_thread("t3-watch-codex", 128 * 1024, drain_parked).is_err()
    {
        DRAINING.store(false, Ordering::SeqCst);
    }
}

fn unpark(socket: &Path, thread: &str) -> Option<Conn> {
    let mut parked = lock_parked();
    let at = parked
        .iter()
        .position(|p| p.socket == socket && p.conn.thread == thread)?;
    let conn = parked.remove(at).conn;
    KEPT.store(parked.len(), Ordering::SeqCst);
    Some(conn)
}

/// Connections kept between waits; the daemon stays up while there are any.
pub fn lingering() -> usize {
    KEPT.load(Ordering::SeqCst)
}

/// Test hook: with `T3_WATCH_TEST_CODEX_DRAIN_MS` set, each drain round
/// reads each kept connection that long and rounds follow each other
/// without a pause, so a test can start a wait in the middle of one.
fn drain_wait() -> Option<Duration> {
    static WAIT: std::sync::OnceLock<Option<Duration>> = std::sync::OnceLock::new();
    *WAIT.get_or_init(|| {
        std::env::var("T3_WATCH_TEST_CODEX_DRAIN_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(Duration::from_millis)
    })
}

fn drain_parked() {
    loop {
        let wait = drain_wait();
        let mut parked = util::lock(&PARKED);
        parked.retain_mut(|p| {
            let open = p.conn.drain(wait.unwrap_or(Duration::from_millis(50))).is_ok();
            let seen = std::mem::take(&mut p.conn.events);
            if seen.is_empty() {
                return open && p.since.elapsed() < LINGER;
            }
            // Whoever counted is gone: this one counts from now on.
            if p.conn.counter.is_none() {
                if let Some(lock) = take_counter(&p.conn.thread) {
                    p.conn.counter = Some(lock);
                    p.conn.counted_at = None;
                }
            }
            if p.conn.counter.is_some() {
                let now = util::now_ms();
                let hooks = p.conn.hooks.is_some_and(|(active, _)| active);
                record_requests(&p.conn.thread, &p.conn.session, None, &seen, p.conn.counted_at, hooks);
                p.conn.counted_at = Some(now);
            }
            // The newest status decides: a batch can end one turn and start
            // the next.
            let turn_over = seen.last().is_some_and(|s| s["type"] != "active");
            open && !turn_over && p.since.elapsed() < LINGER
        });
        KEPT.store(parked.len(), Ordering::SeqCst);
        if parked.is_empty() {
            DRAINING.store(false, Ordering::SeqCst);
            return;
        }
        drop(parked);
        let deadline = Instant::now() + Duration::from_secs(1);
        while WAITERS.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(if wait.is_some() {
            Duration::ZERO
        } else {
            Duration::from_millis(200)
        });
    }
}

fn flags_of(status: &Value) -> Vec<&str> {
    status["activeFlags"]
        .as_array()
        .map(|f| f.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn codex_dir() -> PathBuf {
    config::state_dir().join("native").join("codex")
}

/// The thread's counter lock, if no other connection holds it. One
/// connection at a time counts a thread's requests, so that two of them
/// never count the same notification twice; the others read the count.
fn take_counter(thread: &str) -> Option<std::fs::File> {
    let dir = codex_dir();
    util::ensure_private_dir(&dir).ok()?;
    let f = util::open_lock_file(&dir.join(format!("{thread}.counter"))).ok()?;
    util::try_flock(&f).ok()?.then_some(f)
}

/// What a read tells about the latest turn's requests.
#[derive(Default)]
struct Requests {
    /// The open approval and user-input request's number in the turn,
    /// counted from 1 (0: none open).
    approval: u64,
    input: u64,
    /// The turn's approval requests that opened and closed while no
    /// connection counted, found in the hook log.
    recovered: Vec<RecoveredRequest>,
    /// `hook` when the hook log is installed for this state directory.
    recovery: Option<&'static str>,
}

fn stored_requests(thread: &str) -> Option<Value> {
    util::open_private_read(&codex_dir().join(format!("{thread}.json")))
        .ok()
        .and_then(|f| serde_json::from_reader::<_, Value>(f.take(1 << 20)).ok())
}

fn recovered_of(kept: &Value, turn: &str) -> Vec<RecoveredRequest> {
    kept["approval"]["recovered"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| RecoveredRequest {
            id: format!("codex-{turn}-approval-{}", r["k"].as_u64().unwrap_or(0)),
            kind: "approval",
            tool: r["tool"].as_str().map(str::to_string),
            opened_ms: r["atMs"].as_u64().unwrap_or(0),
            found_ms: r["foundMs"].as_u64().unwrap_or(0),
        })
        .collect()
}

/// The request numbers as the counting connection last stored them, for a
/// connection that does not count: a flag that is on but not yet counted
/// as open is the next request.
fn read_requests(thread: &str, turn: &str, status: &Value) -> Requests {
    let stored = stored_requests(thread).filter(|v| v["turn"] == turn || v["turn"] == "");
    let flags = flags_of(status);
    let number = |kind: &str, flag: &str| {
        if !flags.contains(&flag) {
            return 0;
        }
        let k = stored.as_ref().map_or(&Value::Null, |v| &v[kind]);
        let n = k["n"].as_u64().unwrap_or(0);
        if k["open"] == true {
            k["k"].as_u64().unwrap_or(n).max(1)
        } else {
            n + 1
        }
    };
    Requests {
        approval: number("approval", "waitingOnApproval"),
        input: number("input", "waitingOnUserInput"),
        recovered: stored.as_ref().map(|v| recovered_of(v, turn)).unwrap_or_default(),
        recovery: None,
    }
}

/// One line of the hook log for the turn being counted.
struct HookRecord {
    at: u64,
    tool: Option<String>,
}

fn hook_log(home: &Path, session: &str) -> PathBuf {
    home.join("native")
        .join("codex")
        .join(format!("{session}.requests.jsonl"))
}

/// The turn's `PermissionRequest` records in the session's hook log, read
/// from `offset`, and where the next read starts: at the turn's first
/// record, or past every complete line when the turn has none yet (the
/// log is append-only, so the turn's records all come later).
fn hook_records(session: &str, turn: &str, offset: u64) -> (Vec<HookRecord>, u64) {
    let Ok(mut f) = util::open_private_read(&hook_log(&config::state_dir(), session)) else {
        return (Vec::new(), 0);
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    // A log that is shorter than the offset was replaced: read it whole.
    let offset = if offset > len { 0 } else { offset };
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(offset)).is_err() || f.take(MAX_HOOK_READ).read_to_end(&mut buf).is_err() {
        return (Vec::new(), offset);
    }
    let mut records = Vec::new();
    let mut start = None;
    let mut pos = 0usize;
    // Only complete lines: the hook may be writing the last one.
    while let Some(nl) = buf[pos..].iter().position(|&b| b == b'\n') {
        if let Ok(v) = serde_json::from_slice::<Value>(&buf[pos..pos + nl]) {
            if v["event"] == "PermissionRequest" && v["turn"] == turn {
                start.get_or_insert(offset + pos as u64);
                records.push(HookRecord {
                    at: v["atMs"].as_u64().unwrap_or(0),
                    tool: v["tool"].as_str().filter(|t| t.len() <= 128).map(str::to_string),
                });
            }
        }
        pos += nl + 1;
    }
    (records, start.unwrap_or(offset + pos as u64))
}

/// The number of the open approval and user-input request in the latest
/// turn, counted from 1 (0: none open), after the statuses in `seen` (in
/// the order they were observed; the last is the newest), and the turn's
/// recovered requests.
///
/// The app-server tells a reader only that the thread waits
/// (`waitingOnApproval`, `waitingOnUserInput`), not on which request: with
/// codex-cli 0.166.0 nothing a reader can read changes between two requests
/// of one turn, and a pending request's item is not persisted. Every
/// initialized connection does receive each `thread/status/changed`, so
/// every time a flag comes on while the counting connection is open,
/// however briefly it was off, one more request is counted. Only the
/// connection holding the thread's counter lock counts (`take_counter`);
/// the count is kept in `<state>/native/codex/<thread>.json`, shared by
/// every process watching the thread and by connections kept between
/// waits.
///
/// Approval requests are then reconciled with the hook log, which has one
/// record per request whether or not anything counted it. Each record is
/// matched, newest first, to a request counted live: one counted after the
/// previous count of the same connection (`counted_since`; any time for a
/// connection that has just taken the counter) and at most `HOOK_LAG_MS`
/// before the record. A record no count matches opened while nothing
/// counted: if it is the newest and a request is open, it is that request
/// (it replaced the one counted before), and otherwise it opened and closed
/// unseen and is reported as recovered, under the next number. A
/// connection that takes the counter over counts a flag that is still on
/// once more, unless the hook is installed and the open request's record is
/// in the log. Without the hook, a request that opens and closes while
/// nothing counts is not seen (docs/native-sessions.md, "Limits").
///
/// `turn` is the latest turn's id, from a read; `None` (a kept connection,
/// which reads nothing) continues the stored turn, or, when `seen` ends one
/// turn and goes on, starts a turn whose id is not known yet (`""`), which
/// the next read names.
fn record_requests(
    thread: &str,
    session: &str,
    turn: Option<&str>,
    seen: &[Value],
    counted_since: Option<u64>,
    hooks: bool,
) -> Requests {
    let dir = codex_dir();
    let path = dir.join(format!("{thread}.json"));
    let _lock = util::ensure_private_dir(&dir)
        .and_then(|_| util::open_lock_file(&dir.join(format!("{thread}.lock"))))
        .ok()
        .filter(|f| util::flock_within(f, Duration::from_secs(2)).unwrap_or(false));
    let stored = stored_requests(thread);
    let offset = stored.as_ref().and_then(|v| v["hooks"]["offset"].as_u64()).unwrap_or(0);
    // The last status that ended a turn; what follows it is the next turn.
    let ended = seen.iter().rposition(|s| s["type"] != "active");
    let (same_turn, from) = match (turn, &stored) {
        (None, _) => match ended {
            Some(i) if i + 1 < seen.len() => (false, i + 1),
            _ => (true, 0),
        },
        (Some(t), Some(v)) if v["turn"] == t || v["turn"] == "" => (true, 0),
        (Some(_), _) => (false, ended.map_or(0, |i| i + 1)),
    };
    let mut kept = match stored {
        Some(v) if same_turn => v,
        // A new turn's records come after the last one's first record.
        _ => json!({ "turn": turn.unwrap_or(""), "hooks": { "offset": offset, "h": 0 } }),
    };
    if let Some(t) = turn {
        kept["turn"] = json!(t);
    }
    let before = kept.clone();
    let now = util::now_ms();
    let seen = &seen[from.min(seen.len())..];
    let mut out = Requests::default();

    // A connection that has just taken the counter (`counted_since` is
    // `None`) cannot tell whether a flag that is still on is the request
    // counted before or one that replaced it while nothing counted. It
    // counts it again: an extra wake for the same request, never a missed
    // one. For an approval whose hook record is in, the hook log tells
    // instead (below).
    let fresh = counted_since.is_none();
    let mut n = kept["input"]["n"].as_u64().unwrap_or(0);
    let mut open = kept["input"]["open"] == true && !fresh;
    for status in seen {
        let on = flags_of(status).contains(&"waitingOnUserInput");
        if on && !open {
            n += 1;
        }
        open = on;
    }
    kept["input"] = json!({ "n": n, "open": open });
    out.input = if open { n } else { 0 };

    let a = &kept["approval"];
    let mut n = a["n"].as_u64().unwrap_or(0);
    // The open request's number: the newest, unless one was recovered later.
    let mut k = a["k"].as_u64().unwrap_or(n);
    // Its hook record is in the log, so the hook works for it.
    let mut recorded = a["recorded"] == true;
    let mut open = a["open"] == true && !(fresh && !(hooks && recorded));
    // Requests counted live whose hook record has not been matched yet:
    // [counted at, opened after (Unix ms), number].
    let mut unmatched: Vec<(u64, u64, u64)> = a["unmatched"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| Some((e[0].as_u64()?, e[1].as_u64()?, e[2].as_u64()?)))
        .collect();
    let mut recovered: Vec<Value> = a["recovered"].as_array().cloned().unwrap_or_default();
    for status in seen {
        let on = flags_of(status).contains(&"waitingOnApproval");
        if on && !open {
            n += 1;
            k = n;
            recorded = false;
            unmatched.push((now, counted_since.unwrap_or(0), n));
        }
        open = on;
    }
    let t = kept["turn"].as_str().unwrap_or("").to_string();
    if !t.is_empty() {
        let offset = kept["hooks"]["offset"].as_u64().unwrap_or(0);
        let (records, start) = hook_records(session, &t, offset);
        let h = kept["hooks"]["h"].as_u64().unwrap_or(0) as usize;
        let mut unseen = Vec::new();
        for (i, r) in records.iter().enumerate().skip(h).rev() {
            // A clock step can put a record a moment before its count.
            let matches =
                |&(at, after, _): &(u64, u64, u64)| after.saturating_sub(1_000) <= r.at && r.at <= at + HOOK_LAG_MS;
            match unmatched.iter().rposition(matches) {
                Some(j) => {
                    recorded |= unmatched.remove(j).2 == k;
                }
                None => unseen.push(i),
            }
        }
        unseen.reverse();
        let replaced = open && unseen.last().is_some_and(|&i| i + 1 == records.len());
        let closed = unseen.len() - usize::from(replaced);
        for &i in &unseen[..closed] {
            n += 1;
            recovered.push(json!({ "k": n, "atMs": records[i].at, "tool": records[i].tool, "foundMs": now }));
        }
        if replaced {
            n += 1;
            k = n;
            recorded = true;
        }
        if recovered.len() > MAX_RECOVERED {
            recovered.drain(..recovered.len() - MAX_RECOVERED);
        }
        kept["hooks"] = json!({ "offset": start, "h": records.len().max(h) });
    }
    // A count no record matched in time had no hook (another kind of
    // approval, or no hook installed).
    unmatched.retain(|&(at, _, _)| now.saturating_sub(at) <= HOOK_LAG_MS);
    let unmatched: Vec<Value> = unmatched.iter().map(|&(a, b, c)| json!([a, b, c])).collect();
    kept["approval"] = json!({
        "n": n, "open": open, "k": k, "recorded": recorded, "unmatched": unmatched, "recovered": recovered,
    });
    out.approval = if open { k } else { 0 };
    out.recovered = if t.is_empty() {
        Vec::new()
    } else {
        recovered_of(&kept, &t)
    };
    if kept != before {
        // Best effort: without the file, a later process counts afresh.
        let _ = util::write_private(&path, kept.to_string().as_bytes());
    }
    out
}

/// `t3-watch hook codex`: records one `PermissionRequest` from `input` (the
/// hook's stdin) in `home`'s hook log: the time, the turn id and the tool
/// name, never the command, its input or a path. Any other event is
/// ignored. An error is for the caller to log; the hook still succeeds.
pub fn record_codex_hook(input: impl Read, home: &Path) -> Result<(), String> {
    let mut raw = Vec::new();
    input
        .take(MAX_HOOK_INPUT)
        .read_to_end(&mut raw)
        .map_err(|e| format!("cannot read the hook input: {e}"))?;
    let v: Value = serde_json::from_slice(&raw).map_err(|_| "the hook input is not JSON".to_string())?;
    if v["hook_event_name"] != "PermissionRequest" {
        return Ok(());
    }
    let id = |key: &str| v[key].as_str().filter(|s| valid_thread_id(s));
    let session = id("session_id").ok_or("the hook input has no valid session_id")?;
    let turn = id("turn_id").ok_or("the hook input has no valid turn_id")?;
    let mut line = json!({
        "atMs": util::now_ms(),
        "event": "PermissionRequest",
        "turn": turn,
        "tool": crate::native::tool_name(&v),
    })
    .to_string();
    line.push('\n');
    let path = hook_log(home, session);
    let mut f = util::open_log(&path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    f.write_all(line.as_bytes())
        .and_then(|_| f.sync_data())
        .map_err(|e| format!("cannot record the hook event: {e}"))
}

/// The hook block for the user to add to `~/.codex/config.toml`, running
/// `bin` as an async `PermissionRequest` hook that records into `home`.
pub fn codex_hook_settings(bin: &str, home: &Path) -> String {
    let command = format!(
        "{} hook codex --home {}",
        crate::native::shell_quote(bin),
        crate::native::shell_quote(&home.display().to_string())
    );
    format!(
        "# t3-watch: records each Codex approval request (its time, turn id and\n\
         # tool name; never the command or its input) so that a wait reports a\n\
         # request that opened and closed while nothing was watching. It runs\n\
         # async: it cannot approve, deny or change anything. Codex runs it once\n\
         # you review and trust it (/hooks in the CLI, Settings > Hooks in the app).\n\
         [[hooks.PermissionRequest]]\n\
         [[hooks.PermissionRequest.hooks]]\n\
         type = \"command\"\n\
         command = {}\n\
         timeout = 10\n\
         async = true\n",
        serde_json::to_string(&command).unwrap_or_default()
    )
}

/// `hooks/list` lists `t3-watch hook codex` recording into this state
/// directory (as given, or resolved), enabled and trusted (or managed), for
/// `PermissionRequest`.
fn our_hook_listed(v: &Value, homes: &[String]) -> bool {
    v["data"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|e| e["hooks"].as_array().into_iter().flatten())
        .any(|h| {
            h["eventName"] == "permissionRequest"
                && h["enabled"] == true
                && matches!(h["trustStatus"].as_str(), Some("trusted" | "managed"))
                && h["command"]
                    .as_str()
                    .is_some_and(|c| c.contains("hook codex") && homes.iter().any(|h| c.contains(h.as_str())))
        })
}

/// The thread state the engine reads, from the thread's runtime status and
/// its latest turn: a status, the latest run (one per turn) and the open
/// requests, each with its own id.
fn state_of(
    thread: &str,
    status: &Value,
    latest: Option<&Value>,
    ordinal: u64,
    requests: Requests,
    observed_at: Instant,
) -> ThreadState {
    let Requests {
        approval,
        input,
        recovered,
        recovery,
    } = requests;
    let tid = codex_thread_id(thread);
    let kind = status["type"].as_str().unwrap_or("");
    let turn_status = latest.and_then(|t| t["status"].as_str());
    let state = match (kind, turn_status) {
        ("active", _) => Some("running"),
        ("systemError", _) => Some("error"),
        (_, Some("completed")) => Some("completed"),
        (_, Some("interrupted")) => Some("interrupted"),
        (_, Some("failed")) => Some("failed"),
        // A turn still in progress on a server that does not run it.
        (_, Some("inProgress")) => Some("running"),
        _ => None,
    };
    let turn_id = latest.and_then(|t| t["id"].as_str()).unwrap_or("none");
    let mut pending = Vec::new();
    if approval > 0 {
        pending.push(format!("codex-{turn_id}-approval-{approval}"));
    }
    if input > 0 {
        pending.push(format!("codex-{turn_id}-input-{input}"));
    }
    let mut t = json!({
        "threadId": tid,
        "status": state,
        "pendingRequestCount": pending.len(),
        "title": if ordinal == 0 { "Codex thread (no turn yet)" } else { "Codex thread" },
    });
    if ordinal > 0 {
        let run = format!("run:thread:{tid}:ordinal:{ordinal}");
        t["latestRunId"] = json!(run);
        t["runCount"] = json!(ordinal);
        if state == Some("running") {
            t["activeRunId"] = json!(run);
        }
    }
    let mut s = ThreadState::from_thread_json(&json!({ "thread": t }), &tid, observed_at);
    s.pending_ids = pending;
    s.recovered = recovered;
    s.request_recovery = recovery;
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (input, out) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), out);
        }
    }

    #[test]
    fn states_come_from_the_runtime_status_and_the_latest_turn() {
        let t = |status: Value, turn: Option<Value>, n| {
            let flags = flags_of(&status);
            let numbers = Requests {
                approval: u64::from(flags.contains(&"waitingOnApproval")),
                input: u64::from(flags.contains(&"waitingOnUserInput")),
                ..Requests::default()
            };
            state_of("01a1", &status, turn.as_ref(), n, numbers, Instant::now())
        };
        let busy = t(
            json!({"type": "active", "activeFlags": []}),
            Some(json!({"id": "x", "status": "inProgress"})),
            3,
        );
        assert_eq!(busy.status.as_deref(), Some("running"));
        assert_eq!(busy.ordinal(), Some(3));
        assert!(!busy.needs_input());
        let asks = t(
            json!({"type": "active", "activeFlags": ["waitingOnApproval"]}),
            Some(json!({"id": "x", "status": "inProgress"})),
            3,
        );
        assert!(asks.needs_input());
        let done = t(
            json!({"type": "idle"}),
            Some(json!({"id": "x", "status": "completed"})),
            4,
        );
        assert_eq!(done.status.as_deref(), Some("completed"));
        assert!(done.active_run_id.is_none());
        let failed = t(
            json!({"type": "notLoaded"}),
            Some(json!({"id": "x", "status": "failed"})),
            4,
        );
        assert_eq!(failed.status.as_deref(), Some("failed"));
        let fresh = t(json!({"type": "idle"}), None, 0);
        assert_eq!(fresh.status, None);
        assert!(fresh.latest_run_id.is_none());
    }
}
