//! The multiplexer daemon and its thin client.
//!
//! Protocol: newline-delimited JSON over a Unix socket. The client sends one
//! request line. For `wait` the daemon answers `{"event":"accepted"}` at
//! once and the final outcome line when the wait ends. The socket lives in a
//! 0700 directory and is itself 0600, so only the owning user can connect.

use crate::config;
use crate::engine::{error_json, Hub, Outcome, SubMsg, WaitRequest};
use crate::util;
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
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

fn log(msg: &str) {
    eprintln!("[{}] t3-watch daemon {}: {msg}", util::now_ms(), std::process::id());
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

/// Creates the socket's directory. Directories t3-watch owns become 0700; a
/// directory chosen with `T3_WATCH_SOCKET` (say `/tmp`) is created if missing
/// but its permissions are left alone.
fn prepare_socket_dir(sock: &Path) -> Result<(), String> {
    let dir = socket_dir(sock);
    let res = if std::env::var_os("T3_WATCH_SOCKET").is_some_and(|v| !v.is_empty()) {
        fs::create_dir_all(&dir)
    } else {
        util::ensure_private_dir(&dir)
    };
    res.map_err(|e| format!("cannot create {}: {e}", dir.display()))
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
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(Some(f)),
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline && UnixStream::connect(sock).is_err() => {
                thread::sleep(Duration::from_millis(20))
            }
            Err(fs::TryLockError::WouldBlock) => return Ok(None),
            Err(fs::TryLockError::Error(e)) => return Err(e),
        }
    }
}

pub fn run(idle_exit: Duration) -> Result<(), String> {
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

    // We hold the lock, so any socket file left behind is stale.
    let _ = fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).map_err(|e| format!("cannot bind {}: {e}", sock.display()))?;
    fs::set_permissions(&sock, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    log(&format!("listening on {} (idle exit {:?})", sock.display(), idle_exit));

    let hub = Hub::new("daemon");
    let connections = Arc::new(AtomicUsize::new(0));
    {
        let (hub, connections, sock) = (hub.clone(), connections.clone(), sock.clone());
        thread::Builder::new()
            .name("idle".into())
            .stack_size(64 * 1024)
            .spawn(move || loop {
                thread::sleep((idle_exit / 4).clamp(Duration::from_millis(50), Duration::from_secs(1)));
                if connections.load(Ordering::SeqCst) == 0 && hub.idle_for().is_some_and(|d| d >= idle_exit) {
                    log("idle; exiting");
                    shutdown(&sock);
                }
            })
            .map_err(|e| e.to_string())?;
    }

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                connections.fetch_add(1, Ordering::SeqCst);
                let (hub, open, sock) = (hub.clone(), connections.clone(), sock.clone());
                let spawned = thread::Builder::new()
                    .name("conn".into())
                    .stack_size(256 * 1024)
                    .spawn(move || {
                        if let Err(e) = handle(&hub, stream, &sock) {
                            log(&format!("connection error: {e}"));
                        }
                        open.fetch_sub(1, Ordering::SeqCst);
                    });
                if spawned.is_err() {
                    log("cannot spawn connection thread");
                    connections.fetch_sub(1, Ordering::SeqCst);
                }
            }
            Err(e) => {
                log(&format!("accept failed: {e}"));
                thread::sleep(Duration::from_millis(50));
            }
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

fn handle(hub: &Arc<Hub>, stream: UnixStream, sock: &Path) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    (&mut reader).take(1 << 20).read_line(&mut line)?;
    let req: Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(e) => return write_line(&stream, &error_json("protocol", &format!("bad request: {e}"))),
    };
    match req.get("op").and_then(Value::as_str).unwrap_or("") {
        "ping" => write_line(&stream, &json!({ "ok": true, "pid": std::process::id() })),
        "status" => write_line(&stream, &hub.status_json()),
        "stop" => {
            write_line(&stream, &json!({ "ok": true, "pid": std::process::id() }))?;
            log("stop requested");
            shutdown(sock)
        }
        "wait" => {
            let wreq = match WaitRequest::from_json(&req) {
                Ok(r) => r,
                Err(e) => return write_line(&stream, &error_json("usage", &e)),
            };
            write_line(&stream, &json!({ "event": "accepted", "pid": std::process::id() }))?;
            stream.set_read_timeout(None)?;
            let (tx, rx) = mpsc::channel();
            // Blocks in read() until the client hangs up: no polling.
            let gone = tx.clone();
            thread::Builder::new()
                .name("hangup".into())
                .stack_size(32 * 1024)
                .spawn(move || {
                    let mut buf = [0u8; 64];
                    while let Ok(n) = reader.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                    }
                    let _ = gone.send(SubMsg::ClientGone);
                })?;
            let outcome = hub.wait(&wreq, tx, rx);
            if let Some(v) = outcome.json() {
                let _ = write_line(&stream, v);
            }
            let _ = stream.shutdown(std::net::Shutdown::Both);
            Ok(())
        }
        op => write_line(&stream, &error_json("usage", &format!("unknown op {op:?}"))),
    }
}

// ---- client side ---------------------------------------------------------

fn spawn_daemon() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find own executable: {e}"))?;
    let state = config::state_dir();
    util::ensure_private_dir(&state).map_err(|e| format!("cannot create {}: {e}", state.display()))?;
    prepare_socket_dir(&config::socket_path())?;
    let log_path = state.join("daemon.log");
    if fs::metadata(&log_path).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = fs::remove_file(&log_path);
    }
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)
        .map_err(|e| format!("cannot open {}: {e}", log_path.display()))?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon")
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

/// Connects to the daemon, starting it first if nothing is listening.
pub fn connect_or_spawn() -> Result<UnixStream, String> {
    let sock = config::socket_path();
    check_socket_path(&sock)?;
    if let Ok(s) = UnixStream::connect(&sock) {
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
        if let Ok(s) = UnixStream::connect(&sock) {
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

/// Sends one request to a running daemon without starting one.
pub fn request(op: &str) -> Result<Option<Value>, String> {
    let sock = config::socket_path();
    let Ok(stream) = UnixStream::connect(&sock) else {
        return Ok(None);
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    write_line(&stream, &json!({ "op": op })).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(&stream)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    serde_json::from_str(line.trim()).map(Some).map_err(|e| e.to_string())
}

/// Reads the daemon's answer to a wait. `None` means the connection dropped
/// before an outcome arrived.
fn read_outcome(stream: &UnixStream, started: Instant) -> Option<Outcome> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return None,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                return Some(Outcome::Timeout(json!({
                    "event": "timeout",
                    "elapsedMs": started.elapsed().as_millis() as u64,
                    "via": "daemon",
                    "threads": [],
                    "lastError": "the daemon did not answer before the timeout",
                })));
            }
            Err(_) => return None,
            Ok(_) => {}
        }
        let v = serde_json::from_str::<Value>(line.trim()).ok()?;
        if v.get("event").and_then(Value::as_str) != Some("accepted") {
            return Some(Outcome::from_json(v));
        }
    }
}

/// Runs a wait through the daemon. If the daemon dies mid-wait (crash,
/// upgrade, idle-exit race) the client reconnects and re-registers with the
/// remaining time.
pub fn wait(req: &WaitRequest) -> Result<Outcome, String> {
    let started = Instant::now();
    let mut reconnects = 0;
    loop {
        let stream = connect_or_spawn()?;
        let mut r = req.clone();
        r.timeout = req.timeout.saturating_sub(started.elapsed());
        // The daemon enforces the timeout; this bound only matters if the
        // daemon itself stops answering.
        let _ = stream.set_read_timeout(Some(r.timeout + Duration::from_secs(30)));
        // A failed write means the daemon exited after we connected (idle
        // exit or `stop`): reconnect like any other dropped connection.
        if write_line(&stream, &r.to_json()).is_ok() {
            if let Some(outcome) = read_outcome(&stream, started) {
                return Ok(outcome);
            }
        }
        reconnects += 1;
        if reconnects > 5 {
            return Err("daemon kept closing the connection".into());
        }
        thread::sleep(Duration::from_millis(50 * reconnects));
    }
}
