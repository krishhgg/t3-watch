//! Native Codex threads (experimental; see `docs/native-sessions.md`),
//! watched read-only through the Codex app-server's documented JSON-RPC
//! protocol on its WebSocket-over-Unix-socket transport: the socket of
//! `codex app-server --listen unix://…`, such as the shared daemon's
//! control socket.
//!
//! Only `initialize`, `thread/read` and `thread/turns/list` are sent, the
//! latter with `itemsView: "notLoaded"` so that no message text is read.
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

use crate::cancel::Cancel;
use crate::config;
use crate::mcp::CallError;
use crate::model::ThreadState;
use crate::util;
use serde_json::{json, Value};
use std::io::{self, Read, Write};
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
                0x9 => self.frame(0xA, &payload)?,
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
}

impl Watcher {
    pub fn new(socket: PathBuf, thread: String) -> Self {
        Watcher {
            socket,
            thread,
            conn: Mutex::new(None),
            counted: Mutex::new(None),
            active: AtomicBool::new(false),
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
        // Notifications that arrived before this read's answer, in order;
        // the read itself is the latest.
        let mut seen = std::mem::take(&mut conn.events);
        seen.push(status.clone());
        if conn.counter.is_none() {
            if let Some(lock) = take_counter(&self.thread) {
                // Taking over from another connection, which may have
                // counted what this one has seen so far: only the read.
                conn.counter = Some(lock);
                seen = vec![status.clone()];
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
            // answer "list_turns is not supported yet" (1 run in 6): retried,
            // with the reason in `upstream.lastError`, never settled on.
            Err(CallError::Fatal(m)) if m.contains("not supported yet") => return Err(transient(m)),
            page => page?,
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
        let numbers = if conn.counter.is_some() {
            record_requests(&self.thread, Some(turn), &seen)
        } else {
            read_requests(&self.thread, turn, &status)
        };
        Ok(state_of(&self.thread, &status, latest, ordinal, numbers, observed_at))
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

static PARKED: Mutex<Vec<Parked>> = Mutex::new(Vec::new());
static DRAINING: AtomicBool = AtomicBool::new(false);

fn park(socket: PathBuf, conn: Conn) {
    let mut parked = util::lock(&PARKED);
    parked.retain(|p| !(p.socket == socket && p.conn.thread == conn.thread));
    parked.push(Parked {
        socket,
        conn,
        since: Instant::now(),
    });
    drop(parked);
    if !DRAINING.swap(true, Ordering::SeqCst) && util::spawn_thread("t3-watch-codex", 128 * 1024, drain_parked).is_err()
    {
        DRAINING.store(false, Ordering::SeqCst);
    }
}

fn unpark(socket: &Path, thread: &str) -> Option<Conn> {
    let mut parked = util::lock(&PARKED);
    let at = parked
        .iter()
        .position(|p| p.socket == socket && p.conn.thread == thread)?;
    Some(parked.remove(at).conn)
}

/// Connections kept between waits; the daemon stays up while there are any.
pub fn lingering() -> usize {
    util::lock(&PARKED).len()
}

fn drain_parked() {
    loop {
        let batch: Vec<Parked> = std::mem::take(&mut *util::lock(&PARKED));
        let mut keep = Vec::new();
        for mut p in batch {
            let open = p.conn.drain(Duration::from_millis(50)).is_ok();
            let seen = std::mem::take(&mut p.conn.events);
            if !seen.is_empty() && p.conn.counter.is_some() {
                record_requests(&p.conn.thread, None, &seen);
            }
            let turn_over = seen.iter().any(|s| s["type"] != "active");
            if open && !turn_over && p.since.elapsed() < LINGER {
                keep.push(p);
            }
        }
        let mut parked = util::lock(&PARKED);
        // A watch may have taken or parked one meanwhile: those stand.
        for p in keep {
            if !parked
                .iter()
                .any(|q| q.socket == p.socket && q.conn.thread == p.conn.thread)
            {
                parked.push(p);
            }
        }
        if parked.is_empty() {
            DRAINING.store(false, Ordering::SeqCst);
            return;
        }
        drop(parked);
        std::thread::sleep(Duration::from_millis(200));
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

/// The request numbers as the counting connection last stored them, for a
/// connection that does not count: a flag that is on but not yet counted
/// as open is the next request.
fn read_requests(thread: &str, turn: &str, status: &Value) -> (u64, u64) {
    let stored = util::open_private_read(&codex_dir().join(format!("{thread}.json")))
        .ok()
        .and_then(|f| serde_json::from_reader::<_, Value>(f.take(1 << 16)).ok())
        .filter(|v| v["turn"] == turn);
    let flags = flags_of(status);
    let number = |kind: &str, flag: &str| {
        if !flags.contains(&flag) {
            return 0;
        }
        let k = stored.as_ref().map_or(&Value::Null, |v| &v[kind]);
        let n = k["n"].as_u64().unwrap_or(0);
        if k["open"] == true {
            n.max(1)
        } else {
            n + 1
        }
    };
    (
        number("approval", "waitingOnApproval"),
        number("input", "waitingOnUserInput"),
    )
}

/// The number of the open approval and user-input request in the latest
/// turn, counted from 1 (0: none open), after the statuses in `seen` (in
/// the order they were observed; the last is the newest).
///
/// The app-server tells a reader only that the thread waits
/// (`waitingOnApproval`, `waitingOnUserInput`), not on which request: with
/// codex-cli 0.166.0 nothing a reader can read changes between two requests
/// of one turn, and a pending request's item is not persisted. Every
/// initialized connection does receive each `thread/status/changed`, so
/// every time a flag comes on while some t3-watch connection is open,
/// however briefly it was off, one more request is counted. Only the
/// connection holding the thread's counter lock counts (`take_counter`);
/// the count is kept in `<state>/native/codex/<thread>.json`, shared by
/// every process watching the thread and by connections kept between
/// waits. A request that opens while no connection is open, or during the
/// handover from one counting connection to the next, shares its
/// predecessor's number (docs/native-sessions.md, "Limits").
///
/// `turn` is the latest turn's id, from a read; `None` (a kept connection,
/// which reads nothing) continues the stored turn.
fn record_requests(thread: &str, turn: Option<&str>, seen: &[Value]) -> (u64, u64) {
    let dir = codex_dir();
    let path = dir.join(format!("{thread}.json"));
    let _lock = util::ensure_private_dir(&dir)
        .and_then(|_| util::open_lock_file(&dir.join(format!("{thread}.lock"))))
        .ok()
        .filter(|f| util::flock_within(f, Duration::from_secs(2)).unwrap_or(false));
    let stored = util::open_private_read(&path)
        .ok()
        .and_then(|f| serde_json::from_reader::<_, Value>(f.take(1 << 16)).ok());
    let same_turn = match (turn, &stored) {
        (None, _) => true,
        (Some(t), Some(v)) => v["turn"] == t,
        (Some(_), None) => false,
    };
    let mut kept = match stored {
        Some(v) if same_turn => v,
        _ => json!({ "turn": turn.unwrap_or("none") }),
    };
    // A new turn: only what came after the last status that ended a turn
    // belongs to it.
    let from = if same_turn {
        0
    } else {
        seen.iter().rposition(|s| s["type"] != "active").map_or(0, |i| i + 1)
    };
    let before = kept.clone();
    let mut numbers = [0u64; 2];
    for (i, (kind, flag)) in [("approval", "waitingOnApproval"), ("input", "waitingOnUserInput")]
        .into_iter()
        .enumerate()
    {
        let mut n = kept[kind]["n"].as_u64().unwrap_or(0);
        let mut open = kept[kind]["open"] == true;
        for status in &seen[from.min(seen.len())..] {
            let on = flags_of(status).contains(&flag);
            if on && !open {
                n += 1;
            }
            open = on;
        }
        kept[kind] = json!({ "n": n, "open": open });
        numbers[i] = if open { n } else { 0 };
    }
    if kept != before {
        // Best effort: without the file, a later process counts afresh.
        let _ = util::write_private(&path, kept.to_string().as_bytes());
    }
    (numbers[0], numbers[1])
}

/// The thread state the engine reads, from the thread's runtime status and
/// its latest turn: a status, the latest run (one per turn) and the open
/// requests, each with its own id.
fn state_of(
    thread: &str,
    status: &Value,
    latest: Option<&Value>,
    ordinal: u64,
    (approval, input): (u64, u64),
    observed_at: Instant,
) -> ThreadState {
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
            let numbers = (
                u64::from(flags.contains(&"waitingOnApproval")),
                u64::from(flags.contains(&"waitingOnUserInput")),
            );
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
