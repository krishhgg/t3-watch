//! Native Codex threads (experimental): `wait codex://threads/<id>` reads
//! the thread from a Codex app-server over its WebSocket-on-Unix-socket
//! transport. The app-server here is a mock that speaks the documented
//! protocol; it records every method a client sends, so the tests can show
//! that t3-watch only reads and never answers a server request. Approval
//! requests are also recorded by `t3-watch hook codex`, run here as Codex
//! runs a `PermissionRequest` hook.

mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const T: &str = "01a1ba73-b8d4-7aa3-b523-643d92150f60";
const READS: &[&str] = &[
    "initialize",
    "initialized",
    "thread/read",
    "thread/turns/list",
    "hooks/list",
];

fn target() -> String {
    format!("codex://threads/{T}")
}

#[derive(Default)]
struct Codex {
    /// The thread's runtime status, as `thread/read` reports it.
    status: Value,
    /// Turns, oldest first: `{id, status}`.
    turns: Vec<Value>,
    /// Every method a client sent, with its params.
    calls: Vec<(String, Value)>,
    /// Any client message that answers the server's own request.
    answers: Vec<Value>,
    /// Client frames that were not masked (RFC 6455 requires masking).
    unmasked: usize,
    connections: usize,
    /// Close each connection after this many requests on it.
    drop_after: Option<usize>,
    /// Bytes of padding in each `thread/turns/list` answer.
    pad: usize,
    /// Send each answer in two frames.
    fragment: bool,
    /// `thread/read` says the thread does not exist.
    missing: bool,
    /// No user message yet: `thread/turns/list` is refused, as codex-cli
    /// 0.166.0 refuses it.
    unmaterialized: bool,
    /// `thread/turns/list` answers "not supported yet" this many times
    /// first, as codex-cli 0.166.0 can while a thread materializes.
    unsupported: usize,
    /// The refusal's message, when not codex-cli 0.166.0's.
    unsupported_message: Option<String>,
    /// Every open connection, for the status notifications the app-server
    /// sends each initialized connection.
    writers: Vec<Arc<Mutex<UnixStream>>>,
    /// What `hooks/list` lists; `None`: an app-server without it.
    hooks: Option<Value>,
}

struct MockCodex {
    sock: PathBuf,
    st: Arc<Mutex<Codex>>,
}

impl MockCodex {
    fn start(env: &Env) -> Self {
        let sock = env.home.join("cx.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let st = Arc::new(Mutex::new(Codex {
            status: json!({ "type": "idle" }),
            ..Codex::default()
        }));
        let shared = st.clone();
        thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let st = shared.clone();
                thread::spawn(move || serve(conn, st));
            }
        });
        MockCodex { sock, st }
    }

    /// The thread's state; a status change is also announced, as a real
    /// app-server does.
    fn set(&self, status: Value, turns: &[(&str, &str)]) {
        let changed = {
            let mut s = self.st.lock().unwrap();
            let changed = s.status != status;
            s.status = status.clone();
            s.turns = turns.iter().map(|(id, st)| json!({ "id": id, "status": st })).collect();
            changed
        };
        if changed {
            self.push(status);
        }
    }

    /// A `thread/status/changed` to every open connection, without changing
    /// what `thread/read` answers: a status that came and went between two
    /// reads.
    fn push(&self, status: Value) {
        let writers = self.st.lock().unwrap().writers.clone();
        let note = json!({ "method": "thread/status/changed", "params": { "threadId": T, "status": status } });
        for w in writers {
            let _ = send(&mut w.lock().unwrap(), &note, false);
        }
    }

    fn with(&self, f: impl FnOnce(&mut Codex)) {
        f(&mut self.st.lock().unwrap());
    }

    fn methods(&self) -> Vec<String> {
        self.st.lock().unwrap().calls.iter().map(|(m, _)| m.clone()).collect()
    }

    /// `hooks/list` lists `t3-watch hook codex` for this state directory,
    /// as Codex does once the user installed and trusted it.
    fn install_hook(&self, env: &Env, trust: &str) {
        let command = format!("'{BIN}' hook codex --home {}", env.home.display());
        self.with(|c| {
            c.hooks = Some(json!([{
                "key": "/u/.codex/config.toml:permission_request:0:0", "eventName": "permissionRequest",
                "handlerType": "command", "command": command, "async": true, "enabled": true,
                "trustStatus": trust, "isManaged": false, "source": "user" }]))
        });
    }
}

/// Runs `t3-watch hook codex` with `input` on stdin, as Codex runs a hook.
fn run_hook(env: &Env, input: &[u8]) -> std::process::Output {
    let mut c = env.cmd(&["hook", "codex", "--home", env.home.to_str().unwrap()]);
    c.stdin(Stdio::piped());
    let mut child = c.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

/// Codex is about to ask for an approval in `turn`: its `PermissionRequest`
/// hook runs.
fn hook(env: &Env, turn: &str) {
    let input = json!({
        "session_id": T, "turn_id": turn, "hook_event_name": "PermissionRequest",
        "tool_name": "Bash", "tool_input": { "command": "echo TOOL_INPUT_CANARY", "description": "DESCRIPTION_CANARY" },
        "cwd": "/tmp/CWD_CANARY", "transcript_path": "/tmp/TRANSCRIPT_CANARY.jsonl",
        "model": "MODEL_CANARY", "permission_mode": "default" });
    let out = run_hook(env, input.to_string().as_bytes());
    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stdout.is_empty(),
        "a hook that prints could decide the request: {out:?}"
    );
    assert!(out.stderr.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
}

fn hook_log(env: &Env) -> PathBuf {
    env.home
        .join("native")
        .join("codex")
        .join(format!("{T}.requests.jsonl"))
}

/// A hook log line as `t3-watch hook codex` writes it.
fn record_line(turn: &str, at: u64) -> String {
    json!({ "atMs": at, "event": "PermissionRequest", "tool": "Bash", "turn": turn }).to_string() + "\n"
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn append(path: &std::path::Path, text: &str) {
    let mut f = fs::OpenOptions::new().append(true).create(true).open(path).unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

fn wait(env: &Env, cx: &MockCodex, args: &[&str]) -> std::process::Command {
    let mut all = vec!["wait"];
    all.extend_from_slice(args);
    let mut c = env.cmd(&all);
    c.env("T3_WATCH_CODEX_SOCKET", &cx.sock);
    c
}

// ---- the mock app-server ---------------------------------------------------

fn b64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n =
            (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            out.push(if i <= c.len() {
                A[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

fn frame(s: &mut UnixStream, fin: bool, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
    s.write_all(&frame_bytes(fin, opcode, payload))
}

fn frame_bytes(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![if fin { 0x80 } else { 0 } | opcode];
    let n = payload.len();
    if n < 126 {
        f.push(n as u8);
    } else if n < 65536 {
        f.push(126);
        f.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        f.push(127);
        f.extend_from_slice(&(n as u64).to_be_bytes());
    }
    f.extend_from_slice(payload);
    f
}

fn send(s: &mut UnixStream, v: &Value, fragment: bool) -> std::io::Result<()> {
    let text = v.to_string();
    if fragment && text.len() > 1 {
        let (a, b) = text.as_bytes().split_at(text.len() / 2);
        frame(s, false, 0x1, a)?;
        frame(s, true, 0x0, b)
    } else {
        frame(s, true, 0x1, text.as_bytes())
    }
}

/// One client frame: (opcode, payload, masked). None at EOF.
fn read_frame(s: &mut UnixStream) -> Option<(u8, Vec<u8>, bool)> {
    let mut h = [0u8; 2];
    s.read_exact(&mut h).ok()?;
    let masked = h[1] & 0x80 != 0;
    let mut len = u64::from(h[1] & 0x7F);
    if len == 126 {
        let mut b = [0u8; 2];
        s.read_exact(&mut b).ok()?;
        len = u64::from(u16::from_be_bytes(b));
    } else if len == 127 {
        let mut b = [0u8; 8];
        s.read_exact(&mut b).ok()?;
        len = u64::from_be_bytes(b);
    }
    let mut mask = [0u8; 4];
    if masked {
        s.read_exact(&mut mask).ok()?;
    }
    let mut payload = vec![0u8; len as usize];
    s.read_exact(&mut payload).ok()?;
    if masked {
        payload.iter_mut().enumerate().for_each(|(i, b)| *b ^= mask[i % 4]);
    }
    Some((h[0] & 0x0F, payload, masked))
}

fn serve(mut s: UnixStream, st: Arc<Mutex<Codex>>) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if s.read_exact(&mut byte).is_err() || head.len() > 8192 {
            return;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_string();
    let Some(key) = head
        .lines()
        .find_map(|l| {
            l.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("sec-websocket-key"))
        })
        .map(|(_, v)| v.trim().to_string())
    else {
        return;
    };
    let digest = ring::digest::digest(
        &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
        format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
    );
    let accept = b64(digest.as_ref());
    let _ = s.write_all(
        format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .as_bytes(),
    );
    let w = Arc::new(Mutex::new(s.try_clone().unwrap()));
    {
        let mut g = st.lock().unwrap();
        g.connections += 1;
        g.writers.push(w.clone());
    }
    let mut requests = 0usize;
    while let Some((opcode, payload, masked)) = read_frame(&mut s) {
        if !masked {
            st.lock().unwrap().unmasked += 1;
        }
        match opcode {
            0x8 => return,
            0xA => continue,
            0x1 => {}
            _ => continue,
        }
        let Ok(m) = serde_json::from_slice::<Value>(&payload) else {
            continue;
        };
        let Some(method) = m["method"].as_str().map(str::to_string) else {
            // A response: the only request the server sent is its approval.
            st.lock().unwrap().answers.push(m);
            continue;
        };
        let (fragment, reply) = {
            let mut g = st.lock().unwrap();
            g.calls.push((method.clone(), m["params"].clone()));
            let id = m["id"].clone();
            if id.is_null() {
                continue;
            }
            requests += 1;
            let reply = match method.as_str() {
                "initialize" => json!({ "id": id, "result": {
                    "userAgent": "mock-codex/0.0", "codexHome": "/nonexistent",
                    "platformFamily": "unix", "platformOs": "macos" } }),
                "thread/read" if g.missing => json!({ "id": id, "error": {
                    "code": -32600, "message": format!("thread not found: {}", m["params"]["threadId"]) } }),
                "thread/read" => json!({ "id": id, "result": { "thread": {
                    "id": T, "sessionId": T, "cwd": "/tmp/mock-project", "status": g.status,
                    "preview": "PREVIEW_CANARY first message" } } }),
                "hooks/list" if g.hooks.is_some() => json!({ "id": id, "result": { "data": [{
                    "cwd": m["params"]["cwds"][0], "hooks": g.hooks, "errors": [], "warnings": [] }] } }),
                "thread/turns/list" if g.unsupported > 0 => {
                    g.unsupported -= 1;
                    let message = g
                        .unsupported_message
                        .clone()
                        .unwrap_or("list_turns is not supported yet".into());
                    json!({ "id": id, "error": { "code": -32600, "message": message } })
                }
                "thread/turns/list" if g.unmaterialized => json!({ "id": id, "error": { "code": -32600,
                    "message": format!("thread {T} is not materialized yet; thread/turns/list is unavailable \
                                        before first user message") } }),
                "thread/turns/list" => {
                    let p = &m["params"];
                    let mut turns = g.turns.clone();
                    if p["sortDirection"] == "desc" {
                        turns.reverse();
                    }
                    let from: usize = p["cursor"].as_str().and_then(|c| c.parse().ok()).unwrap_or(0);
                    let limit = p["limit"].as_u64().unwrap_or(25) as usize;
                    let page: Vec<Value> = turns
                        .iter()
                        .skip(from)
                        .take(limit)
                        .map(|t| {
                            let mut t = t.clone();
                            t["items"] = json!([]);
                            t["pad"] = json!("x".repeat(g.pad));
                            t
                        })
                        .collect();
                    let next = (from + limit < turns.len()).then(|| (from + limit).to_string());
                    json!({ "id": id, "result": { "data": page, "nextCursor": next, "backwardsCursor": null } })
                }
                _ => json!({ "id": id, "error": { "code": -32601, "message": "unexpected method" } }),
            };
            (g.fragment, reply)
        };
        // Noise first, as a real app-server sends: another thread's status,
        // a ping, and (once) its own approval request, which no watcher may
        // answer.
        let mut out = w.lock().unwrap();
        let _ = send(
            &mut out,
            &json!({ "method": "thread/status/changed",
            "params": { "threadId": "01a1-another-thread", "status": { "type": "active", "activeFlags": ["waitingOnApproval"] } } }),
            fragment,
        );
        // Closing after this answer: the ping and the answer go out in one
        // write and the connection shuts at once, so the client's pong
        // always meets a closed peer (on Linux it did by timing alone, and
        // the failed pong threw the answer away).
        if st.lock().unwrap().drop_after.is_some_and(|n| requests >= n) {
            let mut bytes = frame_bytes(true, 0x9, b"hi");
            bytes.extend(frame_bytes(true, 0x1, reply.to_string().as_bytes()));
            let _ = out.write_all(&bytes);
            // Closed for good: every descriptor of it, the kept writer too.
            drop(out);
            st.lock().unwrap().writers.retain(|x| !Arc::ptr_eq(x, &w));
            drop(w);
            drop(s);
            return;
        }
        let _ = frame(&mut out, true, 0x9, b"hi");
        if method == "initialize" {
            let _ = send(
                &mut out,
                &json!({ "id": "srv-approval-1", "method": "item/commandExecution/requestApproval",
                "params": { "threadId": T, "turnId": "t1", "itemId": "i1", "command": "true" } }),
                fragment,
            );
        }
        if send(&mut out, &reply, fragment).is_err() {
            return;
        }
        drop(out);
        if st.lock().unwrap().drop_after.is_some_and(|n| requests >= n) {
            let _ = s.shutdown(std::net::Shutdown::Both);
            return;
        }
    }
}

fn assert_read_only(cx: &MockCodex) {
    let g = cx.st.lock().unwrap();
    for (m, _) in &g.calls {
        assert!(READS.contains(&m.as_str()), "t3-watch sent {m}");
    }
    assert!(
        g.answers.is_empty(),
        "t3-watch answered a server request: {:?}",
        g.answers
    );
    assert_eq!(g.unmasked, 0, "client frames must be masked");
    for (m, p) in &g.calls {
        if m == "thread/turns/list" {
            assert_eq!(
                p["itemsView"], "notLoaded",
                "turn items (message text) must not be read"
            );
        }
    }
}

// ---- tests -----------------------------------------------------------------

#[test]
fn a_turn_is_watched_read_only_and_server_requests_are_never_answered() {
    for daemon in [true, false] {
        let env = Env::new();
        let cx = MockCodex::start(&env);
        // A long answer (8-byte frame lengths), sent in fragments.
        cx.with(|c| {
            c.pad = 40_000;
            c.fragment = true;
        });
        cx.set(
            json!({ "type": "active", "activeFlags": [] }),
            &[("turn-1", "completed"), ("turn-2", "inProgress")],
        );
        let hook_rx = MockHook::start();
        let mut args = vec!["--timeout", "20s", "--webhook-url", &hook_rx.url];
        if !daemon {
            args.push("--no-daemon");
        }
        let t = target();
        args.push(&t);
        let mut child = wait(&env, &cx, &args).spawn().unwrap();
        still_running(&mut child, Duration::from_millis(1500));
        cx.set(
            json!({ "type": "idle" }),
            &[("turn-1", "completed"), ("turn-2", "completed")],
        );
        let (code, out, err) = finish(child, Duration::from_secs(10));
        assert_eq!(code, 0, "daemon={daemon}: {out} {err}");
        assert_eq!(out["outcome"], "completed", "{out}");
        let th = &out["threads"][0];
        assert_eq!(th["threadId"], t, "{out}");
        assert_eq!(th["runOrdinal"], 2, "{out}");
        assert!(!out.to_string().contains("PREVIEW_CANARY"), "{out}");
        let reqs = hook_rx.requests();
        assert_eq!(reqs.len(), 1, "daemon={daemon}");
        assert_eq!(reqs[0].headers["x-t3-watch-event"], "settled");
        assert!(String::from_utf8_lossy(&reqs[0].body).contains(&t));
        assert!(!String::from_utf8_lossy(&reqs[0].body).contains("PREVIEW_CANARY"));
        assert_read_only(&cx);
        // One connection, kept between polls, initialized once.
        let methods = cx.methods();
        assert!(
            methods.iter().filter(|m| *m == "thread/read").count() >= 2,
            "{methods:?}"
        );
        assert_eq!(methods.iter().filter(|m| *m == "initialize").count(), 1, "{methods:?}");
        if daemon {
            let s = env.status();
            assert!(s["running"] == true, "{s}");
        }
    }
}

#[test]
fn waiting_on_approval_or_input_is_needs_input_with_its_own_key() {
    let env = Env::new();
    let cx = MockCodex::start(&env);
    let t = target();
    let run = |args: &[&str]| {
        let mut all = vec!["--no-daemon"];
        all.extend_from_slice(args);
        all.push(&t);
        wait(&env, &cx, &all).output().unwrap()
    };
    cx.set(
        json!({ "type": "active", "activeFlags": ["waitingOnApproval"] }),
        &[("turn-1", "inProgress")],
    );
    let out = run(&["--timeout", "5s"]);
    assert_eq!(out.status.code(), Some(5), "{}", String::from_utf8_lossy(&out.stdout));
    let v = output_json(&out);
    assert_eq!(v["outcome"], "needs_input", "{v}");
    let approval = v["threads"][0]["pendingKey"].clone();
    let action = v["action"].as_str().unwrap_or("");
    assert!(
        action.contains("never answers") && !action.contains("t3_pending_request"),
        "{v}"
    );
    assert!(approval.as_str().is_some_and(|k| k.starts_with("pk-")), "{v}");
    // User input asked as well: another request, another key.
    cx.set(
        json!({ "type": "active", "activeFlags": ["waitingOnApproval", "waitingOnUserInput"] }),
        &[("turn-1", "inProgress")],
    );
    let out = run(&["--timeout", "5s"]);
    assert_eq!(out.status.code(), Some(5));
    let both = output_json(&out)["threads"][0]["pendingKey"].clone();
    assert_ne!(both, approval);
    // Acknowledged through the daemon, which keeps the counting connection
    // between waits: the same request does not wake again.
    cx.set(
        json!({ "type": "active", "activeFlags": ["waitingOnApproval"] }),
        &[("turn-1", "inProgress")],
    );
    let daemon = |args: &[&str]| {
        let mut all = args.to_vec();
        all.push(&t);
        wait(&env, &cx, &all).output().unwrap()
    };
    let out = daemon(&["--timeout", "5s"]);
    assert_eq!(out.status.code(), Some(5));
    let ack = output_json(&out)["threads"][0]["pendingKey"]
        .as_str()
        .unwrap()
        .to_string();
    let out = daemon(&["--timeout", "1500ms", "--acknowledge", &ack]);
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stdout));
    // A process without the daemon reads the count the daemon keeps.
    let out = run(&["--timeout", "1500ms", "--acknowledge", &ack]);
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stdout));
    // Answered by its owner: running again, then completed.
    cx.set(
        json!({ "type": "active", "activeFlags": [] }),
        &[("turn-1", "inProgress")],
    );
    let out = run(&["--timeout", "1500ms"]);
    assert_eq!(out.status.code(), Some(3), "running, not needs-input");
    cx.set(json!({ "type": "idle" }), &[("turn-1", "completed")]);
    let out = run(&["--timeout", "5s"]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
    // Interrupted and failed turns are failures; an error status is too.
    for (status, turn) in [
        ("idle", "interrupted"),
        ("notLoaded", "failed"),
        ("systemError", "completed"),
    ] {
        cx.set(json!({ "type": status }), &[("turn-1", turn)]);
        let out = run(&["--timeout", "5s"]);
        assert_eq!(
            out.status.code(),
            Some(6),
            "{status}/{turn}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
    assert_read_only(&cx);
}

#[test]
fn a_dropped_connection_is_reopened_and_a_turn_finished_meanwhile_is_seen() {
    let env = Env::new();
    let cx = MockCodex::start(&env);
    // Every connection serves one poll (initialize, read, hooks/list,
    // list), then closes.
    cx.with(|c| c.drop_after = Some(4));
    cx.set(
        json!({ "type": "active", "activeFlags": [] }),
        &[("turn-1", "inProgress")],
    );
    let t = target();
    let mut child = wait(&env, &cx, &["--timeout", "20s", &t]).spawn().unwrap();
    still_running(&mut child, Duration::from_millis(2500));
    cx.set(json!({ "type": "notLoaded" }), &[("turn-1", "completed")]);
    let (code, out, err) = finish(child, Duration::from_secs(30));
    assert_eq!(code, 0, "{out} {err}");
    assert_eq!(out["threads"][0]["runOrdinal"], 1, "{out}");
    let connections = cx.st.lock().unwrap().connections;
    assert!(connections >= 2, "the watcher never reconnected ({connections})");
    assert_read_only(&cx);
}

#[test]
fn turns_are_counted_once_and_a_pinned_wait_follows_the_next_turn() {
    let env = Env::new();
    let cx = MockCodex::start(&env);
    let ids: Vec<String> = (1..=120).map(|n| format!("turn-{n}")).collect();
    let turns: Vec<(&str, &str)> = ids.iter().map(|i| (i.as_str(), "completed")).collect();
    cx.set(json!({ "type": "idle" }), &turns);
    let t = target();
    let out = wait(&env, &cx, &["--no-daemon", "--timeout", "5s", &t])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(output_json(&out)["threads"][0]["runOrdinal"], 120);

    // A bot pins the next turn before it starts it: the finished turn 120
    // does not end the wait.
    cx.with(|c| c.calls.clear());
    let mut child = wait(
        &env,
        &cx,
        &["--no-daemon", "--timeout", "20s", "--expect-min-run-ordinal", "121", &t],
    )
    .spawn()
    .unwrap();
    still_running(&mut child, Duration::from_millis(1500));
    let mut more = turns.clone();
    more.push(("turn-121", "inProgress"));
    cx.set(json!({ "type": "active", "activeFlags": [] }), &more);
    still_running(&mut child, Duration::from_millis(1500));
    more.pop();
    more.push(("turn-121", "completed"));
    cx.set(json!({ "type": "idle" }), &more);
    let (code, out, err) = finish(child, Duration::from_secs(10));
    assert_eq!(code, 0, "{out} {err}");
    assert_eq!(out["threads"][0]["runOrdinal"], 121, "{out}");
    // The full count paged once (100 + 20); later polls only read the
    // newest page.
    let g = cx.st.lock().unwrap();
    let ascending = g
        .calls
        .iter()
        .filter(|(m, p)| m == "thread/turns/list" && p["sortDirection"] == "asc")
        .count();
    let polls = g.calls.iter().filter(|(m, _)| m == "thread/read").count();
    assert_eq!(
        ascending,
        2,
        "{:?}",
        g.calls.iter().map(|(m, p)| format!("{m} {p}")).collect::<Vec<_>>()
    );
    assert!(polls >= 3, "{polls}");
    drop(g);
    assert_read_only(&cx);
}

#[test]
fn a_fresh_thread_keeps_waiting_and_an_unknown_one_is_an_error() {
    let env = Env::new();
    let cx = MockCodex::start(&env);
    let t = target();
    cx.set(json!({ "type": "idle" }), &[]);
    let out = wait(&env, &cx, &["--no-daemon", "--timeout", "1500ms", &t])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "no turn yet is not a completion");
    // Before its first user message the app-server refuses to list turns:
    // still no turn, not an error.
    cx.with(|c| c.unmaterialized = true);
    let out = wait(&env, &cx, &["--no-daemon", "--timeout", "1500ms", &t])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(output_json(&out)["threads"][0]["title"], "Codex thread (no turn yet)");
    cx.with(|c| c.unmaterialized = false);
    cx.with(|c| c.missing = true);
    let out = wait(&env, &cx, &["--no-daemon", "--timeout", "10s", &t])
        .output()
        .unwrap();
    let v = output_json(&out);
    assert_eq!(out.status.code(), Some(2), "{v}");
    assert!(v.to_string().contains("not found"), "{v}");
    assert_read_only(&cx);
}

#[test]
fn without_an_app_server_the_wait_keeps_trying_until_its_timeout() {
    let env = Env::new();
    let missing = env.home.join("none.sock");
    let t = target();
    let out = env
        .cmd(&["wait", "--no-daemon", "--timeout", "2s", &t])
        .env("T3_WATCH_CODEX_SOCKET", &missing)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stdout));
}

#[test]
fn codex_targets_are_validated_and_t3_contracts_refused() {
    let env = Env::new();
    let cx = MockCodex::start(&env);
    let t = target();
    for args in [
        vec!["--no-daemon", "codex://threads/a/b"],
        vec!["--no-daemon", "codex://threads/"],
        vec!["--no-daemon", "codex://projects/x"],
        vec!["--no-daemon", "--until", "workstream", &t],
        vec!["--no-daemon", "--final-marker", "DONE", &t],
    ] {
        let out = wait(&env, &cx, &args).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
    assert!(cx.methods().is_empty(), "{:?}", cx.methods());
}

#[test]
fn successive_requests_in_one_turn_each_wake_once() {
    // A reader sees only the flag, the same for every request of a turn.
    // Each time the counting connection sees it come back on is a new
    // request. Through the daemon, a request already relayed stays
    // acknowledged from one wait to the next.
    let env = Env::new();
    let cx = MockCodex::start(&env);
    let t = target();
    let run = |args: &[&str]| {
        let mut all = args.to_vec();
        all.push(&t);
        wait(&env, &cx, &all).output().unwrap()
    };
    let key = |out: &std::process::Output| output_json(out)["threads"][0]["pendingKey"].clone();
    let asks = json!({ "type": "active", "activeFlags": ["waitingOnApproval"] });
    let busy = json!({ "type": "active", "activeFlags": [] });
    cx.set(asks.clone(), &[("turn-1", "inProgress")]);
    let out = run(&["--timeout", "5s"]);
    assert_eq!(out.status.code(), Some(5));
    let first = key(&out);
    // The next wait, the same request: acknowledged, it keeps waiting.
    let out = run(&["--timeout", "1500ms", "--acknowledge", first.as_str().unwrap()]);
    assert_eq!(out.status.code(), Some(3));
    // The owner answers, then a second request opens in the same turn.
    cx.set(busy.clone(), &[("turn-1", "inProgress")]);
    let out = run(&["--timeout", "1500ms", "--acknowledge", first.as_str().unwrap()]);
    assert_eq!(out.status.code(), Some(3));
    cx.set(asks.clone(), &[("turn-1", "inProgress")]);
    let out = run(&["--timeout", "5s", "--acknowledge", first.as_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(5),
        "the second request was hidden by the first one's key"
    );
    let second = key(&out);
    assert_ne!(second, first);
    // One wait sees a third come and go by itself.
    let mut child = wait(
        &env,
        &cx,
        &["--timeout", "20s", "--acknowledge", second.as_str().unwrap(), &t],
    )
    .spawn()
    .unwrap();
    still_running(&mut child, Duration::from_millis(1200));
    cx.set(busy, &[("turn-1", "inProgress")]);
    std::thread::sleep(Duration::from_millis(1500));
    cx.set(asks, &[("turn-1", "inProgress")]);
    let (code, out, err) = finish(child, Duration::from_secs(15));
    assert_eq!(code, 5, "{out} {err}");
    let third = out["threads"][0]["pendingKey"].clone();
    assert!(third != first && third != second, "{out}");
    let ids = out["threads"][0]["pendingRequestIds"].to_string();
    assert!(ids.contains("turn-1-approval-3"), "{ids}");
    assert_read_only(&cx);
}

#[test]
fn a_request_that_opens_between_two_reads_is_still_counted() {
    // Review ordinal 15: polling the flag missed a second request whose
    // predecessor closed and reopened between two reads. Every connection
    // gets the thread's status notifications, so the off-and-on is seen
    // even though both reads say "waiting".
    let env = Env::new();
    let cx = MockCodex::start(&env);
    let t = target();
    let asks = json!({ "type": "active", "activeFlags": ["waitingOnApproval"] });
    cx.set(asks.clone(), &[("turn-1", "inProgress")]);
    let out = wait(&env, &cx, &["--no-daemon", "--timeout", "5s", &t])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5));
    let first = output_json(&out)["threads"][0]["pendingKey"]
        .as_str()
        .unwrap()
        .to_string();
    // A fresh count, so that this one wait starts at request 1 again.
    fs::remove_file(env.home.join("native").join("codex").join(format!("{T}.json"))).unwrap();
    let mut child = wait(
        &env,
        &cx,
        &["--no-daemon", "--timeout", "20s", "--acknowledge", &first, &t],
    )
    .spawn()
    .unwrap();
    still_running(&mut child, Duration::from_millis(1200));
    cx.push(json!({ "type": "active", "activeFlags": [] }));
    cx.push(asks);
    let (code, out, err) = finish(child, Duration::from_secs(15));
    assert_eq!(code, 5, "the second request was missed: {out} {err}");
    let ids = out["threads"][0]["pendingRequestIds"].to_string();
    assert!(ids.contains("turn-1-approval-2"), "{ids}");
    assert_read_only(&cx);
}

#[test]
fn the_daemon_keeps_counting_requests_between_waits() {
    // A bot relays request 1 and waits again; request 2 opens in between.
    // The daemon keeps the connection of a watch that ended mid-turn, so
    // the next wait knows it is a new request.
    let env = Env::new();
    let cx = MockCodex::start(&env);
    let t = target();
    let asks = json!({ "type": "active", "activeFlags": ["waitingOnApproval"] });
    cx.set(asks.clone(), &[("turn-1", "inProgress")]);
    let out = wait(&env, &cx, &["--timeout", "5s", &t]).output().unwrap();
    assert_eq!(out.status.code(), Some(5), "{}", String::from_utf8_lossy(&out.stdout));
    let first = output_json(&out)["threads"][0]["pendingKey"]
        .as_str()
        .unwrap()
        .to_string();
    env.wait_status("the connection kept", Duration::from_secs(5), |s| s["codexKept"] == 1);
    cx.push(json!({ "type": "active", "activeFlags": [] }));
    cx.push(asks);
    std::thread::sleep(Duration::from_millis(600));
    let out = wait(&env, &cx, &["--timeout", "5s", "--acknowledge", &first, &t])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5), "{}", String::from_utf8_lossy(&out.stdout));
    let v = output_json(&out);
    assert!(
        v["threads"][0]["pendingRequestIds"].to_string().contains("approval-2"),
        "{v}"
    );
    // The turn ends: nothing is kept, and the daemon may idle out.
    cx.set(json!({ "type": "idle" }), &[("turn-1", "completed")]);
    env.wait_status("nothing kept", Duration::from_secs(10), |s| {
        s["running"] == false || s["codexKept"] == 0
    });
    assert_read_only(&cx);
}

#[test]
fn a_request_replaced_while_nothing_counts_is_counted_again() {
    // Review ordinal 16: with nothing connected, request 1 can be answered
    // and request 2 open, and the flag says "waiting" before and after. A
    // connection that takes the count over counts a flag still on once
    // more: an extra wake for the same request, never a missed one.
    let env = Env::new();
    let cx = MockCodex::start(&env);
    let t = target();
    let asks = json!({ "type": "active", "activeFlags": ["waitingOnApproval"] });
    cx.set(asks.clone(), &[("turn-1", "inProgress")]);
    let out = wait(&env, &cx, &["--no-daemon", "--timeout", "5s", &t])
        .output()
        .unwrap();
    let first = output_json(&out)["threads"][0]["pendingKey"]
        .as_str()
        .unwrap()
        .to_string();
    cx.push(json!({ "type": "active", "activeFlags": [] }));
    cx.push(asks);
    let out = wait(
        &env,
        &cx,
        &["--no-daemon", "--timeout", "5s", "--acknowledge", &first, &t],
    )
    .output()
    .unwrap();
    assert_eq!(out.status.code(), Some(5), "{}", String::from_utf8_lossy(&out.stdout));
    let v = output_json(&out);
    assert!(
        v["threads"][0]["pendingRequestIds"].to_string().contains("approval-2"),
        "{v}"
    );
}

#[test]
fn without_the_hook_a_request_that_opens_and_closes_while_nothing_counts_is_not_seen() {
    // The app-server keeps no record of past requests, so without the hook
    // (an app-server without `hooks/list`, or the hook listed but not
    // trusted, so Codex never runs it) a request answered before any
    // t3-watch connection counts again leaves no trace. The result says so.
    for trust in [None, Some("untrusted")] {
        let env = Env::new();
        let cx = MockCodex::start(&env);
        if let Some(trust) = trust {
            cx.install_hook(&env, trust);
        }
        let t = target();
        let busy = json!({ "type": "active", "activeFlags": [] });
        cx.set(busy.clone(), &[("turn-1", "inProgress")]);
        let out = wait(&env, &cx, &["--no-daemon", "--timeout", "1500ms", &t])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(3));
        cx.push(json!({ "type": "active", "activeFlags": ["waitingOnApproval"] }));
        cx.push(busy);
        let out = wait(&env, &cx, &["--no-daemon", "--timeout", "1500ms", &t])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stdout));
        let v = output_json(&out);
        let th = &v["threads"][0];
        assert!(th["pendingKey"].is_null(), "{v}");
        assert!(th["recoveredRequests"].is_null(), "{v}");
        assert_eq!(th["requestRecovery"], "none", "{v}");
        assert_read_only(&cx);
    }
}

#[test]
fn a_request_that_opens_and_closes_while_nothing_counts_is_recovered_from_the_hook_log() {
    // X3b (docs/acceptance-manifest.md): the app-server keeps no record of
    // past requests; Codex's PermissionRequest hook does. A request that
    // opens and closes while no connection counts (no wait, no daemon) is
    // found in the hook log by the next connection that counts: reported
    // once as `requests_recovered` progress, then in every result of the
    // turn. The next request is numbered after it. Without the daemon, and
    // with the daemon stopped between the waits.
    for daemon in [false, true] {
        let env = Env::new();
        let cx = MockCodex::start(&env);
        cx.install_hook(&env, "trusted");
        let t = target();
        let run = |extra: &[&str]| {
            let mut all: Vec<&str> = if daemon { vec![] } else { vec!["--no-daemon"] };
            all.extend_from_slice(extra);
            all.push(&t);
            wait(&env, &cx, &all).output().unwrap()
        };
        let busy = json!({ "type": "active", "activeFlags": [] });
        let asks = json!({ "type": "active", "activeFlags": ["waitingOnApproval"] });
        cx.set(busy.clone(), &[("turn-1", "inProgress")]);
        let out = run(&["--timeout", "1500ms"]);
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(output_json(&out)["threads"][0]["requestRecovery"], "hook");
        if daemon {
            assert!(env.run(&["stop"]).status.success());
        }
        // Nothing counts: the request opens (its hook runs) and its owner
        // answers it.
        cx.push(asks.clone());
        hook(&env, "turn-1");
        cx.push(busy.clone());
        let rx = MockHook::start();
        let out = run(&["--timeout", "2s", "--webhook-url", &rx.url]);
        assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stdout));
        let v = output_json(&out);
        let th = &v["threads"][0];
        assert!(th["pendingKey"].is_null(), "nothing waits on it: {v}");
        let rec = th["recoveredRequests"].as_array().expect("recovered");
        assert_eq!(rec.len(), 1, "{v}");
        assert_eq!(rec[0]["id"], "codex-turn-1-approval-1");
        assert_eq!(rec[0]["kind"], "approval");
        assert_eq!(rec[0]["open"], false);
        assert_eq!(rec[0]["tool"], "Bash");
        assert!(rec[0]["openedAt"].as_str().unwrap() <= rec[0]["recoveredAt"].as_str().unwrap());
        let sent = rx.requests();
        assert_eq!(sent.len(), 1, "daemon={daemon}");
        assert_eq!(sent[0].headers["x-t3-watch-event"], "requests_recovered");
        let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
        assert_eq!(body["progress"], true);
        assert_eq!(body["recovered"][0]["id"], "codex-turn-1-approval-1");
        // A later wait lists it again, without announcing it again.
        let rx = MockHook::start();
        let out = run(&["--timeout", "1500ms", "--webhook-url", &rx.url]);
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(
            output_json(&out)["threads"][0]["recoveredRequests"][0]["id"],
            "codex-turn-1-approval-1"
        );
        assert!(rx.requests().is_empty());
        // The next request, seen live, is number 2.
        cx.set(asks, &[("turn-1", "inProgress")]);
        hook(&env, "turn-1");
        let out = run(&["--timeout", "5s"]);
        assert_eq!(out.status.code(), Some(5), "{}", String::from_utf8_lossy(&out.stdout));
        let v = output_json(&out);
        assert!(
            v["threads"][0]["pendingRequestIds"]
                .to_string()
                .contains("turn-1-approval-2"),
            "{v}"
        );
        assert_eq!(
            v["threads"][0]["recoveredRequests"].as_array().map(Vec::len),
            Some(1),
            "{v}"
        );
        // The log holds metadata only.
        let log = fs::read_to_string(hook_log(&env)).unwrap();
        for canary in [
            "TOOL_INPUT_CANARY",
            "DESCRIPTION_CANARY",
            "CWD_CANARY",
            "TRANSCRIPT_CANARY",
            "MODEL_CANARY",
        ] {
            assert!(!log.contains(canary), "{log}");
        }
        assert!(!out.stdout.windows(6).any(|w| w == b"CANARY"));
        assert_read_only(&cx);
        if daemon {
            env.run(&["stop"]);
        }
    }
}

#[test]
fn requests_counted_live_are_matched_to_their_hook_records_not_recovered() {
    // While a connection counts, every hook record is one of the requests it
    // counted, whether the record lands before the count or after it.
    let env = Env::new();
    let cx = MockCodex::start(&env);
    cx.install_hook(&env, "trusted");
    let t = target();
    let asks = json!({ "type": "active", "activeFlags": ["waitingOnApproval"] });
    let busy = json!({ "type": "active", "activeFlags": [] });
    let key = |out: &std::process::Output| {
        output_json(out)["threads"][0]["pendingKey"]
            .as_str()
            .unwrap()
            .to_string()
    };
    cx.set(asks.clone(), &[("turn-1", "inProgress")]);
    hook(&env, "turn-1");
    let out = wait(&env, &cx, &["--timeout", "5s", &t]).output().unwrap();
    assert_eq!(out.status.code(), Some(5));
    let first = key(&out);
    env.wait_status("the connection kept", Duration::from_secs(5), |s| s["codexKept"] == 1);
    // The second request is counted live by the kept connection; its hook
    // record lands only after a wait reported it.
    cx.set(busy.clone(), &[("turn-1", "inProgress")]);
    std::thread::sleep(Duration::from_millis(400));
    cx.set(asks.clone(), &[("turn-1", "inProgress")]);
    std::thread::sleep(Duration::from_millis(400));
    let out = wait(&env, &cx, &["--timeout", "5s", "--acknowledge", &first, &t])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5), "{}", String::from_utf8_lossy(&out.stdout));
    let v = output_json(&out);
    assert!(
        v["threads"][0]["pendingRequestIds"]
            .to_string()
            .contains("turn-1-approval-2"),
        "{v}"
    );
    let second = key(&out);
    hook(&env, "turn-1");
    let out = wait(&env, &cx, &["--timeout", "1500ms", "--acknowledge", &second, &t])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "the late record is request 2, not a new one"
    );
    let v = output_json(&out);
    assert!(v["threads"][0]["recoveredRequests"].is_null(), "{v}");
    cx.set(json!({ "type": "idle" }), &[("turn-1", "completed")]);
    let out = wait(&env, &cx, &["--timeout", "5s", &t]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v = output_json(&out);
    assert!(v["threads"][0]["recoveredRequests"].is_null(), "{v}");
    assert_eq!(fs::read_to_string(hook_log(&env)).unwrap().lines().count(), 2);
    assert_read_only(&cx);
    env.run(&["stop"]);
}

#[test]
fn with_the_hook_a_request_replaced_while_nothing_counts_is_counted_once() {
    // Without the hook, a connection that takes the count over while the
    // flag is on counts it once more (an extra wake). With it, the hook log
    // tells: a new record is a new request, no new record the same one.
    let env = Env::new();
    let cx = MockCodex::start(&env);
    cx.install_hook(&env, "trusted");
    let t = target();
    let asks = json!({ "type": "active", "activeFlags": ["waitingOnApproval"] });
    cx.set(asks.clone(), &[("turn-1", "inProgress")]);
    hook(&env, "turn-1");
    let run = |extra: &[&str]| {
        let mut all = vec!["--no-daemon"];
        all.extend_from_slice(extra);
        all.push(&t);
        wait(&env, &cx, &all).output().unwrap()
    };
    let out = run(&["--timeout", "5s"]);
    assert_eq!(out.status.code(), Some(5));
    let first = output_json(&out)["threads"][0]["pendingKey"]
        .as_str()
        .unwrap()
        .to_string();
    // The same request, from a new connection: acknowledged, no new wake.
    let out = run(&["--timeout", "1500ms", "--acknowledge", &first]);
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stdout));
    // Request 1 is answered and request 2 opens while nothing counts.
    cx.push(json!({ "type": "active", "activeFlags": [] }));
    cx.push(asks);
    hook(&env, "turn-1");
    let out = run(&["--timeout", "5s", "--acknowledge", &first]);
    assert_eq!(out.status.code(), Some(5), "{}", String::from_utf8_lossy(&out.stdout));
    let v = output_json(&out);
    let th = &v["threads"][0];
    assert!(th["pendingRequestIds"].to_string().contains("turn-1-approval-2"), "{v}");
    assert!(th["recoveredRequests"].is_null(), "it is open, not recovered: {v}");
    let second = th["pendingKey"].as_str().unwrap().to_string();
    let out = run(&["--timeout", "1500ms", "--acknowledge", &second]);
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stdout));
    assert_read_only(&cx);
}

#[test]
fn a_trusted_hook_that_records_nothing_does_not_hide_a_replaced_request() {
    // The hook is listed and trusted but records nothing (its binary was
    // moved, say), and user-input requests have no hook at all. A request
    // replaced while nothing counts is then counted once more, as without
    // the hook: an extra wake, never a missed one.
    for flag in ["waitingOnApproval", "waitingOnUserInput"] {
        let env = Env::new();
        let cx = MockCodex::start(&env);
        cx.install_hook(&env, "trusted");
        let t = target();
        let asks = json!({ "type": "active", "activeFlags": [flag] });
        cx.set(asks.clone(), &[("turn-1", "inProgress")]);
        let out = wait(&env, &cx, &["--no-daemon", "--timeout", "5s", &t])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(5));
        let first = output_json(&out)["threads"][0]["pendingKey"]
            .as_str()
            .unwrap()
            .to_string();
        cx.push(json!({ "type": "active", "activeFlags": [] }));
        cx.push(asks);
        let out = wait(
            &env,
            &cx,
            &["--no-daemon", "--timeout", "5s", "--acknowledge", &first, &t],
        )
        .output()
        .unwrap();
        assert_eq!(
            out.status.code(),
            Some(5),
            "{flag}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let ids = output_json(&out)["threads"][0]["pendingRequestIds"].to_string();
        assert!(ids.contains("-2\""), "{flag}: {ids}");
        assert_read_only(&cx);
    }
}

#[test]
fn a_replaced_truncated_or_deleted_hook_log_loses_and_repeats_nothing() {
    // Review ordinal 21: a log shorter than the saved offset was read again
    // from its start, but the old count of records already read then
    // skipped the new ones, so later missed requests were never reported.
    // Each step below is a new --no-daemon process: across restarts too, a
    // record is reported once.
    let env = Env::new();
    let cx = MockCodex::start(&env);
    cx.install_hook(&env, "trusted");
    let t = target();
    let busy = json!({ "type": "active", "activeFlags": [] });
    cx.set(busy.clone(), &[("turn-1", "inProgress")]);
    let log = hook_log(&env);
    // The recovered requests' numbers, and how many this wait announced.
    let run = || {
        let out = wait(&env, &cx, &["--no-daemon", "--timeout", "1200ms", &t])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stdout));
        let v = output_json(&out);
        let ids: Vec<String> = v["threads"][0]["recoveredRequests"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| {
                r["id"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches("codex-turn-1-approval-")
                    .to_string()
            })
            .collect();
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        let announced: usize = err
            .lines()
            .filter_map(|l| l.strip_prefix("t3-watch: progress "))
            .map(|p| {
                serde_json::from_str::<Value>(p).unwrap()["recovered"]
                    .as_array()
                    .unwrap()
                    .len()
            })
            .sum();
        (ids.join(","), announced)
    };
    let unseen = || {
        cx.push(json!({ "type": "active", "activeFlags": ["waitingOnApproval"] }));
        hook(&env, "turn-1");
        cx.push(busy.clone());
    };
    assert_eq!(run(), (String::new(), 0));
    // An earlier turn's record first, so this turn's records start past
    // byte 0.
    append(&log, &record_line("turn-0", now_ms()));
    unseen();
    assert_eq!(run(), ("1".into(), 1));
    // Truncated to nothing, then the next request writes a shorter log.
    fs::write(&log, "").unwrap();
    assert_eq!(run(), ("1".into(), 0));
    unseen();
    assert_eq!(run(), ("1,2".into(), 1));
    // Truncated and grown past the cursor before anything read it.
    fs::write(&log, "").unwrap();
    append(&log, &record_line("turn-0", 1));
    append(&log, &record_line("turn-0", 2));
    unseen();
    assert_eq!(run(), ("1,2,3".into(), 1));
    // Deleted, then created again by the next request's hook.
    fs::remove_file(&log).unwrap();
    assert_eq!(run(), ("1,2,3".into(), 0));
    unseen();
    assert_eq!(run(), ("1,2,3,4".into(), 1));
    // Replaced by another file holding a record already read and a new one.
    let mut copy = fs::read_to_string(&log).unwrap();
    copy.push_str(&record_line("turn-1", now_ms()));
    let tmp = log.with_extension("new");
    fs::write(&tmp, copy).unwrap();
    fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    fs::rename(&tmp, &log).unwrap();
    assert_eq!(run(), ("1,2,3,4,5".into(), 1));
    // A last line still being written is left until it is complete.
    let line = record_line("turn-1", now_ms());
    let (head, tail) = line.split_at(line.len() / 2);
    append(&log, head);
    assert_eq!(run(), ("1,2,3,4,5".into(), 0));
    append(&log, tail);
    assert_eq!(run(), ("1,2,3,4,5,6".into(), 1));
    assert_eq!(run(), ("1,2,3,4,5,6".into(), 0));
    assert_read_only(&cx);
}

#[test]
fn a_hook_record_that_lands_long_after_its_count_is_still_that_request() {
    // Review ordinal 21: Codex runs a session's background hooks eight at a
    // time, so a record can land long after its request was counted. It
    // used to be matched only within 60 s; later, it looked like a new
    // request that had replaced the counted one: an extra wake. It is the
    // same request, and nothing is reported as recovered.
    let env = Env::new();
    let cx = MockCodex::start(&env);
    cx.install_hook(&env, "trusted");
    let t = target();
    cx.set(
        json!({ "type": "active", "activeFlags": ["waitingOnApproval"] }),
        &[("turn-1", "inProgress")],
    );
    let out = wait(&env, &cx, &["--timeout", "5s", &t]).output().unwrap();
    assert_eq!(out.status.code(), Some(5));
    let first = output_json(&out)["threads"][0]["pendingKey"]
        .as_str()
        .unwrap()
        .to_string();
    env.wait_status("the connection kept", Duration::from_secs(5), |s| s["codexKept"] == 1);
    // Its record, written two minutes after the count.
    append(&hook_log(&env), &record_line("turn-1", now_ms() + 120_000));
    let rx = MockHook::start();
    let out = wait(
        &env,
        &cx,
        &[
            "--timeout",
            "1500ms",
            "--acknowledge",
            &first,
            "--webhook-url",
            &rx.url,
            &t,
        ],
    )
    .output()
    .unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stdout));
    let v = output_json(&out);
    assert!(v["threads"][0]["recoveredRequests"].is_null(), "{v}");
    assert!(rx.requests().is_empty(), "nothing was recovered");
    assert_read_only(&cx);
    env.run(&["stop"]);
}

#[test]
fn the_codex_hook_records_metadata_only_and_never_decides() {
    let env = Env::new();
    hook(&env, "turn-1");
    let path = hook_log(&env);
    assert_eq!(mode(&path), 0o600);
    let log = fs::read_to_string(&path).unwrap();
    let lines: Vec<Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 1);
    let mut keys: Vec<&String> = lines[0].as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["atMs", "event", "tool", "turn"]);
    assert_eq!(lines[0]["turn"], "turn-1");
    assert!(!log.contains("CANARY"), "{log}");
    // Other events and bad input: nothing recorded, nothing printed, and
    // the hook still succeeds.
    for input in [
        json!({ "hook_event_name": "PreToolUse", "session_id": T, "turn_id": "turn-1" }).to_string(),
        "not json".to_string(),
        json!({ "hook_event_name": "PermissionRequest", "session_id": "../escape", "turn_id": "t" }).to_string(),
        json!({ "hook_event_name": "PermissionRequest", "session_id": T }).to_string(),
    ] {
        let out = run_hook(&env, input.as_bytes());
        assert_eq!(out.status.code(), Some(0), "{input}");
        assert!(out.stdout.is_empty(), "{input}");
    }
    assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 1);
    assert!(!env.home.join("native").join("escape.requests.jsonl").exists());
    // A log someone opened up is made private again by the next record.
    fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    hook(&env, "turn-1");
    assert_eq!(mode(&path), 0o600);
    assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 2);
    // The block to install: async, recording into this state directory.
    let out = env.run(&["hook", "codex", "--print-settings"]);
    assert!(out.status.success());
    let block = String::from_utf8(out.stdout).unwrap();
    assert!(
        block.contains("[[hooks.PermissionRequest]]\n[[hooks.PermissionRequest.hooks]]"),
        "{block}"
    );
    assert!(
        block.contains("type = \"command\"") && block.contains("async = true"),
        "{block}"
    );
    let name = env.home.file_name().unwrap().to_str().unwrap();
    assert!(block.contains("hook codex --home ") && block.contains(name), "{block}");
    let out = env.run(&["hook", "codex", "--home", "relative/dir", "--print-settings"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn a_thread_that_cannot_list_its_turns_yet_is_retried_not_failed() {
    // Seen once in six real runs: while the first message materializes the
    // thread, the app-server answered "list_turns is not supported yet",
    // and the wait failed with exit 2.
    let env = Env::new();
    let cx = MockCodex::start(&env);
    let t = target();
    cx.with(|c| c.unsupported = 2);
    cx.set(
        json!({ "type": "active", "activeFlags": ["waitingOnApproval"] }),
        &[("turn-1", "inProgress")],
    );
    let out = wait(&env, &cx, &["--no-daemon", "--timeout", "15s", &t])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(5), "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(cx.st.lock().unwrap().unsupported, 0);
    assert_read_only(&cx);
}

/// Waits through the daemon, whose drain rounds each read every kept
/// connection for `drain_ms` (the test hook), back to back.
fn waits_with_drain<'a>(
    env: &'a Env,
    cx: &'a MockCodex,
    drain_ms: &'a str,
) -> impl Fn(&[&str]) -> std::process::Output + 'a {
    move |args: &[&str]| {
        let mut all = vec!["wait"];
        all.extend_from_slice(args);
        let mut c = env.cmd(&all);
        c.env("T3_WATCH_CODEX_SOCKET", &cx.sock)
            .env("T3_WATCH_TEST_CODEX_DRAIN_MS", drain_ms);
        c.output().unwrap()
    }
}

#[test]
fn a_wait_that_starts_during_a_drain_round_keeps_the_counting_connection() {
    // Review ordinal 16 (P1): a drain round took every kept connection out
    // of sight, so a wait starting meanwhile opened a second one, and when
    // it ended, that one replaced the connection holding the counter: the
    // next request opened between waits shared its predecessor's key. Each
    // round here reads for 3 s, back to back, so the second wait starts
    // inside one.
    let env = Env::new();
    let cx = MockCodex::start(&env);
    let t = target();
    let run = waits_with_drain(&env, &cx, "3000");
    let asks = json!({ "type": "active", "activeFlags": ["waitingOnApproval"] });
    cx.set(asks.clone(), &[("turn-1", "inProgress")]);
    let out = run(&["--timeout", "5s", &t]);
    assert_eq!(out.status.code(), Some(5), "{}", String::from_utf8_lossy(&out.stdout));
    let first = output_json(&out)["threads"][0]["pendingKey"]
        .as_str()
        .unwrap()
        .to_string();
    env.wait_status("the connection kept", Duration::from_secs(10), |s| s["codexKept"] == 1);
    // Inside a drain round: the same request, still acknowledged.
    let out = run(&["--timeout", "8s", "--acknowledge", &first, &t]);
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(cx.st.lock().unwrap().connections, 1, "a second connection was opened");
    // With nothing waiting, request 1 is answered and request 2 opens.
    cx.push(json!({ "type": "active", "activeFlags": [] }));
    cx.push(asks);
    std::thread::sleep(Duration::from_millis(3500));
    let out = run(&["--timeout", "8s", "--acknowledge", &first, &t]);
    assert_eq!(
        out.status.code(),
        Some(5),
        "request 2 went unseen: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let v = output_json(&out);
    assert!(
        v["threads"][0]["pendingRequestIds"].to_string().contains("approval-2"),
        "{v}"
    );
    assert_read_only(&cx);
}

#[test]
fn a_drain_batch_that_ends_one_turn_and_starts_the_next_keeps_the_connection() {
    // Review ordinal 16 (P2): any status other than `active` in a drain
    // batch closed the kept connection, even when the batch went on into
    // the next turn. The newest status decides.
    let env = Env::new();
    let cx = MockCodex::start(&env);
    let t = target();
    let run = waits_with_drain(&env, &cx, "3000");
    let asks = json!({ "type": "active", "activeFlags": ["waitingOnApproval"] });
    cx.set(asks.clone(), &[("turn-1", "inProgress")]);
    let out = run(&["--timeout", "5s", &t]);
    assert_eq!(out.status.code(), Some(5));
    let first = output_json(&out)["threads"][0]["pendingKey"]
        .as_str()
        .unwrap()
        .to_string();
    env.wait_status("the connection kept", Duration::from_secs(10), |s| s["codexKept"] == 1);
    // Within one 3 s round: turn 1 ends, turn 2 starts and asks.
    cx.push(json!({ "type": "idle" }));
    cx.with(|c| {
        c.turns = vec![
            json!({ "id": "turn-1", "status": "completed" }),
            json!({ "id": "turn-2", "status": "inProgress" }),
        ]
    });
    cx.push(asks);
    std::thread::sleep(Duration::from_millis(4000));
    let s = env.status();
    assert_eq!(s["codexKept"], 1, "the connection was closed at the new turn: {s}");
    let out = run(&["--timeout", "8s", "--acknowledge", &first, &t]);
    assert_eq!(out.status.code(), Some(5), "{}", String::from_utf8_lossy(&out.stdout));
    let v = output_json(&out);
    assert!(
        v["threads"][0]["pendingRequestIds"]
            .to_string()
            .contains("turn-2-approval-1"),
        "{v}"
    );
    assert_read_only(&cx);
}

#[test]
fn a_turn_list_that_stays_unsupported_fails_after_its_retry() {
    // Review ordinal 16 (P2): every "not supported yet" was retried until
    // the wait's own timeout. Only codex-cli 0.166.0's materializing answer
    // is retried, for 30 s; then the wait fails (exit 2).
    let env = Env::new();
    let cx = MockCodex::start(&env);
    let t = target();
    cx.with(|c| c.unsupported = usize::MAX);
    cx.set(
        json!({ "type": "active", "activeFlags": [] }),
        &[("turn-1", "inProgress")],
    );
    let started = std::time::Instant::now();
    let out = wait(&env, &cx, &["--no-daemon", "--timeout", "150s", &t])
        .output()
        .unwrap();
    let took = started.elapsed();
    let v = output_json(&out);
    assert_eq!(out.status.code(), Some(2), "{v}");
    assert!(v.to_string().contains("list_turns is not supported yet"), "{v}");
    assert!(
        took >= Duration::from_secs(29) && took < Duration::from_secs(120),
        "{took:?}"
    );
}

#[test]
fn any_other_not_supported_answer_fails_at_once() {
    let env = Env::new();
    let cx = MockCodex::start(&env);
    let t = target();
    cx.with(|c| {
        c.unsupported = usize::MAX;
        c.unsupported_message = Some("thread/turns/list is not supported yet for remote threads".into());
    });
    cx.set(
        json!({ "type": "active", "activeFlags": [] }),
        &[("turn-1", "inProgress")],
    );
    let started = std::time::Instant::now();
    let out = wait(&env, &cx, &["--no-daemon", "--timeout", "60s", &t])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(started.elapsed() < Duration::from_secs(15));
}
