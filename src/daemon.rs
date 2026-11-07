//! The multiplexer daemon and its thin client.
//!
//! Protocol: newline-delimited JSON over a Unix socket. The client sends one
//! request line. For `wait` the daemon answers `{"event":"accepted"}` at
//! once, any progress lines (`"progress": true`) as they happen, and the
//! final outcome line when the wait ends. Every line must end in `\n` and is
//! size-bounded; every read and write has a timeout except the long wait
//! itself.
//!
//! Both sides check the engine revision (`version::ENGINE`) before a wait
//! means anything: the daemon refuses a request from another revision, and
//! the client refuses an `accepted` line that does not name its own. A
//! daemon from a build with other result semantics is therefore never
//! trusted, even on a custom socket shared by both.
//!
//! The daemon also runs the durable-notification sender (`outbox`) and does
//! not idle-exit while the outbox holds entries.
//!
//! Access control does not rely on file modes alone: the socket's directory
//! and the socket must belong to us and be closed to group and others, lock
//! files are opened without following symlinks, and both ends check the
//! other's uid with `getpeereid`/`SO_PEERCRED`.

use crate::config;
use crate::engine::{error_json, Hub, Outcome, SubMsg, WaitRequest};
use crate::outbox;
use crate::trace;
use crate::util;
use crate::version;
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

pub const DEFAULT_IDLE_EXIT: Duration = Duration::from_secs(60);
/// sockaddr_un.sun_path is 104 bytes on macOS and 108 on Linux.
const MAX_SOCKET_PATH: usize = 103;
const MAX_REQUEST_LINE: usize = 256 * 1024;
const MAX_RESPONSE_LINE: usize = 16 << 20;
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MAX_CONNECTIONS: usize = 512;
/// Extra connections allowed beyond the wait limit, so `status` and `stop`
/// still get through when every wait slot is taken.
const CONTROL_HEADROOM: usize = 64;

fn log(msg: &str) {
    util::warn(&format!(
        "[{}] t3-watch daemon {}: {msg}",
        util::now_ms(),
        std::process::id()
    ));
}

fn check_socket_path(sock: &Path) -> Result<(), String> {
    if sock.as_os_str().len() > MAX_SOCKET_PATH {
        return Err(format!(
            "socket path is too long for a Unix socket ({} bytes): {}; set T3_WATCH_SOCKET to a shorter path",
            sock.as_os_str().len(),
            sock.display()
        ));
    }
    Ok(())
}

fn socket_dir(sock: &Path) -> PathBuf {
    sock.parent().map(Path::to_path_buf).unwrap_or_else(config::state_dir)
}

/// Creates the socket's directory. Directories t3-watch owns become 0700. A
/// directory chosen with `T3_WATCH_SOCKET` is created 0700 if missing and
/// otherwise must already be a private directory of ours; its permissions
/// are never changed.
fn prepare_socket_dir(sock: &Path) -> Result<(), String> {
    let dir = socket_dir(sock);
    if config::custom_socket() {
        util::verify_private_dir(&dir)
    } else {
        util::ensure_private_dir(&dir).map_err(|e| format!("cannot prepare {}: {e}", dir.display()))
    }
}

/// Checks the socket file and its directory before trusting them: both must
/// be ours and closed to everyone else. `Ok(false)` means there is no socket.
fn check_socket(sock: &Path) -> Result<bool, String> {
    let meta = match fs::symlink_metadata(sock) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("cannot inspect {}: {e}", sock.display())),
    };
    if !meta.file_type().is_socket() {
        return Err(format!("{} exists but is not a socket", sock.display()));
    }
    util::check_private(&meta, sock)?;
    let dir = socket_dir(sock);
    let dir_meta = fs::symlink_metadata(&dir).map_err(|e| format!("cannot inspect {}: {e}", dir.display()))?;
    if !dir_meta.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    util::check_private(&dir_meta, &dir)?;
    Ok(true)
}

/// The single-daemon lock sits next to the socket it guards.
fn lock_path(sock: &Path) -> PathBuf {
    let mut p = sock.as_os_str().to_owned();
    p.push(".lock");
    PathBuf::from(p)
}

/// Takes the single-daemon lock. If another daemon holds it and answers on
/// the socket, gives up at once; otherwise waits briefly in case that daemon
/// is in the middle of its idle shutdown.
fn take_lock(path: &Path, sock: &Path) -> io::Result<Option<File>> {
    let f = util::open_lock_file(path)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if util::try_flock(&f)? {
            return Ok(Some(f));
        }
        if Instant::now() >= deadline || UnixStream::connect(sock).is_ok() {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn max_connections() -> usize {
    std::env::var("T3_WATCH_MAX_CONNECTIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or(DEFAULT_MAX_CONNECTIONS)
}

pub fn run(idle_exit: Duration) -> Result<(), String> {
    // Everything this process creates (socket, lock, log) starts private.
    // Without this, `bind` makes the socket with the default umask and a
    // client could see it open to others before the chmod below.
    unsafe { libc::umask(0o077) };
    let sock = config::socket_path();
    check_socket_path(&sock)?;
    prepare_socket_dir(&sock)?;
    let lock_path = lock_path(&sock);
    let Some(mut lock_file) =
        take_lock(&lock_path, &sock).map_err(|e| format!("cannot lock {}: {e}", lock_path.display()))?
    else {
        log("another daemon holds the lock; exiting");
        return Ok(());
    };
    let _ = lock_file.set_len(0);
    let _ = writeln!(lock_file, "{}", std::process::id());

    // We hold the lock, so a socket left behind is stale. Anything else at
    // that path is not ours to delete.
    if check_socket(&sock)? {
        fs::remove_file(&sock).map_err(|e| format!("cannot remove stale {}: {e}", sock.display()))?;
    }
    let listener = UnixListener::bind(&sock).map_err(|e| format!("cannot bind {}: {e}", sock.display()))?;
    fs::set_permissions(&sock, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    check_socket(&sock)?;
    // Paths arrive absolute from clients; do not pin the caller's directory.
    let _ = std::env::set_current_dir("/");
    let limit = max_connections();
    let waits = Arc::new(AtomicUsize::new(0));
    log(&format!(
        "listening on {} (idle exit {:?}, max {limit} waiting clients; {})",
        sock.display(),
        idle_exit,
        version::line()
    ));
    trace::record("daemon.start", json!({ "socket": sock.display().to_string() }));

    let hub = Hub::new("daemon");
    let connections = Arc::new(AtomicUsize::new(0));
    {
        let (hub, connections, sock) = (hub.clone(), connections.clone(), sock.clone());
        util::spawn_thread("idle", 64 * 1024, move || loop {
            thread::sleep((idle_exit / 4).clamp(Duration::from_millis(50), Duration::from_secs(1)));
            if connections.load(Ordering::SeqCst) == 0
                && hub.idle_for().is_some_and(|d| d >= idle_exit)
                && outbox::pending_count() == 0
            {
                log("idle; exiting");
                shutdown(&sock);
            }
        })
        .map_err(|e| format!("cannot start idle thread: {e}"))?;
    }
    util::spawn_thread("outbox", 256 * 1024, || outbox::run_forever(&log))
        .map_err(|e| format!("cannot start the outbox sender: {e}"))?;

    for conn in listener.incoming() {
        let stream = match conn {
            Ok(s) => s,
            Err(e) => {
                log(&format!("accept failed: {e}"));
                thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        if let Err(e) = util::check_peer(&stream) {
            log(&format!("rejected connection: {e}"));
            continue;
        }
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        // Waits are limited in `handle`; this hard cap only bounds threads.
        if connections.load(Ordering::SeqCst) >= limit + CONTROL_HEADROOM {
            let _ = write_line(
                &stream,
                &error_json("limit", "the daemon has too many open connections"),
            );
            continue;
        }
        connections.fetch_add(1, Ordering::SeqCst);
        let (hub, open, sock) = (hub.clone(), connections.clone(), sock.clone());
        let slots = WaitSlots {
            used: waits.clone(),
            max: limit,
        };
        let spawned = util::spawn_thread("conn", 256 * 1024, move || {
            if let Err(e) = handle(&hub, stream, &sock, &slots) {
                log(&format!("connection error: {e}"));
            }
            open.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            log("cannot spawn connection thread");
            connections.fetch_sub(1, Ordering::SeqCst);
        }
    }
    Ok(())
}

fn shutdown(sock: &Path) -> ! {
    let _ = fs::remove_file(sock);
    std::process::exit(0)
}

fn write_line(mut w: &UnixStream, v: &Value) -> io::Result<()> {
    let mut s = v.to_string();
    s.push('\n');
    w.write_all(s.as_bytes())?;
    w.flush()
}

#[derive(Debug)]
enum LineError {
    TooLong,
    /// The peer closed the stream before sending `\n`.
    Incomplete,
    Io(io::Error),
}

/// Reads one `\n`-terminated line of at most `max` bytes (newline excluded).
fn read_line_bounded(r: &mut impl BufRead, max: usize) -> Result<Vec<u8>, LineError> {
    read_line_resumable(r, max, &mut Vec::new())
}

/// `read_line_bounded` that keeps what it read in `out` when the read times
/// out, so the caller can check a deadline and carry on with the same line.
fn read_line_resumable(r: &mut impl BufRead, max: usize, out: &mut Vec<u8>) -> Result<Vec<u8>, LineError> {
    loop {
        let buf = match r.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(LineError::Io(e)),
        };
        if buf.is_empty() {
            return Err(LineError::Incomplete);
        }
        let (take, done) = match buf.iter().position(|b| *b == b'\n') {
            Some(i) => (i, true),
            None => (buf.len(), false),
        };
        if out.len() + take > max {
            return Err(LineError::TooLong);
        }
        out.extend_from_slice(&buf[..take]);
        r.consume(if done { take + 1 } else { take });
        if done {
            return Ok(std::mem::take(out));
        }
    }
}

/// Counts waiting clients against `T3_WATCH_MAX_CONNECTIONS`. Control
/// requests (`status`, `stop`, `ping`) do not take a slot.
struct WaitSlots {
    used: Arc<AtomicUsize>,
    max: usize,
}

/// Releases a wait slot when the wait ends, however it ends.
struct Slot<'a>(&'a AtomicUsize);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl WaitSlots {
    fn take(&self) -> Option<Slot<'_>> {
        if self.used.fetch_add(1, Ordering::SeqCst) >= self.max {
            self.used.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Slot(&self.used))
    }
}

fn handle(hub: &Arc<Hub>, stream: UnixStream, sock: &Path, slots: &WaitSlots) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let line = match read_line_bounded(&mut reader, MAX_REQUEST_LINE) {
        Ok(l) => l,
        Err(LineError::TooLong) => {
            return write_line(
                &stream,
                &error_json("protocol", &format!("request longer than {MAX_REQUEST_LINE} bytes")),
            )
        }
        Err(LineError::Incomplete) => {
            return write_line(&stream, &error_json("protocol", "request must end with a newline"))
        }
        Err(LineError::Io(e)) => return Err(e),
    };
    let req: Value = match serde_json::from_slice(&line) {
        Ok(v) => v,
        Err(e) => return write_line(&stream, &error_json("protocol", &format!("bad request: {e}"))),
    };
    match req.get("op").and_then(Value::as_str).unwrap_or("") {
        "ping" => write_line(
            &stream,
            &json!({ "ok": true, "pid": std::process::id(), "build": version::json() }),
        ),
        "status" => {
            let mut v = hub.status_json();
            v["build"] = version::json();
            v["outbox"] = outbox::summary_json();
            write_line(&stream, &v)
        }
        "stop" => {
            write_line(&stream, &json!({ "ok": true, "pid": std::process::id() }))?;
            log("stop requested");
            shutdown(sock)
        }
        "wait" => {
            let engine = req.get("engine").and_then(Value::as_u64);
            if engine != Some(u64::from(version::ENGINE)) {
                return write_line(
                    &stream,
                    &error_json(
                        "incompatible_client",
                        &format!(
                            "this daemon runs engine {} ({}); the client sent engine {}, so its \
                             request may mean something else here. Use a t3-watch of the same engine \
                             revision on this socket",
                            version::ENGINE,
                            version::line(),
                            engine.map_or_else(|| "none".to_string(), |e| e.to_string())
                        ),
                    ),
                );
            }
            let Some(_slot) = slots.take() else {
                return write_line(
                    &stream,
                    &error_json(
                        "limit",
                        &format!("the daemon already serves {} waiting clients, its limit", slots.max),
                    ),
                );
            };
            let wreq = match WaitRequest::from_json(&req) {
                Ok(r) => r,
                Err(e) => return write_line(&stream, &error_json("usage", &e)),
            };
            let (tx, rx) = mpsc::channel();
            // Blocks in read() until the client hangs up: no polling. The
            // read timeout is per socket, so clear it for this reader.
            stream.set_read_timeout(None)?;
            let gone = tx.clone();
            let hangup = util::spawn_thread("hangup", 32 * 1024, move || {
                let mut buf = [0u8; 64];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                }
                let _ = gone.send(SubMsg::ClientGone);
            });
            if let Err(e) = hangup {
                return write_line(
                    &stream,
                    &error_json("resource", &format!("cannot start a connection thread: {e}")),
                );
            }
            write_line(
                &stream,
                &json!({
                    "event": "accepted",
                    "pid": std::process::id(),
                    "version": version::VERSION,
                    "engine": version::ENGINE,
                    "rev": version::REV,
                    "capabilities": version::CAPABILITIES,
                }),
            )?;
            let outcome = hub.wait(&wreq, tx, rx, &mut |p| {
                let _ = write_line(&stream, &p);
            });
            if let Some(v) = outcome.json() {
                let _ = write_line(&stream, v);
            }
            let _ = stream.shutdown(std::net::Shutdown::Both);
            Ok(())
        }
        op => write_line(
            &stream,
            &error_json("usage", &format!("unknown op {:?}", util::sanitize_text(op, 40))),
        ),
    }
}

// ---- client side ---------------------------------------------------------

fn spawn_daemon() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find own executable: {e}"))?;
    let state = config::state_dir();
    util::ensure_private_dir(&state).map_err(|e| format!("cannot create {}: {e}", state.display()))?;
    prepare_socket_dir(&config::socket_path())?;
    let log_path = state.join("daemon.log");
    if fs::symlink_metadata(&log_path).is_ok_and(|m| m.is_file() && m.len() > 1 << 20) {
        let _ = fs::remove_file(&log_path);
    }
    // The daemon's stderr. Without a usable log file it runs without one:
    // a log must never keep the daemon (and the waiting client) from
    // starting.
    let log_file = util::open_log(&log_path)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());
    let mut cmd = Command::new(exe);
    cmd.arg("daemon")
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log_file);
    // A new session detaches the daemon from the caller's terminal and
    // process group, so killing the waiting client never kills the daemon.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().map(drop).map_err(|e| format!("cannot start daemon: {e}"))
}

/// Connects only to a socket that passes `check_socket`, and only to a
/// daemon running as our uid. `Ok(None)` means nothing is listening.
fn connect(sock: &Path) -> Result<Option<UnixStream>, String> {
    if !check_socket(sock)? {
        return Ok(None);
    }
    let Ok(s) = UnixStream::connect(sock) else {
        return Ok(None);
    };
    util::check_peer(&s).map_err(|e| format!("refusing daemon at {}: {e}", sock.display()))?;
    let _ = s.set_write_timeout(Some(IO_TIMEOUT));
    let _ = s.set_read_timeout(Some(IO_TIMEOUT));
    Ok(Some(s))
}

/// Connects to the daemon, starting it first if nothing is listening.
pub fn connect_or_spawn() -> Result<UnixStream, String> {
    let sock = config::socket_path();
    check_socket_path(&sock)?;
    prepare_socket_dir(&sock)?;
    if let Some(s) = connect(&sock)? {
        return Ok(s);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_spawn: Option<Instant> = None;
    let mut pause = Duration::from_millis(2);
    loop {
        if last_spawn.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
            spawn_daemon()?;
            last_spawn = Some(Instant::now());
        }
        thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(50));
        if let Some(s) = connect(&sock)? {
            return Ok(s);
        }
        if Instant::now() > deadline {
            return Err(format!(
                "daemon did not start; see {}",
                config::state_dir().join("daemon.log").display()
            ));
        }
    }
}

fn read_response(stream: &UnixStream) -> Result<Value, String> {
    let mut reader = BufReader::new(stream);
    let line = read_line_bounded(&mut reader, MAX_RESPONSE_LINE).map_err(|e| format!("daemon reply: {e:?}"))?;
    serde_json::from_slice(&line).map_err(|e| format!("daemon reply is not JSON: {e}"))
}

/// Sends one request to a running daemon without starting one.
pub fn request(op: &str) -> Result<Option<Value>, String> {
    let sock = config::socket_path();
    let Some(stream) = connect(&sock)? else {
        return Ok(None);
    };
    write_line(&stream, &json!({ "op": op })).map_err(|e| e.to_string())?;
    read_response(&stream).map(Some)
}

/// What the client learned about the daemon that served a wait.
pub struct DaemonInfo {
    pub reconnects: u32,
    /// The `accepted` line's pid, version, engine and rev.
    pub build: Option<Value>,
}

/// Why a daemon's answer cannot be trusted by this client, if it cannot:
/// only an `accepted` line naming this engine revision makes what follows
/// mean what this build means.
fn incompatible(accepted: Option<&Value>, first: &Value, sock: &Path) -> Option<String> {
    let engine = match accepted {
        Some(a) => a.get("engine").and_then(Value::as_u64),
        // An error before `accepted` is safe to pass on (a limit, a usage
        // error, or a newer daemon refusing us); anything else is not.
        None if first.get("event").and_then(Value::as_str) == Some("error") => return None,
        None => None,
    };
    let missing: Vec<&str> = version::CAPABILITIES
        .iter()
        .copied()
        .filter(|c| {
            !accepted
                .and_then(|a| a.get("capabilities"))
                .and_then(Value::as_array)
                .is_some_and(|caps| caps.iter().any(|x| x == c))
        })
        .collect();
    if engine == Some(u64::from(version::ENGINE)) && missing.is_empty() {
        return None;
    }
    let who = accepted.map_or_else(
        || "an unidentified daemon".to_string(),
        |a| {
            format!(
                "t3-watch {} (engine {}, rev {}, pid {})",
                a.get("version").and_then(Value::as_str).unwrap_or("?"),
                a.get("engine").map_or_else(|| "none".to_string(), |e| e.to_string()),
                a.get("rev").and_then(Value::as_str).unwrap_or("?"),
                a.get("pid").map_or_else(|| "?".to_string(), |p| p.to_string()),
            )
        },
    );
    let lacking = if missing.is_empty() {
        String::new()
    } else {
        format!(" (it lacks {})", missing.join(", "))
    };
    Some(format!(
        "the daemon at {} is {who}{lacking}, but this client is {}: its results may mean \
         something else, so none is trusted. Stop that daemon with its own t3-watch \
         (`t3-watch stop`) or let it idle out, or point T3_WATCH_SOCKET elsewhere",
        sock.display(),
        version::line()
    ))
}

/// How long past the wait's own deadline the client gives the daemon to
/// answer (it times the wait out itself at the same moment).
const ANSWER_GRACE_MS: u64 = 30_000;

/// The answer to a wait whose time ran out on the client's side: before a
/// (re)registration, or while the daemon did not answer.
fn client_timeout(sw: &util::Stopwatch, why: &str) -> Outcome {
    Outcome::Timeout(json!({
        "event": "timeout",
        "outcome": "timeout",
        "elapsedMs": sw.elapsed_ms(),
        "via": "daemon",
        "threads": [],
        "lastError": why,
    }))
}

/// Reads the daemon's answer to a wait, handing progress lines to
/// `progress`. `None` means the connection dropped before an outcome
/// arrived. An answer from a daemon of another engine revision becomes an
/// `incompatible_daemon` error, and the connection is dropped, which
/// cancels that daemon's wait. Reads time out every second so the wait's
/// suspend-inclusive deadline is checked even while the daemon is silent.
fn read_outcome(
    stream: &UnixStream,
    sw: &util::Stopwatch,
    deadline_ms: u64,
    sock: &Path,
    build: &mut Option<Value>,
    progress: &mut dyn FnMut(Value),
) -> Option<Outcome> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let mut reader = BufReader::new(stream);
    let mut accepted: Option<Value> = None;
    let mut partial = Vec::new();
    loop {
        let line = match read_line_resumable(&mut reader, MAX_RESPONSE_LINE, &mut partial) {
            Ok(l) => l,
            Err(LineError::Io(e)) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                if sw.elapsed_ms() >= deadline_ms + ANSWER_GRACE_MS {
                    return Some(client_timeout(sw, "the daemon did not answer before the timeout"));
                }
                continue;
            }
            Err(LineError::TooLong) => return Some(Outcome::Error(error_json("protocol", "daemon reply is too long"))),
            Err(_) => return None,
        };
        let v = serde_json::from_slice::<Value>(&line).ok()?;
        let is_accepted = v.get("event").and_then(Value::as_str) == Some("accepted");
        if accepted.is_none() {
            let candidate = is_accepted.then_some(&v);
            if let Some(why) = incompatible(candidate, &v, sock) {
                trace::record("daemon.incompatible", json!({ "daemon": candidate, "reason": why }));
                return Some(Outcome::Error(error_json("incompatible_daemon", &why)));
            }
            if is_accepted {
                trace::record("daemon.accepted", json!({ "daemon": v }));
                *build = Some(v.clone());
                accepted = Some(v);
                continue;
            }
        }
        if v.get("progress").and_then(Value::as_bool) == Some(true) {
            progress(v);
        } else if !is_accepted {
            return Some(Outcome::from_json(v));
        }
    }
}

/// Runs a wait through the daemon and returns the outcome with what the
/// client learned about the daemon. The wait has one deadline on the
/// suspend-inclusive clock, kept across reconnects: if the daemon dies
/// mid-wait (crash, upgrade, idle-exit race) the client reconnects and
/// re-registers with what is left of it, and times out at once when nothing
/// is. The daemon's `elapsedMs` counts from the last registration only.
pub fn wait(req: &WaitRequest, progress: &mut dyn FnMut(Value)) -> Result<(Outcome, DaemonInfo), String> {
    let sw = util::Stopwatch::start();
    let deadline_ms = req.timeout.as_millis() as u64;
    let sock = config::socket_path();
    let mut info = DaemonInfo {
        reconnects: 0,
        build: None,
    };
    loop {
        let remaining = deadline_ms.saturating_sub(sw.elapsed_ms());
        if remaining == 0 {
            trace::record(
                "daemon.expired",
                json!({ "reconnects": info.reconnects, "elapsedMs": sw.elapsed_ms() }),
            );
            return Ok((
                client_timeout(
                    &sw,
                    "the wait's time ran out (suspended time included) before it could be registered again",
                ),
                info,
            ));
        }
        let stream = connect_or_spawn()?;
        let mut r = req.clone();
        r.timeout = Duration::from_millis(deadline_ms.saturating_sub(sw.elapsed_ms()).max(1));
        // Read even if the write failed: a daemon at its connection limit
        // answers with an error and closes before our request lands. With no
        // answer at all, the daemon exited after we connected (idle exit or
        // `stop`): reconnect like any other dropped connection.
        let _ = write_line(&stream, &r.to_json());
        if let Some(outcome) = read_outcome(&stream, &sw, deadline_ms, &sock, &mut info.build, progress) {
            return Ok((outcome, info));
        }
        info.reconnects += 1;
        trace::record("daemon.reconnect", json!({ "reconnects": info.reconnects }));
        if info.reconnects > 5 {
            return Err("daemon kept closing the connection".into());
        }
        thread::sleep(Duration::from_millis(50 * u64::from(info.reconnects)));
    }
}

/// Makes sure a daemon is running (it sends queued notifications).
pub fn ensure_running() -> Result<(), String> {
    connect_or_spawn().map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_must_end_in_newline_and_fit() {
        let mut ok = BufReader::new(&b"{\"op\":\"ping\"}\nrest"[..]);
        assert_eq!(read_line_bounded(&mut ok, 64).unwrap(), b"{\"op\":\"ping\"}");
        let mut unterminated = BufReader::new(&b"{\"op\":\"ping\"}"[..]);
        assert!(matches!(
            read_line_bounded(&mut unterminated, 64),
            Err(LineError::Incomplete)
        ));
        let mut empty = BufReader::new(&b""[..]);
        assert!(matches!(read_line_bounded(&mut empty, 64), Err(LineError::Incomplete)));
        let long = vec![b'x'; 1000];
        let mut too_long = BufReader::with_capacity(16, &long[..]);
        assert!(matches!(read_line_bounded(&mut too_long, 100), Err(LineError::TooLong)));
        // Exactly at the limit is fine, across several buffer refills.
        let mut exact = vec![b'y'; 100];
        exact.push(b'\n');
        let mut r = BufReader::with_capacity(7, &exact[..]);
        assert_eq!(read_line_bounded(&mut r, 100).unwrap().len(), 100);
    }

    #[test]
    fn lock_path_sits_next_to_the_socket() {
        assert_eq!(
            lock_path(Path::new("/a/b/daemon.sock")),
            Path::new("/a/b/daemon.sock.lock")
        );
    }
}
