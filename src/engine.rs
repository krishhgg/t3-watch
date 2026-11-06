//! The watch engine shared by the daemon and `--no-daemon` mode.
//!
//! A `Hub` owns one `Watch` per key: (origin, pairing, thread, environment,
//! delegated) for MCP and (data dir, `t3` binary, thread) for the local CLI.
//! Each watch has one worker thread that talks upstream; every waiting client
//! subscribes to the watches it needs and evaluates its own condition
//! (`mode`, `expect_min_run_ordinal`) against their shared observations. So
//! 20 clients on one thread cost one upstream long-poll, not 20.

use crate::config::{self, Config, Machine};
use crate::local;
use crate::mcp::{CallError, McpClient, TokenRecovery};
use crate::model::{classify, run_covered, Predicate, StatusClass, ThreadState};
use crate::oauth;
use crate::util::{self, lock, Jitter};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const LOCAL_MACHINE: &str = "local";
pub const MIN_INTERVAL: Duration = Duration::from_millis(50);
pub const MIN_SLICE: Duration = Duration::from_millis(100);
/// Longest single `t3_thread_wait`. Longer slices only tie up a connection
/// and outlive most proxies.
pub const MAX_SLICE: Duration = Duration::from_secs(600);
const MAX_TARGETS: usize = 256;
const DEFAULT_MAX_WATCHES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Transport {
    Mcp,
    Local,
}

/// One thread to watch, as the client resolved it.
#[derive(Debug, Clone)]
pub struct TargetSpec {
    pub transport: Transport,
    /// Saved machine name for MCP; `local` for the local CLI.
    pub machine: String,
    pub thread_id: String,
    pub environment_id: Option<String>,
    /// Absolute path of an explicit `t3` binary (local only).
    pub t3_bin: Option<PathBuf>,
    /// Absolute T3 data directory (local only).
    pub base_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct WaitRequest {
    pub targets: Vec<TargetSpec>,
    pub any: bool,
    pub predicate: Predicate,
    pub timeout: Duration,
    /// Local poll interval.
    pub interval: Duration,
    /// MCP `t3_thread_wait` long-poll slice.
    pub slice: Duration,
}

impl WaitRequest {
    pub fn to_json(&self) -> Value {
        let targets: Vec<Value> = self
            .targets
            .iter()
            .map(|t| {
                json!({
                    "transport": if t.transport == Transport::Local { "local" } else { "mcp" },
                    "machine": t.machine,
                    "threadId": t.thread_id,
                    "environmentId": t.environment_id,
                    "t3Bin": t.t3_bin.as_ref().map(|p| p.to_string_lossy()),
                    "baseDir": t.base_dir.as_ref().map(|p| p.to_string_lossy()),
                })
            })
            .collect();
        json!({
            "op": "wait",
            "targets": targets,
            "mode": if self.any { "any" } else { "all" },
            "expectMinRunOrdinal": self.predicate.min_run_ordinal,
            "untilDelegatedSettled": self.predicate.until_delegated,
            "timeoutMs": self.timeout.as_millis() as u64,
            "intervalMs": self.interval.as_millis() as u64,
            "sliceMs": self.slice.as_millis() as u64,
        })
    }

    pub fn from_json(v: &Value) -> Result<Self, String> {
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let ms = |k: &str, d: u64| Duration::from_millis(v.get(k).and_then(Value::as_u64).unwrap_or(d));
        let mut targets = Vec::new();
        for t in v.get("targets").and_then(Value::as_array).ok_or("missing targets")? {
            targets.push(TargetSpec {
                transport: match s(t, "transport").as_deref() {
                    Some("local") => Transport::Local,
                    Some("mcp") => Transport::Mcp,
                    _ => return Err("target has no valid transport".into()),
                },
                machine: s(t, "machine").ok_or("target missing machine")?,
                thread_id: s(t, "threadId").ok_or("target missing threadId")?,
                environment_id: s(t, "environmentId"),
                t3_bin: s(t, "t3Bin").map(PathBuf::from),
                base_dir: s(t, "baseDir").map(PathBuf::from),
            });
        }
        let req = WaitRequest {
            targets,
            any: s(v, "mode").as_deref() == Some("any"),
            predicate: Predicate {
                min_run_ordinal: v.get("expectMinRunOrdinal").and_then(Value::as_u64).unwrap_or(0),
                until_delegated: v.get("untilDelegatedSettled").and_then(Value::as_bool).unwrap_or(false),
            },
            timeout: ms("timeoutMs", 4 * 3_600_000),
            interval: ms("intervalMs", 5_000),
            slice: ms("sliceMs", 240_000),
        }
        .normalized();
        req.validate()?;
        Ok(req)
    }

    /// Clamps the tuning knobs: an interval of 0 would spawn `t3` in a tight
    /// loop, and a huge slice would pin a connection for hours.
    pub fn normalized(mut self) -> Self {
        self.interval = self.interval.max(MIN_INTERVAL);
        self.slice = self.slice.clamp(MIN_SLICE, MAX_SLICE);
        self
    }

    /// Checks what both the daemon and `--no-daemon` rely on.
    pub fn validate(&self) -> Result<(), String> {
        if self.targets.is_empty() {
            return Err("no threads to watch".into());
        }
        if self.targets.len() > MAX_TARGETS {
            return Err(format!("at most {MAX_TARGETS} threads per wait"));
        }
        for t in &self.targets {
            if t.thread_id.is_empty() || t.thread_id.len() > 1024 || util::has_control_chars(&t.thread_id) {
                return Err("invalid thread id".into());
            }
            if t.transport == Transport::Local {
                if self.predicate.until_delegated {
                    return Err(format!(
                        "--until-delegated-settled needs the mcp transport, but {} resolved to the local T3 CLI, \
                         which cannot see delegated work; pair the machine with `t3-watch auth login` and pass \
                         --machine or a thread URL",
                        t.thread_id
                    ));
                }
                match &t.base_dir {
                    Some(p) if p.is_absolute() => {}
                    _ => return Err("local targets need an absolute base dir".into()),
                }
                if t.t3_bin.as_ref().is_some_and(|p| !p.is_absolute()) {
                    return Err("local targets need an absolute t3 binary path".into());
                }
            }
        }
        Ok(())
    }
}

/// How a wait ended. The JSON is what the CLI prints.
#[derive(Debug)]
pub enum Outcome {
    Settled(Value),
    Timeout(Value),
    Error(Value),
    /// The client went away; nothing to report.
    Cancelled,
}

impl Outcome {
    pub fn exit_code(&self) -> i32 {
        match self {
            Outcome::Settled(_) => 0,
            Outcome::Error(_) => 2,
            Outcome::Timeout(_) => 3,
            Outcome::Cancelled => 1,
        }
    }

    pub fn json(&self) -> Option<&Value> {
        match self {
            Outcome::Settled(v) | Outcome::Timeout(v) | Outcome::Error(v) => Some(v),
            Outcome::Cancelled => None,
        }
    }

    pub fn json_mut(&mut self) -> Option<&mut Value> {
        match self {
            Outcome::Settled(v) | Outcome::Timeout(v) | Outcome::Error(v) => Some(v),
            Outcome::Cancelled => None,
        }
    }

    pub fn event(&self) -> &'static str {
        match self {
            Outcome::Settled(_) => "settled",
            Outcome::Timeout(_) => "timeout",
            Outcome::Error(_) => "error",
            Outcome::Cancelled => "cancelled",
        }
    }

    pub fn from_json(v: Value) -> Self {
        match v.get("event").and_then(Value::as_str) {
            Some("settled") => Outcome::Settled(v),
            Some("timeout") => Outcome::Timeout(v),
            _ => Outcome::Error(v),
        }
    }
}

pub fn error_json(kind: &str, message: &str) -> Value {
    json!({ "event": "error", "error": { "kind": kind, "message": message } })
}

pub enum SubMsg {
    /// A watch published a new observation.
    Update,
    /// The client's socket closed.
    ClientGone,
}

#[derive(Clone)]
enum Observation {
    State(ThreadState),
    Failed { error: CallError, at: Instant },
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum WatchKey {
    /// Keyed by the immutable origin and pairing, never by machine name, so
    /// a renamed or re-paired machine cannot reuse a stale backend.
    Mcp {
        origin: String,
        fingerprint: String,
        thread_id: String,
        environment_id: Option<String>,
        until_delegated: bool,
    },
    /// Keyed by resolved absolute paths, so clients with different installs
    /// or working directories never share a poller by accident.
    Local {
        base_dir: PathBuf,
        t3_bin: Option<PathBuf>,
        thread_id: String,
    },
}

impl WatchKey {
    fn thread_id(&self) -> &str {
        match self {
            WatchKey::Mcp { thread_id, .. } | WatchKey::Local { thread_id, .. } => thread_id,
        }
    }
}

enum Backend {
    Mcp(Arc<McpClient>),
    /// `t3_bin` is the explicit binary; otherwise the newest install under
    /// `base_dir` is looked up on every poll, so a T3 auto-update mid-wait
    /// is picked up.
    Local {
        t3_bin: Option<PathBuf>,
        base_dir: PathBuf,
    },
}

struct Sub {
    id: u64,
    tx: Sender<SubMsg>,
    /// This subscriber's `--interval`. The worker polls at the smallest one.
    interval: Duration,
}

struct WatchInner {
    subs: Vec<Sub>,
    last: Option<Observation>,
    last_transient: Option<String>,
    running: bool,
    /// Registration time of the newest subscriber. If it is later than the
    /// last observation, that subscriber has not seen fresh data yet and the
    /// worker should poll now instead of sleeping.
    newest_sub: Option<Instant>,
    last_observed: Option<Instant>,
    cycle_started: Option<Instant>,
}

struct Watch {
    key: WatchKey,
    backend: Backend,
    slice_ms: AtomicU64,
    inner: Mutex<WatchInner>,
    cv: Condvar,
    upstream_calls: AtomicU64,
    created: Instant,
}

/// What a worker does after an upstream cycle.
#[derive(Debug, PartialEq)]
enum Next {
    /// Local CLI: sleep for the poll interval.
    Interval,
    /// The long-poll already blocked for a whole slice: poll again now.
    Immediately,
    /// The wait proved an older run than the read saw: wait on the newer
    /// run right away (a few times, then gently).
    Rewait,
    /// The thread is idle but someone still waits (say for a higher run
    /// ordinal): poll with a growing delay.
    Gently,
}

struct Cycle {
    state: ThreadState,
    next: Next,
}

/// The fields of a `t3_thread_wait` result this tool relies on. Missing
/// fields never count as proof of completion.
#[derive(Debug, Default)]
struct WaitReply {
    status: Option<String>,
    timed_out: Option<bool>,
    run_id: Option<String>,
    result_run: Option<String>,
}

impl WaitReply {
    fn parse(v: &Value) -> Self {
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        WaitReply {
            status: s("status"),
            timed_out: v.get("timedOut").and_then(Value::as_bool),
            run_id: s("runId").or_else(|| s("latestRunId")),
            result_run: match v.get("resultRun") {
                Some(Value::String(id)) if !id.is_empty() => Some(id.clone()),
                Some(o @ Value::Object(_)) => o.get("runId").and_then(Value::as_str).map(str::to_string),
                _ => None,
            },
        }
    }

    fn class(&self) -> Option<StatusClass> {
        self.status.as_deref().map(classify)
    }

    /// The server said the wait finished (not timed out) on a settled run.
    fn proves_settled(&self) -> bool {
        self.timed_out == Some(false) && self.class() == Some(StatusClass::Settled)
    }

    fn proven_runs(&self) -> Vec<&str> {
        self.run_id
            .iter()
            .chain(self.result_run.iter())
            .map(String::as_str)
            .collect()
    }
}

/// One watch's state, copied once per evaluation so the settled decision and
/// the printed report always describe the same observations.
struct Snap {
    last: Option<Observation>,
    last_transient: Option<String>,
}

pub struct Hub {
    watches: Mutex<HashMap<WatchKey, Arc<Watch>>>,
    clients: Mutex<HashMap<(String, String), Arc<McpClient>>>,
    active_waits: AtomicUsize,
    next_sub: AtomicU64,
    idle_since: Mutex<Option<Instant>>,
    started: Instant,
    max_watches: usize,
    pub label: &'static str,
}

fn env_limit(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or(default)
}

impl Hub {
    pub fn new(label: &'static str) -> Arc<Self> {
        Self::with_max_watches(label, env_limit("T3_WATCH_MAX_WATCHES", DEFAULT_MAX_WATCHES))
    }

    pub fn with_max_watches(label: &'static str, max_watches: usize) -> Arc<Self> {
        Arc::new(Hub {
            watches: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            active_waits: AtomicUsize::new(0),
            next_sub: AtomicU64::new(1),
            idle_since: Mutex::new(Some(Instant::now())),
            started: Instant::now(),
            max_watches,
            label,
        })
    }

    /// How long the hub has had no waiting clients, if it is idle.
    pub fn idle_for(&self) -> Option<Duration> {
        if self.active_waits.load(Ordering::SeqCst) > 0 {
            return None;
        }
        lock(&self.idle_since).map(|t| t.elapsed())
    }

    /// Returns the shared client for this machine's (origin, pairing),
    /// creating it under the map lock so two waits never build two.
    fn mcp_client(&self, name: &str, m: &Machine) -> Result<Arc<McpClient>, Value> {
        let origin = util::secure_url(&m.origin, &format!("origin of machine {name:?}"))
            .map_err(|e| error_json("config", &e))?
            .origin;
        let key = (origin.clone(), m.fingerprint());
        let mut clients = lock(&self.clients);
        if let Some(c) = clients.get(&key) {
            return Ok(c.clone());
        }
        let recovery: TokenRecovery = {
            let (name, origin) = (name.to_string(), origin.clone());
            Box::new(move |used: &str| oauth::recover_token(&name, &origin, used))
        };
        let c = McpClient::new(name, &origin, &m.access_token, &key.1, Some(recovery))
            .map(Arc::new)
            .map_err(|e| error_json("config", &e))?;
        clients.insert(key, c.clone());
        Ok(c)
    }

    fn resolve(&self, req: &WaitRequest) -> Result<Vec<(WatchKey, Backend)>, Value> {
        let mut cfg: Option<Config> = None;
        let mut out = Vec::new();
        for t in &req.targets {
            let entry = match t.transport {
                Transport::Mcp => {
                    if cfg.is_none() {
                        cfg = Some(Config::load().map_err(|e| error_json("config", &e))?);
                    }
                    let cfg = cfg.as_ref().unwrap();
                    let m = cfg.machines.get(&t.machine).ok_or_else(|| {
                        error_json(
                            "config",
                            &format!(
                                "no saved machine named {:?}; run `t3-watch auth login <origin>` first",
                                t.machine
                            ),
                        )
                    })?;
                    if m.access_token.is_empty() {
                        return Err(error_json(
                            "auth",
                            &format!(
                                "machine {:?} has no token; run `t3-watch auth login {}`",
                                t.machine, m.origin
                            ),
                        ));
                    }
                    let client = self.mcp_client(&t.machine, m)?;
                    let key = WatchKey::Mcp {
                        origin: client.origin().to_string(),
                        fingerprint: client.fingerprint().to_string(),
                        thread_id: t.thread_id.clone(),
                        environment_id: t.environment_id.clone(),
                        until_delegated: req.predicate.until_delegated,
                    };
                    (key, Backend::Mcp(client))
                }
                Transport::Local => {
                    let base_dir = t
                        .base_dir
                        .clone()
                        .ok_or_else(|| error_json("usage", "missing base dir"))?;
                    match &t.t3_bin {
                        Some(bin) if !bin.is_file() => {
                            return Err(error_json(
                                "config",
                                &format!("t3 binary {} does not exist", bin.display()),
                            ))
                        }
                        None if config::find_t3_in_base(&base_dir).is_none() => {
                            return Err(error_json(
                                "config",
                                &format!("no t3 CLI under {}; pass --t3-bin", base_dir.display()),
                            ))
                        }
                        _ => {}
                    }
                    let key = WatchKey::Local {
                        base_dir: base_dir.clone(),
                        t3_bin: t.t3_bin.clone(),
                        thread_id: t.thread_id.clone(),
                    };
                    (
                        key,
                        Backend::Local {
                            t3_bin: t.t3_bin.clone(),
                            base_dir,
                        },
                    )
                }
            };
            out.push(entry);
        }
        Ok(out)
    }

    fn subscribe(
        self: &Arc<Self>,
        key: WatchKey,
        backend: Backend,
        req: &WaitRequest,
        sub: u64,
        tx: Sender<SubMsg>,
    ) -> Result<Arc<Watch>, Value> {
        let mut map = lock(&self.watches);
        if !map.contains_key(&key) && map.len() >= self.max_watches {
            return Err(error_json(
                "limit",
                &format!("the daemon is already watching {} threads, its limit", self.max_watches),
            ));
        }
        let w = map
            .entry(key.clone())
            .or_insert_with(|| {
                Arc::new(Watch {
                    key,
                    backend,
                    slice_ms: AtomicU64::new(req.slice.as_millis() as u64),
                    inner: Mutex::new(WatchInner {
                        subs: Vec::new(),
                        last: None,
                        last_transient: None,
                        running: false,
                        newest_sub: None,
                        last_observed: None,
                        cycle_started: None,
                    }),
                    cv: Condvar::new(),
                    upstream_calls: AtomicU64::new(0),
                    created: Instant::now(),
                })
            })
            .clone();
        let spawn = {
            let mut g = lock(&w.inner);
            g.subs.push(Sub {
                id: sub,
                tx,
                interval: req.interval,
            });
            g.newest_sub = Some(Instant::now());
            !std::mem::replace(&mut g.running, true)
        };
        if spawn {
            let (hub, watch) = (self.clone(), w.clone());
            if let Err(e) = util::spawn_thread("t3-watch-worker", 256 * 1024, move || watch.run(&hub)) {
                // Nobody will ever poll this watch: undo the subscription and
                // tell the caller instead of letting it wait until timeout.
                let mut g = lock(&w.inner);
                g.subs.retain(|s| s.id != sub);
                g.running = false;
                if g.subs.is_empty() {
                    drop(g);
                    map.remove(&w.key);
                }
                return Err(error_json("resource", &format!("cannot start a worker thread: {e}")));
            }
        }
        drop(map);
        w.cv.notify_all();
        Ok(w)
    }

    /// Blocks until the request is satisfied, times out, fails, or the
    /// client goes away (`ClientGone` on `rx`).
    pub fn wait(self: &Arc<Self>, req: &WaitRequest, tx: Sender<SubMsg>, rx: Receiver<SubMsg>) -> Outcome {
        let started = Instant::now();
        let deadline = started + req.timeout;
        if let Err(e) = req.validate() {
            return Outcome::Error(error_json("usage", &e));
        }
        let resolved = match self.resolve(req) {
            Ok(r) => r,
            Err(e) => return Outcome::Error(e),
        };
        self.active_waits.fetch_add(1, Ordering::SeqCst);
        let sub = self.next_sub.fetch_add(1, Ordering::Relaxed);
        let mut watches = Vec::new();
        let mut failure = None;
        for (key, backend) in resolved {
            match self.subscribe(key, backend, req, sub, tx.clone()) {
                Ok(w) => watches.push(w),
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        drop(tx);

        let outcome = match failure {
            Some(e) => Outcome::Error(e),
            None => loop {
                let snaps = snapshot(&watches);
                if let Some(done) = self.evaluate(&snaps, req, started) {
                    break done;
                }
                let now = Instant::now();
                if now >= deadline {
                    break Outcome::Timeout(self.report("timeout", &snaps, req, started, None));
                }
                match rx.recv_timeout(deadline - now) {
                    Ok(SubMsg::Update) => {}
                    Ok(SubMsg::ClientGone) | Err(RecvTimeoutError::Disconnected) => break Outcome::Cancelled,
                    Err(RecvTimeoutError::Timeout) => {}
                }
            },
        };

        for w in &watches {
            w.unsubscribe(sub);
        }
        if self.active_waits.fetch_sub(1, Ordering::SeqCst) == 1 {
            *lock(&self.idle_since) = Some(Instant::now());
        }
        outcome
    }

    /// Decides from one snapshot, and builds the report from that same
    /// snapshot, so `event` and the per-thread `settled` flags always agree.
    fn evaluate(&self, snaps: &[Snap], req: &WaitRequest, started: Instant) -> Option<Outcome> {
        let mut settled = 0;
        for (i, snap) in snaps.iter().enumerate() {
            match &snap.last {
                Some(Observation::Failed { error, at }) if *at >= started => {
                    let t = &req.targets[i];
                    let err = json!({
                        "kind": error.kind(),
                        "message": error.message(),
                        "machine": t.machine,
                        "threadId": t.thread_id,
                    });
                    return Some(Outcome::Error(self.report("error", snaps, req, started, Some(err))));
                }
                Some(Observation::State(s)) if s.observed_at >= started && s.satisfies(&req.predicate) => settled += 1,
                _ => {}
            }
        }
        let done = if req.any { settled > 0 } else { settled == snaps.len() };
        done.then(|| Outcome::Settled(self.report("settled", snaps, req, started, None)))
    }

    fn report(&self, event: &str, snaps: &[Snap], req: &WaitRequest, started: Instant, error: Option<Value>) -> Value {
        let threads: Vec<Value> = snaps
            .iter()
            .zip(&req.targets)
            .map(|(snap, t)| {
                let mut m = Map::new();
                m.insert("machine".into(), json!(t.machine));
                m.insert("threadId".into(), json!(t.thread_id));
                if let Some(env) = &t.environment_id {
                    m.insert("environmentId".into(), json!(env));
                }
                match &snap.last {
                    Some(Observation::State(s)) => {
                        if let Value::Object(fields) = s.to_json(&req.predicate) {
                            m.extend(fields);
                        }
                        let fresh = s.observed_at >= started;
                        m.insert("settled".into(), json!(fresh && s.satisfies(&req.predicate)));
                        if !fresh {
                            m.insert("stale".into(), json!(true));
                        }
                    }
                    _ => {
                        m.insert("status".into(), Value::Null);
                        m.insert("settled".into(), json!(false));
                    }
                }
                if let Some(e) = &snap.last_transient {
                    m.insert("lastError".into(), json!(e));
                }
                Value::Object(m)
            })
            .collect();
        let mut out = json!({
            "event": event,
            "mode": if req.any { "any" } else { "all" },
            "elapsedMs": started.elapsed().as_millis() as u64,
            "via": self.label,
            "threads": threads,
        });
        if req.predicate.min_run_ordinal > 0 {
            out["expectMinRunOrdinal"] = json!(req.predicate.min_run_ordinal);
        }
        if req.predicate.until_delegated {
            out["untilDelegatedSettled"] = json!(true);
        }
        if let Some(e) = error {
            out["error"] = e;
        }
        out
    }

    pub fn status_json(&self) -> Value {
        let watches: Vec<Value> = lock(&self.watches)
            .values()
            .map(|w| {
                let g = lock(&w.inner);
                let status = match &g.last {
                    Some(Observation::State(s)) => json!(s.status),
                    Some(Observation::Failed { error, .. }) => json!(format!("error: {}", error.kind())),
                    None => Value::Null,
                };
                let mut v = json!({
                    "threadId": w.key.thread_id(),
                    "subscribers": g.subs.len(),
                    "upstreamCalls": w.upstream_calls.load(Ordering::Relaxed),
                    "status": status,
                    "lastError": g.last_transient,
                    "ageMs": w.created.elapsed().as_millis() as u64,
                });
                match &w.key {
                    WatchKey::Mcp { origin, .. } => {
                        v["transport"] = json!("mcp");
                        v["origin"] = json!(origin);
                    }
                    WatchKey::Local { base_dir, t3_bin, .. } => {
                        v["transport"] = json!("local");
                        v["baseDir"] = json!(base_dir.to_string_lossy());
                        v["t3Bin"] = json!(t3_bin.as_ref().map(|p| p.to_string_lossy()));
                    }
                }
                v
            })
            .collect();
        let machines: Vec<Value> = lock(&self.clients)
            .values()
            .map(|c| {
                json!({
                    "machine": c.label,
                    "endpoint": c.endpoint(),
                    "fingerprint": c.fingerprint(),
                    "hasSession": c.session_id().is_some(),
                    "initialize": c.stats.initialize.load(Ordering::Relaxed),
                    "threadWait": c.stats.thread_wait.load(Ordering::Relaxed),
                    "threadRead": c.stats.thread_read.load(Ordering::Relaxed),
                    "sessionResets": c.stats.session_resets.load(Ordering::Relaxed),
                    "tokenRecoveries": c.stats.token_recoveries.load(Ordering::Relaxed),
                    "retries": c.stats.retries.load(Ordering::Relaxed),
                })
            })
            .collect();
        json!({
            "pid": std::process::id(),
            "version": env!("CARGO_PKG_VERSION"),
            "uptimeMs": self.started.elapsed().as_millis() as u64,
            "activeWaits": self.active_waits.load(Ordering::SeqCst),
            "maxWatches": self.max_watches,
            "watches": watches,
            "machines": machines,
        })
    }
}

fn snapshot(watches: &[Arc<Watch>]) -> Vec<Snap> {
    watches
        .iter()
        .map(|w| {
            let g = lock(&w.inner);
            Snap {
                last: g.last.clone(),
                last_transient: g.last_transient.clone(),
            }
        })
        .collect()
}

impl Watch {
    fn poll_interval(&self) -> Duration {
        let g = lock(&self.inner);
        g.subs
            .iter()
            .map(|s| s.interval)
            .min()
            .unwrap_or(Duration::from_secs(5))
    }

    fn unsubscribe(&self, sub: u64) {
        let mut g = lock(&self.inner);
        g.subs.retain(|s| s.id != sub);
        let empty = g.subs.is_empty();
        drop(g);
        if empty {
            self.cv.notify_all();
        }
    }

    fn publish(&self, obs: Observation) {
        let mut g = lock(&self.inner);
        if let Observation::State(s) = &obs {
            g.last_transient = None;
            g.last_observed = Some(s.observed_at);
        }
        g.last = Some(obs);
        for s in &g.subs {
            let _ = s.tx.send(SubMsg::Update);
        }
    }

    /// Returns false (and deregisters) once nobody is subscribed.
    fn keep_running(self: &Arc<Self>, hub: &Hub) -> bool {
        let mut map = lock(&hub.watches);
        let mut g = lock(&self.inner);
        if !g.subs.is_empty() {
            g.cycle_started = Some(Instant::now());
            return true;
        }
        g.running = false;
        if map.get(&self.key).is_some_and(|w| Arc::ptr_eq(w, self)) {
            map.remove(&self.key);
        }
        false
    }

    /// Sleeps for `delay`, waking early when the last subscriber leaves or,
    /// if `poll_on_new_sub`, when a subscriber arrives that has not seen an
    /// observation from after its registration.
    fn pause(&self, delay: Duration, poll_on_new_sub: bool) {
        let deadline = Instant::now() + delay;
        let mut g = lock(&self.inner);
        loop {
            if g.subs.is_empty() {
                return;
            }
            if poll_on_new_sub && g.newest_sub > g.last_observed {
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            g = self
                .cv
                .wait_timeout(g, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Runs one upstream cycle on a helper thread and waits for it, but gives
    /// up the moment the last subscriber leaves: a long-poll can take
    /// minutes, and an abandoned watch must not linger that long. `None`
    /// means it was abandoned; the helper finishes on its own and its result
    /// is dropped.
    fn cycle_abandonable(self: &Arc<Self>) -> Option<Result<Cycle, CallError>> {
        let (tx, rx) = mpsc::channel();
        let me = self.clone();
        let spawned = util::spawn_thread("t3-watch-call", 256 * 1024, move || {
            let result = me.cycle();
            let _ = tx.send(result);
            // Taking the lock orders this notify after the worker's check.
            let _g = lock(&me.inner);
            me.cv.notify_all();
        });
        if let Err(e) = spawned {
            return Some(Err(CallError::Transient {
                message: format!("cannot start an upstream call thread: {e}"),
                status: None,
            }));
        }
        let mut g = lock(&self.inner);
        loop {
            if let Ok(result) = rx.try_recv() {
                return Some(result);
            }
            if g.subs.is_empty() {
                return None;
            }
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn cycle(&self) -> Result<Cycle, CallError> {
        match &self.backend {
            Backend::Mcp(client) => self.mcp_cycle(client),
            Backend::Local { t3_bin, base_dir } => {
                let bin = t3_bin
                    .clone()
                    .or_else(|| config::find_t3_in_base(base_dir))
                    .ok_or_else(|| CallError::Transient {
                        message: format!("no t3 CLI under {}", base_dir.display()),
                        status: None,
                    })?;
                self.upstream_calls.fetch_add(1, Ordering::Relaxed);
                let state = local::show(&bin, base_dir, self.key.thread_id())?;
                Ok(Cycle {
                    state,
                    next: Next::Interval,
                })
            }
        }
    }

    fn run(self: Arc<Self>, hub: &Hub) {
        let mut jitter = Jitter::new();
        let mut failures: u32 = 0;
        let mut quiet_polls: u32 = 0;
        let mut rewaits: u32 = 0;
        while self.keep_running(hub) {
            let Some(result) = self.cycle_abandonable() else {
                continue;
            };
            let delay = match result {
                Ok(cycle) => {
                    failures = 0;
                    self.publish(Observation::State(cycle.state));
                    if cycle.next != Next::Rewait {
                        rewaits = 0;
                    }
                    match cycle.next {
                        Next::Interval => self.poll_interval(),
                        Next::Immediately => {
                            quiet_polls = 0;
                            Duration::ZERO
                        }
                        Next::Rewait if rewaits < 3 => {
                            rewaits += 1;
                            Duration::ZERO
                        }
                        Next::Rewait | Next::Gently => {
                            quiet_polls += 1;
                            Duration::from_millis((1000.0 * 1.5f64.powi(quiet_polls.min(8) as i32)) as u64)
                                .min(Duration::from_secs(15))
                        }
                    }
                }
                Err(CallError::Transient { message, status } | CallError::Session { message, status }) => {
                    if let Backend::Mcp(c) = &self.backend {
                        c.stats.retries.fetch_add(1, Ordering::Relaxed);
                    }
                    if matches!(status, Some(504 | 524)) {
                        // Cloudflare drops responses idle for ~100s; shorten
                        // long polls on this watch so they finish in time.
                        self.slice_ms.fetch_min(90_000, Ordering::Relaxed);
                    }
                    lock(&self.inner).last_transient = Some(message);
                    failures += 1;
                    jitter.backoff(failures - 1, Duration::from_secs(1), Duration::from_secs(30))
                }
                Err(error) => {
                    let at = lock(&self.inner).cycle_started.unwrap_or_else(Instant::now);
                    self.publish(Observation::Failed { error, at });
                    failures += 1;
                    jitter.backoff(failures - 1, Duration::from_secs(2), Duration::from_secs(60))
                }
            };
            self.pause(delay, failures == 0);
        }
    }

    /// One `t3_thread_wait` long-poll, verified by `t3_thread_read` unless
    /// the poll timed out on a still-active thread.
    fn mcp_cycle(&self, client: &McpClient) -> Result<Cycle, CallError> {
        let WatchKey::Mcp {
            thread_id,
            environment_id,
            until_delegated,
            ..
        } = &self.key
        else {
            unreachable!("MCP backend with a local key");
        };
        let slice_ms = self.slice_ms.load(Ordering::Relaxed);
        let mut args = json!({ "threadId": thread_id, "timeoutMs": slice_ms });
        if *until_delegated {
            args["untilDelegatedWorkSettled"] = json!(true);
        }
        if let Some(env) = environment_id {
            args["environmentId"] = json!(env);
        }
        let wait_started = Instant::now();
        self.upstream_calls.fetch_add(1, Ordering::Relaxed);
        let raw = client.call_tool(
            "t3_thread_wait",
            args,
            Duration::from_millis(slice_ms) + Duration::from_secs(30),
        )?;
        let w = WaitReply::parse(&raw);
        if w.timed_out == Some(true) && w.class() == Some(StatusClass::Active) {
            let state =
                ThreadState::from_thread_json(&json!({ "status": w.status, "latestRunId": w.run_id }), wait_started);
            return Ok(Cycle {
                state,
                next: Next::Immediately,
            });
        }

        let mut args = json!({ "threadId": thread_id, "limit": 1, "maxCharsPerItem": 200 });
        if let Some(env) = environment_id {
            args["environmentId"] = json!(env);
        }
        let read_started = Instant::now();
        let r = client.call_tool("t3_thread_read", args, Duration::from_secs(30))?;
        let mut state = ThreadState::from_thread_json(&r, read_started);
        state.result_run_id = w.result_run.clone();
        let mut next = if w.timed_out == Some(true) {
            Next::Immediately
        } else {
            Next::Gently
        };
        if *until_delegated {
            let proven = w.proven_runs();
            state.delegated_settled = w.proves_settled() && run_covered(&proven, state.latest_run_id.as_deref());
            // The wait settled an older run than the read now shows: a new
            // run started in between, and its delegated work is unproven.
            if w.proves_settled() && !state.delegated_settled && !proven.is_empty() {
                next = Next::Rewait;
            }
        }
        Ok(Cycle { state, next })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn local_target(base: &std::path::Path, bin: Option<PathBuf>, id: &str) -> TargetSpec {
        TargetSpec {
            transport: Transport::Local,
            machine: LOCAL_MACHINE.into(),
            thread_id: id.into(),
            environment_id: None,
            t3_bin: bin,
            base_dir: Some(base.to_path_buf()),
        }
    }

    fn request(targets: Vec<TargetSpec>) -> WaitRequest {
        WaitRequest {
            targets,
            any: false,
            predicate: Predicate::default(),
            timeout: Duration::from_secs(5),
            interval: Duration::from_millis(50),
            slice: Duration::from_secs(1),
        }
    }

    fn script(tag: &str, status: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "t3we-{tag}-{}-{}",
            std::process::id(),
            util::hex(&util::random_bytes(4))
        ));
        fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("t3");
        fs::write(
            &bin,
            format!(
                "#!/bin/sh\nprintf '{{\"thread\":{{\"status\":\"{status}\",\"latestRunId\":\"r:ordinal:1\"}}}}\\n'\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        (dir, bin)
    }

    #[test]
    fn wait_reply_never_invents_completion() {
        let w = WaitReply::parse(&json!({"status": "completed"}));
        assert!(!w.proves_settled(), "missing timedOut is not proof");
        let w = WaitReply::parse(&json!({"timedOut": false}));
        assert!(!w.proves_settled(), "missing status is not proof");
        let w = WaitReply::parse(&json!({"timedOut": false, "status": "mystery"}));
        assert!(!w.proves_settled(), "unknown status is not proof");
        let w = WaitReply::parse(
            &json!({"timedOut": false, "status": "completed", "runId": "a", "resultRun": {"runId": "b"}}),
        );
        assert!(w.proves_settled());
        assert_eq!(w.proven_runs(), ["a", "b"]);
        let w = WaitReply::parse(&json!({"timedOut": false, "status": "idle", "latestRunId": "c"}));
        assert_eq!(w.proven_runs(), ["c"]);
    }

    #[test]
    fn requests_are_clamped_and_validated() {
        let base = std::env::temp_dir();
        let mut r = request(vec![local_target(&base, None, "t")]);
        r.interval = Duration::ZERO;
        r.slice = Duration::from_secs(86_400);
        let r = r.normalized();
        assert_eq!(r.interval, MIN_INTERVAL);
        assert_eq!(r.slice, MAX_SLICE);
        let parsed = WaitRequest::from_json(&r.to_json()).unwrap();
        assert_eq!(parsed.slice, MAX_SLICE);

        let mut delegated = request(vec![local_target(&base, None, "t")]);
        delegated.predicate.until_delegated = true;
        assert!(delegated.validate().unwrap_err().contains("mcp transport"));
        assert!(WaitRequest::from_json(&delegated.to_json()).is_err());

        let relative = request(vec![local_target(std::path::Path::new("rel"), None, "t")]);
        assert!(relative.validate().is_err());
        let bad_bin = request(vec![local_target(&base, Some("t3".into()), "t")]);
        assert!(bad_bin.validate().is_err());
        assert!(request(vec![local_target(&base, None, "a\nb")]).validate().is_err());
        assert!(request(vec![]).validate().is_err());
    }

    #[test]
    fn report_comes_from_the_snapshot_that_was_judged() {
        let hub = Hub::new("test");
        let req = request(vec![
            local_target(std::path::Path::new("/x"), None, "a"),
            local_target(std::path::Path::new("/x"), None, "b"),
        ]);
        let started = Instant::now();
        let done = ThreadState::from_thread_json(
            &json!({"status": "completed", "latestRunId": "r:ordinal:1"}),
            Instant::now(),
        );
        let busy = ThreadState::from_thread_json(&json!({"status": "running"}), Instant::now());
        let snaps = vec![
            Snap {
                last: Some(Observation::State(done.clone())),
                last_transient: None,
            },
            Snap {
                last: Some(Observation::State(busy)),
                last_transient: Some("net".into()),
            },
        ];
        assert!(hub.evaluate(&snaps, &req, started).is_none(), "mode all needs both");
        let snaps = vec![
            Snap {
                last: Some(Observation::State(done.clone())),
                last_transient: None,
            },
            Snap {
                last: Some(Observation::State(done)),
                last_transient: None,
            },
        ];
        let Some(Outcome::Settled(v)) = hub.evaluate(&snaps, &req, started) else {
            panic!("expected settled");
        };
        assert!(v["threads"].as_array().unwrap().iter().all(|t| t["settled"] == true));
    }

    #[test]
    fn worker_spawn_failure_reaches_the_subscriber() {
        let (dir, bin) = script("spawn", "running");
        let hub = Hub::new("test");
        let req = request(vec![local_target(&dir, Some(bin), "t")]);
        let (tx, rx) = mpsc::channel();
        util::FAIL_SPAWNS.with(|c| c.set((0, 1)));
        let started = Instant::now();
        let outcome = hub.wait(&req, tx, rx);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "must not wait for the timeout"
        );
        let Outcome::Error(v) = outcome else {
            panic!("expected error")
        };
        assert_eq!(v["error"]["kind"], "resource");
        assert!(lock(&hub.watches).is_empty(), "the failed watch must be removed");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn watch_limit_is_enforced() {
        let hub = Hub::with_max_watches("test", 1);
        let (dir, bin) = script("limit", "running");
        let req = request(vec![
            local_target(&dir, Some(bin.clone()), "a"),
            local_target(&dir, Some(bin), "b"),
        ]);
        let (tx, rx) = mpsc::channel();
        let Outcome::Error(v) = hub.wait(&req, tx, rx) else {
            panic!("expected error")
        };
        assert_eq!(v["error"]["kind"], "limit");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn abandoned_watches_are_dropped_promptly() {
        // A `t3` that hangs: the worker is stuck in its first call when the
        // only subscriber times out.
        let dir = std::env::temp_dir().join(format!(
            "t3we-hang-{}-{}",
            std::process::id(),
            util::hex(&util::random_bytes(4))
        ));
        fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("t3");
        fs::write(&bin, "#!/bin/sh\nsleep 10\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let hub = Hub::new("test");
        let mut req = request(vec![local_target(&dir, Some(bin), "t")]);
        req.timeout = Duration::from_millis(300);
        let (tx, rx) = mpsc::channel();
        assert!(matches!(hub.wait(&req, tx, rx), Outcome::Timeout(_)));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !lock(&hub.watches).is_empty() {
            assert!(Instant::now() < deadline, "watch still registered while its call hangs");
            std::thread::sleep(Duration::from_millis(10));
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
