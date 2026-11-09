//! Native Codex threads (experimental): `wait codex://threads/<id>` reads
//! the thread from a Codex app-server over its WebSocket-on-Unix-socket
//! transport. The app-server here is a mock that speaks the documented
//! protocol; it records every method a client sends, so the tests can show
//! that t3-watch only reads and never answers a server request.

mod common;

use common::*;
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const T: &str = "01a1ba73-b8d4-7aa3-b523-643d92150f60";
const READS: &[&str] = &["initialize", "initialized", "thread/read", "thread/turns/list"];

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
    s.write_all(&f)
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
                    "id": T, "status": g.status, "preview": "PREVIEW_CANARY first message" } } }),
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
    // Every connection serves one poll (initialize, read, list), then closes.
    cx.with(|c| c.drop_after = Some(3));
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
fn a_request_that_opens_and_closes_while_nothing_counts_is_never_seen() {
    // The limit, stated as a test: the app-server keeps no record of past
    // requests, so one answered before any t3-watch connection counts again
    // leaves no trace. (It needs nobody: its owner answered it.)
    let env = Env::new();
    let cx = MockCodex::start(&env);
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
    assert!(v["threads"][0]["pendingKey"].is_null(), "{v}");
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
