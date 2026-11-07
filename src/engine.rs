//! The watch engine shared by the daemon and `--no-daemon` mode.
//!
//! A `Hub` owns one `Watch` per key: (origin, pairing, thread, environment,
//! completion mode, final marker) for MCP and (data dir, `t3` binary,
//! thread, final marker) for the local CLI. Each watch has one worker thread
//! that talks upstream; every waiting client subscribes to the watches it
//! needs and evaluates its own condition (`mode`, run ordinal, acknowledged
//! input) against their shared observations. So 20 clients on one thread cost
//! one upstream long-poll, not 20.
//!
//! Completion has two contracts. Turn mode settles when the latest run of
//! the thread has finished. Workstream mode (MCP only) settles only on T3's
//! own proof, `t3_thread_wait` with `untilDelegatedWorkSettled`, whose
//! `resultRun` names the run holding the final answer after delegated work
//! and the parent runs that deliver its results. A server that does not give
//! that proof gets an `unsupported` error, never a guess.

use crate::cancel::Cancel;
use crate::config::{self, Config, Machine};
use crate::local;
use crate::mcp::{CallError, McpClient, TokenRecovery, PROBE_TIMEOUT_MS};
use crate::model::{
    classify, pending_from_activity, run_ordinal_of, workstream_covered, Predicate, StatusClass, ThreadOutcome,
    ThreadState, SUCCESS_STATUSES,
};
use crate::oauth;
use crate::util::{self, lock, Jitter};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
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
/// Default long-poll slice. A question or approval that arrives while the
/// run still says `running` is only seen by the read after each slice, so
/// this also bounds how late a needs-input wake can be.
pub const DEFAULT_SLICE: Duration = Duration::from_secs(60);
const MAX_TARGETS: usize = 256;
const DEFAULT_MAX_WATCHES: usize = 1024;
const MAX_MARKER: usize = 256;
/// `afterPosition` past any real item: the read returns the thread header
/// and no timeline items. The watcher never reads message content it does
/// not need, because reading a delegated child's final result marks it
/// delivered in T3.
const HEADER_ONLY: u64 = 1_000_000_000;

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
    /// Report `upstream_unreachable` progress once T3 has been unreachable
    /// this long (and `upstream_restored` when it is back).
    pub notify_unreachable: Option<Duration>,
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
        let p = &self.predicate;
        json!({
            "op": "wait",
            "targets": targets,
            "mode": if self.any { "any" } else { "all" },
            "until": if p.until_delegated { "workstream" } else { "turn" },
            "expectMinRunOrdinal": p.min_run_ordinal,
            "finalMarker": p.final_marker,
            "acknowledge": p.acknowledged,
            "follow": p.follow,
            "timeoutMs": self.timeout.as_millis() as u64,
            "intervalMs": self.interval.as_millis() as u64,
            "sliceMs": self.slice.as_millis() as u64,
            "notifyUnreachableMs": self.notify_unreachable.map(|d| d.as_millis() as u64),
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
        let until_delegated = match s(v, "until").as_deref() {
            Some("workstream") => true,
            Some("turn") | None => v.get("untilDelegatedSettled").and_then(Value::as_bool).unwrap_or(false),
            Some(other) => return Err(format!("unknown completion mode {:?}", util::sanitize_text(other, 40))),
        };
        let req = WaitRequest {
            targets,
            any: s(v, "mode").as_deref() == Some("any"),
            predicate: Predicate {
                min_run_ordinal: v.get("expectMinRunOrdinal").and_then(Value::as_u64).unwrap_or(0),
                until_delegated,
                final_marker: s(v, "finalMarker"),
                acknowledged: s(v, "acknowledge"),
                follow: v.get("follow").and_then(Value::as_bool).unwrap_or(false),
            },
            timeout: ms("timeoutMs", 4 * 3_600_000),
            interval: ms("intervalMs", 5_000),
            slice: ms("sliceMs", DEFAULT_SLICE.as_millis() as u64),
            notify_unreachable: v
                .get("notifyUnreachableMs")
                .and_then(Value::as_u64)
                .map(Duration::from_millis),
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
        if let Some(m) = &self.predicate.final_marker {
            if m.trim().is_empty() || m.len() > MAX_MARKER || util::has_control_chars(m) {
                return Err(format!(
                    "--final-marker must be 1 to {MAX_MARKER} bytes of printable text"
                ));
            }
        }
        if let Some(k) = &self.predicate.acknowledged {
            if !k.starts_with("pk-") || k.len() > 64 || util::has_control_chars(k) {
                return Err("--acknowledge takes a pendingKey from an earlier needs_input result".into());
            }
        }
        for t in &self.targets {
            if t.thread_id.is_empty() || t.thread_id.len() > 1024 || util::has_control_chars(&t.thread_id) {
                return Err("invalid thread id".into());
            }
            if t.transport == Transport::Local {
                if self.predicate.until_delegated {
                    return Err(format!(
                        "--until workstream needs the mcp transport, but {} resolved to the local T3 CLI, \
                         which cannot prove that delegated work has settled; pair the machine with \
                         `t3-watch auth login` and pass --machine or a thread URL, or use --until turn \
                         with --final-marker",
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
    /// Every awaited thread (or one, with `--mode any`) reached a terminal
    /// outcome. `outcome` in the JSON says `completed` or `failed`.
    Settled(Value),
    /// A question or approval waits on a thread.
    NeedsInput(Value),
    Timeout(Value),
    Error(Value),
    /// The client went away; nothing to report.
    Cancelled,
}

pub const EXIT_COMPLETED: i32 = 0;
pub const EXIT_CANCELLED: i32 = 1;
pub const EXIT_ERROR: i32 = 2;
pub const EXIT_TIMEOUT: i32 = 3;
pub const EXIT_NEEDS_INPUT: i32 = 5;
pub const EXIT_FAILED: i32 = 6;

impl Outcome {
    pub fn exit_code(&self) -> i32 {
        match self {
            Outcome::Settled(v) if v["outcome"] == "completed" => EXIT_COMPLETED,
            Outcome::Settled(_) => EXIT_FAILED,
            Outcome::NeedsInput(_) => EXIT_NEEDS_INPUT,
            Outcome::Error(_) => EXIT_ERROR,
            Outcome::Timeout(_) => EXIT_TIMEOUT,
            Outcome::Cancelled => EXIT_CANCELLED,
        }
    }

    pub fn json(&self) -> Option<&Value> {
        match self {
            Outcome::Settled(v) | Outcome::NeedsInput(v) | Outcome::Timeout(v) | Outcome::Error(v) => Some(v),
            Outcome::Cancelled => None,
        }
    }

    pub fn json_mut(&mut self) -> Option<&mut Value> {
        match self {
            Outcome::Settled(v) | Outcome::NeedsInput(v) | Outcome::Timeout(v) | Outcome::Error(v) => Some(v),
            Outcome::Cancelled => None,
        }
    }

    pub fn event(&self) -> &'static str {
        match self {
            Outcome::Settled(_) => "settled",
            Outcome::NeedsInput(_) => "needs_input",
            Outcome::Timeout(_) => "timeout",
            Outcome::Error(_) => "error",
            Outcome::Cancelled => "cancelled",
        }
    }

    pub fn from_json(v: Value) -> Self {
        match v.get("event").and_then(Value::as_str) {
            // A settled result without an outcome came from something that
            // does not know about failures; never read it as success.
            Some("settled") if v.get("outcome").is_some() => Outcome::Settled(v),
            Some("needs_input") => Outcome::NeedsInput(v),
            Some("timeout") => Outcome::Timeout(v),
            _ => Outcome::Error(v),
        }
    }
}

pub fn error_json(kind: &str, message: &str) -> Value {
    json!({ "event": "error", "outcome": "error", "error": { "kind": kind, "message": message } })
}

pub enum SubMsg {
    /// A watch published a new observation (or a transient failure).
    Update,
    /// The client's socket closed.
    ClientGone,
}

#[derive(Clone)]
enum Observation {
    State(Box<ThreadState>),
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
        final_marker: Option<String>,
    },
    /// Keyed by resolved absolute paths, so clients with different installs
    /// or working directories never share a poller by accident.
    Local {
        base_dir: PathBuf,
        t3_bin: Option<PathBuf>,
        thread_id: String,
        final_marker: Option<String>,
    },
}

impl WatchKey {
    fn thread_id(&self) -> &str {
        match self {
            WatchKey::Mcp { thread_id, .. } | WatchKey::Local { thread_id, .. } => thread_id,
        }
    }

    fn final_marker(&self) -> Option<&str> {
        match self {
            WatchKey::Mcp { final_marker, .. } | WatchKey::Local { final_marker, .. } => final_marker.as_deref(),
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

/// Upstream reachability as this process sees it: when the current run of
/// failed calls started and the latest error. It says nothing about whether
/// the T3 host is powered on, only that this watcher cannot reach it.
#[derive(Clone)]
struct Outage {
    since: Instant,
    since_wall_ms: u64,
    failures: u32,
    last_error: String,
}

struct WatchInner {
    subs: Vec<Sub>,
    last: Option<Observation>,
    outage: Option<Outage>,
    running: bool,
    /// Registration time of the newest subscriber. If it is later than the
    /// last observation, that subscriber has not seen fresh data yet and the
    /// worker should poll now instead of sleeping.
    newest_sub: Option<Instant>,
    last_observed: Option<Instant>,
    cycle_started: Option<Instant>,
}

/// Answers that only change when the thread does, so a watch sitting on an
/// idle or blocked thread does not refetch them every cycle.
#[derive(Default)]
struct Caches {
    /// (item count, pending count, status, latest run) -> request details.
    pending: Option<(String, Vec<crate::model::PendingRequest>, Vec<String>)>,
    /// (run, item count) -> marker seen.
    marker: Option<(String, bool)>,
    /// latest run -> delegated work still running (turn mode).
    probe: Option<(String, bool)>,
    /// The latest run the previous read saw, so a later wait is known to
    /// have been issued after that run existed.
    seen_latest: Option<String>,
}

struct Watch {
    key: WatchKey,
    backend: Backend,
    slice_ms: AtomicU64,
    inner: Mutex<WatchInner>,
    cv: Condvar,
    caches: Mutex<Caches>,
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
    thread_id: Option<String>,
    status: Option<String>,
    timed_out: Option<bool>,
    result_run: Option<String>,
    result_status: Option<String>,
}

impl WaitReply {
    fn parse(v: &Value) -> Self {
        let s = |v: &Value, k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let (result_run, result_status) = match v.get("resultRun") {
            Some(Value::String(id)) if !id.is_empty() => (Some(id.clone()), None),
            Some(o @ Value::Object(_)) => (s(o, "runId"), s(o, "status")),
            _ => (None, None),
        };
        WaitReply {
            thread_id: s(v, "threadId"),
            status: s(v, "status"),
            timed_out: v.get("timedOut").and_then(Value::as_bool),
            result_run,
            result_status,
        }
    }

    fn class(&self) -> Option<StatusClass> {
        self.status.as_deref().map(classify)
    }

    /// The server said the wait finished (not timed out) on a settled run.
    fn proves_settled(&self) -> bool {
        self.timed_out == Some(false) && self.class() == Some(StatusClass::Settled)
    }
}

/// What a workstream wait plus the read after it prove.
#[derive(Debug, PartialEq)]
enum Proof {
    /// T3's result run is this thread's, no older than the latest run, and
    /// nothing runs now: the whole workstream has settled.
    Proven,
    /// The wait did not finish (timed out, or still active).
    NotYet,
    /// The wait finished on a run of this thread, but a newer run has
    /// started since (a delegated completion's continuation): wait on it.
    Stale(String),
    /// The proof is about something else (another thread's run); it will
    /// never become valid by waiting on it again.
    Rejected(String),
    /// The server finished the wait without `resultRun`.
    Missing,
}

/// `seen_before` is the latest run a read saw before this wait was sent.
fn workstream_proof(thread_id: &str, w: &WaitReply, s: &ThreadState, seen_before: Option<&str>) -> Proof {
    if !w.proves_settled() {
        return Proof::NotYet;
    }
    if w.thread_id.as_deref().is_some_and(|t| t != thread_id) {
        return Proof::Rejected(
            "the delegated-work wait answered for a different thread; not accepted as proof".into(),
        );
    }
    let Some(result) = w.result_run.as_deref() else {
        return if s.latest_run_id.is_none() && s.run_count == Some(0) {
            Proof::Proven
        } else if s.active_run_id.is_some() || s.class() != StatusClass::Settled {
            Proof::Stale("a new run started after the delegated-work wait finished; waiting on it".into())
        } else {
            Proof::Missing
        };
    };
    if run_ordinal_of(thread_id, result).is_none() {
        return Proof::Rejected(format!(
            "resultRun {:?} is not a run of this thread; a child's run never proves the parent's workstream",
            util::sanitize_text(result, 160)
        ));
    }
    if w.result_status
        .as_deref()
        .is_some_and(|st| classify(st) != StatusClass::Settled)
    {
        return Proof::NotYet;
    }
    let idle = s.active_run_id.is_none() && s.class() == StatusClass::Settled;
    let covered = workstream_covered(thread_id, Some(result), s.latest_run_id.as_deref(), s.run_count);
    if covered && idle {
        return Proof::Proven;
    }
    // T3 may name an earlier run as holding the final answer. That is proof
    // for the latest run too when this wait was sent after a read had
    // already seen that run (a wait without runId covers the latest run and
    // its delegated work) and no newer run has started since.
    let latest = s.latest_run_id.as_deref();
    let waited_on_latest = matches!(
        (seen_before, latest),
        (Some(before), Some(now)) if same_run(before, now) && run_ordinal_of(thread_id, now).is_some()
    );
    if waited_on_latest && idle {
        return Proof::Proven;
    }
    Proof::Stale(
        "a newer run of this thread started after the run T3 proved settled (a delegated completion \
         continuing the parent); waiting on it"
            .into(),
    )
}

/// One watch's state, copied once per evaluation so the settled decision and
/// the printed report always describe the same observations.
struct Snap {
    last: Option<Observation>,
    outage: Option<Outage>,
}

/// Bounds live upstream operations (MCP cycles and `t3` runs) across the
/// hub. A permit is held by the thread doing the operation until it has
/// really ended, so a cancelled long-poll still counts until its connection
/// is gone. With `T3_WATCH_MAX_WATCHES=1` there is never more than one
/// request in flight, however many short waits come and go.
struct OpBudget {
    live: Mutex<usize>,
    cv: Condvar,
    max: usize,
    cancelled: AtomicU64,
}

struct Permit(Arc<OpBudget>);

impl Drop for Permit {
    fn drop(&mut self) {
        *lock(&self.0.live) -= 1;
        self.0.cv.notify_all();
    }
}

impl OpBudget {
    /// Waits for a free slot. Gives up (`None`) once `abandoned` says nobody
    /// waits for the operation any more.
    fn acquire(self: &Arc<Self>, abandoned: impl Fn() -> bool) -> Option<Permit> {
        let mut live = lock(&self.live);
        loop {
            if *live < self.max {
                *live += 1;
                return Some(Permit(self.clone()));
            }
            live = self
                .cv
                .wait_timeout(live, Duration::from_millis(100))
                .unwrap_or_else(|e| e.into_inner())
                .0;
            if *live >= self.max {
                drop(live);
                if abandoned() {
                    return None;
                }
                live = lock(&self.live);
            }
        }
    }
}

pub struct Hub {
    watches: Mutex<HashMap<WatchKey, Arc<Watch>>>,
    ops: Arc<OpBudget>,
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

/// Progress a wait reports before it ends (`--follow` failures, upstream
/// outages). The daemon forwards each one to its client as a line.
pub type Progress<'a> = &'a mut dyn FnMut(Value);

/// What one wait has already reported as progress.
#[derive(Default)]
struct Reported {
    failed_runs: HashSet<(usize, String)>,
    failed: Vec<Value>,
    unreachable: HashSet<usize>,
}

impl Hub {
    pub fn new(label: &'static str) -> Arc<Self> {
        Self::with_max_watches(label, env_limit("T3_WATCH_MAX_WATCHES", DEFAULT_MAX_WATCHES))
    }

    pub fn with_max_watches(label: &'static str, max_watches: usize) -> Arc<Self> {
        Arc::new(Hub {
            watches: Mutex::new(HashMap::new()),
            ops: Arc::new(OpBudget {
                live: Mutex::new(0),
                cv: Condvar::new(),
                max: max_watches,
                cancelled: AtomicU64::new(0),
            }),
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
        let marker = req.predicate.final_marker.clone();
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
                        final_marker: marker.clone(),
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
                        final_marker: marker.clone(),
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
                        outage: None,
                        running: false,
                        newest_sub: None,
                        last_observed: None,
                        cycle_started: None,
                    }),
                    cv: Condvar::new(),
                    caches: Mutex::new(Caches::default()),
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

    /// Blocks until the request is satisfied, times out, fails, needs input,
    /// or the client goes away (`ClientGone` on `rx`). Progress (followed
    /// failures, outages) goes to `progress` as it happens.
    pub fn wait(
        self: &Arc<Self>,
        req: &WaitRequest,
        tx: Sender<SubMsg>,
        rx: Receiver<SubMsg>,
        progress: Progress,
    ) -> Outcome {
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

        let mut reported = Reported::default();
        // Outage reporting needs a clock even when nothing is published.
        let tick = if req.notify_unreachable.is_some() {
            Duration::from_millis(500)
        } else {
            Duration::from_secs(3600)
        };
        let outcome = match failure {
            Some(e) => Outcome::Error(e),
            None => loop {
                let snaps = snapshot(&watches);
                self.report_progress(&snaps, req, started, &mut reported, progress);
                if let Some(done) = self.evaluate(&snaps, req, started, &reported) {
                    break done;
                }
                let now = Instant::now();
                if now >= deadline {
                    break Outcome::Timeout(self.report("timeout", &snaps, req, started, None, &reported));
                }
                match rx.recv_timeout((deadline - now).min(tick)) {
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

    /// Emits progress for this snapshot: with `--follow`, each newly failed
    /// run (the wait goes on for a later one); with `--notify-unreachable`,
    /// the start and end of an upstream outage.
    fn report_progress(
        &self,
        snaps: &[Snap],
        req: &WaitRequest,
        started: Instant,
        r: &mut Reported,
        progress: Progress,
    ) {
        for (i, snap) in snaps.iter().enumerate() {
            let t = &req.targets[i];
            if req.predicate.follow {
                if let Some(Observation::State(s)) = &snap.last {
                    if s.observed_at >= started && s.outcome(&req.predicate) == ThreadOutcome::Failed {
                        let run = s.latest_run_id.clone().unwrap_or_default();
                        if r.failed_runs.insert((i, run.clone())) {
                            let mut thread = self.thread_json(snap, t, req, started);
                            thread["followed"] = json!(true);
                            r.failed
                                .push(json!({ "threadId": t.thread_id, "runId": run, "status": s.status }));
                            progress(json!({
                                "event": "run_failed",
                                "progress": true,
                                "via": self.label,
                                "threads": [thread],
                                "note": "this run failed; --follow keeps waiting for a later run of the thread",
                            }));
                        }
                    }
                }
            }
            if let Some(after) = req.notify_unreachable {
                match &snap.outage {
                    Some(o) if o.since.elapsed() >= after && r.unreachable.insert(i) => progress(json!({
                        "event": "upstream_unreachable",
                        "progress": true,
                        "via": self.label,
                        "threads": [{ "machine": t.machine, "threadId": t.thread_id }],
                        "upstream": outage_json(o),
                        "note": "this watcher cannot reach T3 and keeps retrying; nothing is assumed about the thread",
                    })),
                    None if r.unreachable.remove(&i) => progress(json!({
                        "event": "upstream_restored",
                        "progress": true,
                        "via": self.label,
                        "threads": [{ "machine": t.machine, "threadId": t.thread_id }],
                    })),
                    _ => {}
                }
            }
        }
    }

    /// Decides from one snapshot, and builds the report from that same
    /// snapshot, so `event` and the per-thread flags always agree.
    fn evaluate(&self, snaps: &[Snap], req: &WaitRequest, started: Instant, reported: &Reported) -> Option<Outcome> {
        let mut outcomes = Vec::with_capacity(snaps.len());
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
                    return Some(Outcome::Error(self.report(
                        "error",
                        snaps,
                        req,
                        started,
                        Some(err),
                        reported,
                    )));
                }
                Some(Observation::State(s)) if s.observed_at >= started => outcomes.push(s.outcome(&req.predicate)),
                _ => outcomes.push(ThreadOutcome::Pending),
            }
        }
        // A blocked thread will not finish by itself: wake now, in any mode.
        if outcomes.contains(&ThreadOutcome::NeedsInput) {
            return Some(Outcome::NeedsInput(self.report(
                "needs_input",
                snaps,
                req,
                started,
                None,
                reported,
            )));
        }
        let terminal = |o: &ThreadOutcome| match o {
            ThreadOutcome::Completed => true,
            ThreadOutcome::Failed => !req.predicate.follow,
            _ => false,
        };
        let count = outcomes.iter().filter(|o| terminal(o)).count();
        let done = if req.any { count > 0 } else { count == snaps.len() };
        done.then(|| Outcome::Settled(self.report("settled", snaps, req, started, None, reported)))
    }

    fn thread_json(&self, snap: &Snap, t: &TargetSpec, req: &WaitRequest, started: Instant) -> Value {
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
                let outcome = if fresh {
                    s.outcome(&req.predicate)
                } else {
                    ThreadOutcome::Pending
                };
                m.insert("outcome".into(), json!(outcome.as_str()));
                m.insert("settled".into(), json!(outcome.is_terminal()));
                if !fresh {
                    m.insert("stale".into(), json!(true));
                }
            }
            _ => {
                m.insert("status".into(), Value::Null);
                m.insert("outcome".into(), json!("pending"));
                m.insert("settled".into(), json!(false));
            }
        }
        if let Some(o) = &snap.outage {
            m.insert("lastError".into(), json!(o.last_error));
            m.insert("upstream".into(), outage_json(o));
        }
        Value::Object(m)
    }

    fn report(
        &self,
        event: &str,
        snaps: &[Snap],
        req: &WaitRequest,
        started: Instant,
        error: Option<Value>,
        reported: &Reported,
    ) -> Value {
        let threads: Vec<Value> = snaps
            .iter()
            .zip(&req.targets)
            .map(|(snap, t)| self.thread_json(snap, t, req, started))
            .collect();
        let outcome = match event {
            "settled" => {
                let failed = threads
                    .iter()
                    .any(|t| t["outcome"] == "failed" && !req.predicate.follow);
                if failed {
                    "failed"
                } else {
                    "completed"
                }
            }
            other => other,
        };
        let mut out = json!({
            "event": event,
            "outcome": outcome,
            "mode": if req.any { "any" } else { "all" },
            "until": if req.predicate.until_delegated { "workstream" } else { "turn" },
            "elapsedMs": started.elapsed().as_millis() as u64,
            "via": self.label,
            "threads": threads,
        });
        if req.predicate.min_run_ordinal > 0 {
            out["expectMinRunOrdinal"] = json!(req.predicate.min_run_ordinal);
        }
        if req.predicate.final_marker.is_some() {
            out["finalMarkerRequired"] = json!(true);
        }
        if req.predicate.follow {
            out["follow"] = json!(true);
            if !reported.failed.is_empty() {
                out["failedRuns"] = json!(reported.failed);
            }
        }
        if event == "needs_input" {
            out["action"] = json!(
                "A question or approval is waiting. Questions (userOnly false) may be answered with \
                 t3_pending_request; approvals (userOnly true) need the user in T3. To keep waiting \
                 past these requests, run again with --acknowledge <pendingKey>."
            );
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
                    "lastError": g.outage.as_ref().map(|o| o.last_error.clone()),
                    "ageMs": w.created.elapsed().as_millis() as u64,
                    "finalMarker": w.key.final_marker().is_some(),
                });
                if let Some(o) = &g.outage {
                    v["upstream"] = outage_json(o);
                }
                match &w.key {
                    WatchKey::Mcp {
                        origin,
                        until_delegated,
                        ..
                    } => {
                        v["transport"] = json!("mcp");
                        v["origin"] = json!(origin);
                        v["until"] = json!(if *until_delegated { "workstream" } else { "turn" });
                    }
                    WatchKey::Local { base_dir, t3_bin, .. } => {
                        v["transport"] = json!("local");
                        v["until"] = json!("turn");
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
                    "pendingRequest": c.stats.pending_request.load(Ordering::Relaxed),
                    "delegatedProbe": c.stats.delegated_probe.load(Ordering::Relaxed),
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
            "upstreamOps": {
                "live": *lock(&self.ops.live),
                "max": self.ops.max,
                "cancelled": self.ops.cancelled.load(Ordering::Relaxed),
            },
            "watches": watches,
            "machines": machines,
        })
    }
}

fn outage_json(o: &Outage) -> Value {
    json!({
        "reachable": false,
        "unreachableSince": util::rfc3339_ms(o.since_wall_ms),
        "unreachableForMs": o.since.elapsed().as_millis() as u64,
        "consecutiveFailures": o.failures,
        "lastError": o.last_error,
    })
}

fn snapshot(watches: &[Arc<Watch>]) -> Vec<Snap> {
    watches
        .iter()
        .map(|w| {
            let g = lock(&w.inner);
            Snap {
                last: g.last.clone(),
                outage: g.outage.clone(),
            }
        })
        .collect()
}

/// The thread header only: status, runs, counts. See `HEADER_ONLY`.
fn header_args(thread_id: &str, env: Option<&String>) -> Value {
    let mut args = json!({
        "threadId": thread_id,
        "afterPosition": HEADER_ONLY,
        "limit": 1,
        "maxCharsPerItem": 1,
        "runLimit": 1,
    });
    if let Some(env) = env {
        args["environmentId"] = json!(env);
    }
    args
}

fn with_env(mut args: Value, env: Option<&String>) -> Value {
    if let Some(env) = env {
        args["environmentId"] = json!(env);
    }
    args
}

/// Whether two run ids name the same run of the same thread, whatever their
/// percent-encoding.
fn same_run(a: &str, b: &str) -> bool {
    a == b
        || matches!(
            (crate::model::parse_run(a), crate::model::parse_run(b)),
            (Some(x), Some(y)) if x == y
        )
}

/// `--final-marker` reads the final assistant message, which T3 counts as
/// delivering a delegated child's result. Refuse it on child threads.
fn refuse_marker_on_child(state: &ThreadState) -> Result<(), CallError> {
    if state.parent_thread_id.is_some() {
        return Err(CallError::Unsupported(
            "--final-marker would read this delegated child thread's final result, which T3 then \
             marks as delivered; watch the parent thread instead"
                .into(),
        ));
    }
    Ok(())
}

/// Checks `marker` against the last assistant message of `run` in a local
/// `t3 thread show --messages` answer.
fn marker_in_local_messages(v: &Value, run: &str, marker: &str) -> bool {
    v.get("messages")
        .and_then(Value::as_array)
        .and_then(|msgs| {
            msgs.iter().rev().find(|m| {
                m.get("role").and_then(Value::as_str) == Some("assistant")
                    && m.get("runId").and_then(Value::as_str).is_some_and(|r| same_run(r, run))
            })
        })
        .and_then(|m| m.get("text").and_then(Value::as_str))
        .is_some_and(|t| t.contains(marker))
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

    fn notify_subs(g: &WatchInner) {
        for s in &g.subs {
            let _ = s.tx.send(SubMsg::Update);
        }
    }

    fn publish(&self, obs: Observation) {
        let mut g = lock(&self.inner);
        if let Observation::State(s) = &obs {
            g.outage = None;
            g.last_observed = Some(s.observed_at);
        }
        g.last = Some(obs);
        Self::notify_subs(&g);
    }

    fn note_outage(&self, message: String) {
        let mut g = lock(&self.inner);
        match &mut g.outage {
            Some(o) => {
                o.failures += 1;
                o.last_error = message;
            }
            None => {
                g.outage = Some(Outage {
                    since: Instant::now(),
                    since_wall_ms: util::now_ms(),
                    failures: 1,
                    last_error: message,
                })
            }
        }
        Self::notify_subs(&g);
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

    /// Runs one upstream cycle on a helper thread and waits for it. When the
    /// last subscriber leaves, the cycle is cancelled: its HTTP agent is shut
    /// down (interrupting a blocked long-poll), the server gets
    /// `notifications/cancelled`, and a local `t3` is killed. The helper's
    /// budget permit is released only when the operation has really ended.
    /// `None` means the cycle was abandoned.
    fn cycle_abandonable(self: &Arc<Self>, hub: &Hub) -> Option<Result<Cycle, CallError>> {
        let permit = hub.ops.acquire(|| lock(&self.inner).subs.is_empty())?;
        let cancel = Cancel::new();
        let (tx, rx) = mpsc::channel();
        let (me, token) = (self.clone(), cancel.clone());
        let spawned = util::spawn_thread("t3-watch-call", 256 * 1024, move || {
            let _permit = permit;
            let result = me.cycle(&token);
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
                drop(g);
                cancel.cancel();
                hub.ops.cancelled.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn cycle(&self, cancel: &Cancel) -> Result<Cycle, CallError> {
        match &self.backend {
            Backend::Mcp(client) => self.mcp_cycle(client, cancel),
            Backend::Local { t3_bin, base_dir } => {
                let bin = t3_bin
                    .clone()
                    .or_else(|| config::find_t3_in_base(base_dir))
                    .ok_or_else(|| CallError::Transient {
                        message: format!("no t3 CLI under {}", base_dir.display()),
                        status: None,
                    })?;
                self.upstream_calls.fetch_add(1, Ordering::Relaxed);
                let marker = self.key.final_marker();
                let messages = if marker.is_some() { 3 } else { 0 };
                let (mut state, raw) = local::show(&bin, base_dir, self.key.thread_id(), messages, cancel)?;
                if let Some(marker) = marker {
                    if success_status(&state) {
                        refuse_marker_on_child(&state)?;
                        state.marker_seen = Some(
                            state
                                .latest_run_id
                                .as_deref()
                                .is_some_and(|run| marker_in_local_messages(&raw, run, marker)),
                        );
                    }
                }
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
            let Some(result) = self.cycle_abandonable(hub) else {
                continue;
            };
            let delay = match result {
                Ok(cycle) => {
                    failures = 0;
                    self.publish(Observation::State(Box::new(cycle.state)));
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
                // Only an abandoned cycle is cancelled; the loop then exits.
                Err(CallError::Cancelled) => Duration::ZERO,
                Err(CallError::Transient { message, status } | CallError::Session { message, status }) => {
                    if let Backend::Mcp(c) = &self.backend {
                        c.stats.retries.fetch_add(1, Ordering::Relaxed);
                    }
                    if matches!(status, Some(504 | 524)) {
                        // Cloudflare drops responses idle for ~100s; shorten
                        // long polls on this watch so they finish in time.
                        self.slice_ms.fetch_min(90_000, Ordering::Relaxed);
                    }
                    self.note_outage(message);
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

    /// One `t3_thread_wait` long-poll, then a header-only `t3_thread_read`
    /// (always: a pending request can block a run that still says
    /// `running`), then whatever the observation needs: the workstream
    /// proof, pending request details, the turn-mode delegated-work hint,
    /// the final marker.
    fn mcp_cycle(&self, client: &McpClient, cancel: &Cancel) -> Result<Cycle, CallError> {
        let WatchKey::Mcp {
            thread_id,
            environment_id,
            until_delegated,
            final_marker,
            ..
        } = &self.key
        else {
            unreachable!("MCP backend with a local key");
        };
        let env = environment_id.as_ref();
        // One agent per cycle: cancelling shuts it down and interrupts
        // whichever request of this cycle is in flight.
        let agent = client.agent();
        if !cancel.add_agent(&agent) {
            return Err(CallError::Cancelled);
        }
        let slice_ms = self.slice_ms.load(Ordering::Relaxed);
        let mut args = with_env(json!({ "threadId": thread_id, "timeoutMs": slice_ms }), env);
        if *until_delegated {
            args["untilDelegatedWorkSettled"] = json!(true);
        }
        self.upstream_calls.fetch_add(1, Ordering::Relaxed);
        let raw = client.call_tool(
            &agent,
            cancel,
            "t3_thread_wait",
            args,
            Duration::from_millis(slice_ms) + Duration::from_secs(30),
        )?;
        let w = WaitReply::parse(&raw);
        let seen_before = lock(&self.caches).seen_latest.clone();

        let read_started = Instant::now();
        let r = client.call_tool(
            &agent,
            cancel,
            "t3_thread_read",
            header_args(thread_id, env),
            Duration::from_secs(30),
        )?;
        let mut state = ThreadState::from_thread_json(&r, thread_id, read_started);
        lock(&self.caches).seen_latest = state.latest_run_id.clone();
        let mut next = if w.timed_out == Some(true) {
            Next::Immediately
        } else {
            Next::Gently
        };

        if *until_delegated {
            state.result_run_id = w.result_run.clone();
            match workstream_proof(thread_id, &w, &state, seen_before.as_deref()) {
                Proof::Proven => state.delegated_settled = true,
                Proof::NotYet => {}
                Proof::Stale(note) => {
                    state.proof_note = Some(note);
                    next = Next::Rewait;
                }
                Proof::Rejected(note) => state.proof_note = Some(note),
                // A blocked thread is reported as needs-input either way.
                Proof::Missing if state.needs_input() => {}
                Proof::Missing => {
                    return Err(CallError::Unsupported(
                        "T3 finished the delegated-work wait without resultRun, so the whole workstream \
                         cannot be proven settled; refusing to report completion. Update T3, or use \
                         --until turn with --final-marker"
                            .into(),
                    ))
                }
            }
        }

        if state.needs_input() {
            self.fill_pending(client, &agent, cancel, &mut state, env)?;
        }

        let settled_ok = success_status(&state) && state.active_run_id.is_none();
        if !*until_delegated && settled_ok {
            state.delegated_pending_hint = self.probe_delegated(client, &agent, cancel, &state, env)?;
        }
        if let Some(marker) = final_marker {
            if settled_ok && (!*until_delegated || state.delegated_settled) {
                refuse_marker_on_child(&state)?;
                if let Some(run) = state.deciding_run().map(str::to_string) {
                    state.marker_seen = Some(self.marker_seen(client, &agent, cancel, &state, &run, marker, env)?);
                }
            }
        }
        Ok(Cycle { state, next })
    }

    /// Fills in which questions or approvals are pending, from the end of
    /// the activity timeline and `t3_pending_request list` (read-only;
    /// approvals are not listed there). Failures leave the details empty:
    /// the count alone still wakes the caller.
    fn fill_pending(
        &self,
        client: &McpClient,
        agent: &ureq::Agent,
        cancel: &Cancel,
        state: &mut ThreadState,
        env: Option<&String>,
    ) -> Result<(), CallError> {
        let key = format!(
            "{:?}|{:?}|{:?}|{:?}",
            state.item_count, state.pending_requests, state.status, state.latest_run_id
        );
        if let Some((k, requests, ids)) = &lock(&self.caches).pending {
            if *k == key {
                state.requests = requests.clone();
                state.pending_ids = ids.clone();
                return Ok(());
            }
        }
        let mut complete = true;
        let list = with_env(json!({ "action": "list", "threadId": state.thread_id }), env);
        match client.call_tool(agent, cancel, "t3_pending_request", list, Duration::from_secs(30)) {
            Ok(v) => {
                state.pending_ids = v
                    .get("requestIds")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .take(32)
                            .map(|s| util::sanitize_text(s, 200))
                            .collect()
                    })
                    .unwrap_or_default();
            }
            Err(CallError::Cancelled) => return Err(CallError::Cancelled),
            Err(_) => complete = false,
        }
        // A finished child's timeline holds its final result; leave it unread.
        let finished_child = state.parent_thread_id.is_some() && success_status(state);
        if !finished_child {
            let mut args = json!({
                "threadId": state.thread_id,
                "view": "activity",
                "limit": 40,
                "maxCharsPerItem": 4000,
            });
            if let Some(n) = state.item_count.filter(|n| *n > 40) {
                args["afterPosition"] = json!(n - 41);
            }
            match client.call_tool(
                agent,
                cancel,
                "t3_thread_read",
                with_env(args, env),
                Duration::from_secs(30),
            ) {
                Ok(v) => {
                    let items = v.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
                    state.requests = pending_from_activity(&items);
                }
                Err(CallError::Cancelled) => return Err(CallError::Cancelled),
                Err(_) => complete = false,
            }
        }
        if complete {
            lock(&self.caches).pending = Some((key, state.requests.clone(), state.pending_ids.clone()));
        }
        Ok(())
    }

    /// Turn mode only: asks T3, without waiting, whether delegated work the
    /// finished turn started is still running. A hint for the caller.
    fn probe_delegated(
        &self,
        client: &McpClient,
        agent: &ureq::Agent,
        cancel: &Cancel,
        state: &ThreadState,
        env: Option<&String>,
    ) -> Result<Option<bool>, CallError> {
        let Some(run) = state.latest_run_id.clone() else {
            return Ok(None);
        };
        if let Some((r, pending)) = &lock(&self.caches).probe {
            // Pending work only ever becomes settled, so re-ask until it is.
            if *r == run && !*pending {
                return Ok(Some(false));
            }
        }
        let args = with_env(
            json!({ "threadId": state.thread_id, "timeoutMs": PROBE_TIMEOUT_MS, "untilDelegatedWorkSettled": true }),
            env,
        );
        match client.call_tool(agent, cancel, "t3_thread_wait", args, Duration::from_secs(30)) {
            Ok(v) => {
                let w = WaitReply::parse(&v);
                let pending = w.timed_out == Some(true) || w.class() == Some(StatusClass::Active);
                lock(&self.caches).probe = Some((run, pending));
                Ok(Some(pending))
            }
            Err(CallError::Cancelled) => Err(CallError::Cancelled),
            Err(_) => Ok(None),
        }
    }

    /// Whether the final assistant message of `run` contains `marker`. Finds
    /// the message with a 1-character listing of the last 100 timeline
    /// items, then pages through that one message only.
    #[allow(clippy::too_many_arguments)]
    fn marker_seen(
        &self,
        client: &McpClient,
        agent: &ureq::Agent,
        cancel: &Cancel,
        state: &ThreadState,
        run: &str,
        marker: &str,
        env: Option<&String>,
    ) -> Result<bool, CallError> {
        let key = format!("{run}|{:?}", state.item_count);
        if let Some((k, seen)) = &lock(&self.caches).marker {
            if *k == key {
                return Ok(*seen);
            }
        }
        let mut args = json!({
            "threadId": state.thread_id,
            "view": "messages",
            "limit": 100,
            "maxCharsPerItem": 1,
        });
        if let Some(n) = state.item_count.filter(|n| *n > 100) {
            args["afterPosition"] = json!(n - 101);
        }
        let mut last: Option<String> = None;
        for _ in 0..5 {
            let v = client.call_tool(
                agent,
                cancel,
                "t3_thread_read",
                with_env(args.clone(), env),
                Duration::from_secs(30),
            )?;
            for item in v.get("items").and_then(Value::as_array).into_iter().flatten() {
                let is_final = item.get("type").and_then(Value::as_str) == Some("assistant_message")
                    && item
                        .get("runId")
                        .and_then(Value::as_str)
                        .is_some_and(|r| same_run(r, run));
                if is_final {
                    last = item.get("itemId").and_then(Value::as_str).map(str::to_string);
                }
            }
            match (
                v.get("hasMore").and_then(Value::as_bool),
                v.get("nextPosition").and_then(Value::as_u64),
            ) {
                (Some(true), Some(next)) => args["afterPosition"] = json!(next),
                _ => break,
            }
        }
        let seen = match last {
            None => false,
            Some(item_id) => {
                let mut offset = 0u64;
                let mut tail = String::new();
                let mut found = false;
                for _ in 0..40 {
                    let page = with_env(
                        json!({ "threadId": state.thread_id, "itemId": item_id, "textOffset": offset, "maxCharsPerItem": 50_000 }),
                        env,
                    );
                    let v = client.call_tool(agent, cancel, "t3_thread_read", page, Duration::from_secs(30))?;
                    let Some(item) = v.get("items").and_then(Value::as_array).and_then(|a| a.first()) else {
                        break;
                    };
                    // Keep the end of the previous page so a marker split
                    // across pages still matches.
                    let text = format!("{tail}{}", item.get("text").and_then(Value::as_str).unwrap_or(""));
                    if text.contains(marker) {
                        found = true;
                        break;
                    }
                    let keep = text
                        .char_indices()
                        .rev()
                        .nth(marker.chars().count())
                        .map_or(0, |(i, _)| i);
                    tail = text[keep..].to_string();
                    match (
                        item.get("textTruncated").and_then(Value::as_bool),
                        item.get("nextTextOffset").and_then(Value::as_u64),
                    ) {
                        (Some(true), Some(next)) if next > offset => offset = next,
                        _ => break,
                    }
                }
                found
            }
        };
        lock(&self.caches).marker = Some((key, seen));
        Ok(seen)
    }
}

fn success_status(s: &ThreadState) -> bool {
    s.status.as_deref().is_some_and(|st| SUCCESS_STATUSES.contains(&st))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const T: &str = "mcp:launch:parent";

    fn run(thread: &str, n: u64) -> String {
        format!("run:thread:{}:ordinal:{n}", util::pct_encode(thread))
    }

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
            notify_unreachable: None,
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

    fn read(v: Value) -> ThreadState {
        ThreadState::from_thread_json(&v, T, Instant::now())
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
            &json!({"timedOut": false, "status": "completed", "runId": "a", "resultRun": {"runId": "b", "status": "completed"}}),
        );
        assert!(w.proves_settled());
        assert_eq!(w.result_run.as_deref(), Some("b"));
        assert_eq!(w.result_status.as_deref(), Some("completed"));
    }

    #[test]
    fn workstream_proof_is_strict() {
        let settled = |result: Value| {
            WaitReply::parse(&json!({"threadId": T, "timedOut": false, "status": "completed", "resultRun": result}))
        };
        let thread6 = read(json!({"threadId": T, "status": "completed", "latestRunId": run(T, 6), "runCount": 6}));
        // The research incident: run 6 is the synthesis after the reviewers.
        assert_eq!(
            workstream_proof(
                T,
                &settled(json!({"runId": run(T, 6), "status": "completed"})),
                &thread6,
                None
            ),
            Proof::Proven
        );
        // Run 2 settled, but the read already shows run 6: stale, wait again.
        assert!(matches!(
            workstream_proof(
                T,
                &settled(json!({"runId": run(T, 2), "status": "completed"})),
                &thread6,
                None
            ),
            Proof::Stale(_)
        ));
        // A child's run is never proof for the parent.
        assert!(matches!(
            workstream_proof(
                T,
                &settled(json!({"runId": run("mcp:launch:child", 99), "status": "completed"})),
                &thread6,
                None
            ),
            Proof::Rejected(_)
        ));
        // An answer about another thread is not proof either.
        let other = WaitReply::parse(&json!({"threadId": "x", "timedOut": false, "status": "completed",
                                             "resultRun": {"runId": run(T, 6), "status": "completed"}}));
        assert!(matches!(
            workstream_proof(T, &other, &thread6, None),
            Proof::Rejected(_)
        ));
        // No resultRun at all: fail closed.
        let bare = WaitReply::parse(&json!({"timedOut": false, "status": "completed", "runId": run(T, 6)}));
        assert_eq!(workstream_proof(T, &bare, &thread6, None), Proof::Missing);
        // A result run still running, or a timed-out wait, proves nothing yet.
        assert_eq!(
            workstream_proof(
                T,
                &settled(json!({"runId": run(T, 6), "status": "running"})),
                &thread6,
                None
            ),
            Proof::NotYet
        );
        let timed_out = WaitReply::parse(&json!({"timedOut": true, "status": "running"}));
        assert_eq!(workstream_proof(T, &timed_out, &thread6, None), Proof::NotYet);
        // The read shows a run in progress (a late continuation): stale.
        let active = read(json!({"threadId": T, "status": "running", "latestRunId": run(T, 6),
                                 "activeRunId": run(T, 6), "runCount": 6}));
        assert!(matches!(
            workstream_proof(
                T,
                &settled(json!({"runId": run(T, 6), "status": "completed"})),
                &active,
                None
            ),
            Proof::Stale(_)
        ));
        // T3 names run 4 as the answer while run 6 is the latest: only a wait
        // sent after run 6 was seen proves run 6's workstream.
        let older = settled(json!({"runId": run(T, 4), "status": "completed"}));
        assert!(matches!(workstream_proof(T, &older, &thread6, None), Proof::Stale(_)));
        assert!(matches!(
            workstream_proof(T, &older, &thread6, Some(&run(T, 5))),
            Proof::Stale(_)
        ));
        assert_eq!(workstream_proof(T, &older, &thread6, Some(&run(T, 6))), Proof::Proven);
        assert!(matches!(
            workstream_proof(T, &older, &active, Some(&run(T, 6))),
            Proof::Stale(_)
        ));
        // A thread with no runs at all is trivially settled.
        let empty = read(json!({"threadId": T, "status": "idle", "runCount": 0}));
        assert_eq!(
            workstream_proof(
                T,
                &WaitReply::parse(&json!({"timedOut": false, "status": "idle"})),
                &empty,
                None
            ),
            Proof::Proven
        );
    }

    #[test]
    fn local_marker_reads_the_last_assistant_message_of_the_run() {
        let v = json!({"messages": [
            {"role": "assistant", "runId": run(T, 2), "text": "FINAL from the old run"},
            {"role": "user", "runId": run(T, 3), "text": "FINAL please"},
            {"role": "assistant", "runId": run(T, 3), "text": "working"},
            {"role": "assistant", "runId": run(T, 3), "text": "all done FINAL"},
        ]});
        assert!(marker_in_local_messages(&v, &run(T, 3), "FINAL"));
        assert!(!marker_in_local_messages(&v, &run(T, 3), "MISSING"));
        let no_final = json!({"messages": [{"role": "assistant", "runId": run(T, 3), "text": "working"}]});
        assert!(!marker_in_local_messages(&no_final, &run(T, 3), "FINAL"));
        // Percent-encoding differences do not matter.
        assert!(same_run("run:thread:mcp:launch:parent:ordinal:3", &run(T, 3)));
    }

    #[test]
    fn requests_are_clamped_and_validated() {
        let base = std::env::temp_dir();
        let mut r = request(vec![local_target(&base, None, "t")]);
        r.interval = Duration::ZERO;
        r.slice = Duration::from_secs(86_400);
        r.predicate.final_marker = Some("DONE".into());
        r.predicate.acknowledged = Some("pk-0123".into());
        r.predicate.follow = true;
        let r = r.normalized();
        assert_eq!(r.interval, MIN_INTERVAL);
        assert_eq!(r.slice, MAX_SLICE);
        let parsed = WaitRequest::from_json(&r.to_json()).unwrap();
        assert_eq!(parsed.slice, MAX_SLICE);
        assert_eq!(parsed.predicate, r.predicate);

        let mut delegated = request(vec![local_target(&base, None, "t")]);
        delegated.predicate.until_delegated = true;
        assert!(delegated.validate().unwrap_err().contains("mcp transport"));
        assert!(WaitRequest::from_json(&delegated.to_json()).is_err());

        let mut bad_marker = request(vec![local_target(&base, None, "t")]);
        bad_marker.predicate.final_marker = Some(" ".into());
        assert!(bad_marker.validate().is_err());
        bad_marker.predicate.final_marker = Some("a\nb".into());
        assert!(bad_marker.validate().is_err());
        let mut bad_ack = request(vec![local_target(&base, None, "t")]);
        bad_ack.predicate.acknowledged = Some("whatever".into());
        assert!(bad_ack.validate().is_err());

        let relative = request(vec![local_target(std::path::Path::new("rel"), None, "t")]);
        assert!(relative.validate().is_err());
        let bad_bin = request(vec![local_target(&base, Some("t3".into()), "t")]);
        assert!(bad_bin.validate().is_err());
        assert!(request(vec![local_target(&base, None, "a\nb")]).validate().is_err());
        assert!(request(vec![]).validate().is_err());
    }

    #[test]
    fn outcomes_from_json_never_upgrade_to_success() {
        // A settled result without `outcome` (an older daemon) is not success.
        let old = Outcome::from_json(json!({"event": "settled", "threads": []}));
        assert_eq!(old.exit_code(), EXIT_ERROR);
        let failed = Outcome::from_json(json!({"event": "settled", "outcome": "failed"}));
        assert_eq!(failed.exit_code(), EXIT_FAILED);
        let ok = Outcome::from_json(json!({"event": "settled", "outcome": "completed"}));
        assert_eq!(ok.exit_code(), EXIT_COMPLETED);
        let input = Outcome::from_json(json!({"event": "needs_input", "outcome": "needs_input"}));
        assert_eq!(input.exit_code(), EXIT_NEEDS_INPUT);
        assert_eq!(Outcome::from_json(json!({"event": "nonsense"})).exit_code(), EXIT_ERROR);
    }

    fn snap(s: ThreadState) -> Snap {
        Snap {
            last: Some(Observation::State(Box::new(s))),
            outage: None,
        }
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
            &json!({"status": "completed", "latestRunId": "run:thread:a:ordinal:1"}),
            "a",
            Instant::now(),
        );
        let busy = ThreadState::from_thread_json(&json!({"status": "running"}), "b", Instant::now());
        let r = Reported::default();
        let mut busy_snap = snap(busy);
        busy_snap.outage = Some(Outage {
            since: Instant::now(),
            since_wall_ms: util::now_ms(),
            failures: 2,
            last_error: "net".into(),
        });
        let snaps = vec![snap(done.clone()), busy_snap];
        assert!(hub.evaluate(&snaps, &req, started, &r).is_none(), "mode all needs both");
        let timeout = hub.report("timeout", &snaps, &req, started, None, &r);
        assert_eq!(timeout["threads"][1]["upstream"]["reachable"], false);
        assert_eq!(timeout["threads"][1]["lastError"], "net");
        let snaps = vec![snap(done.clone()), snap(done)];
        let Some(Outcome::Settled(v)) = hub.evaluate(&snaps, &req, started, &r) else {
            panic!("expected settled");
        };
        assert_eq!(v["outcome"], "completed");
        assert!(v["threads"].as_array().unwrap().iter().all(|t| t["settled"] == true));
    }

    #[test]
    fn needs_input_wins_and_failure_is_not_success() {
        let hub = Hub::new("test");
        let mut req = request(vec![
            local_target(std::path::Path::new("/x"), None, "a"),
            local_target(std::path::Path::new("/x"), None, "b"),
        ]);
        let started = Instant::now();
        let state = |id: &str, v: Value| ThreadState::from_thread_json(&v, id, Instant::now());
        let done = state(
            "a",
            json!({"status": "completed", "latestRunId": "run:thread:a:ordinal:1"}),
        );
        let failed = state(
            "b",
            json!({"status": "failed", "latestRunId": "run:thread:b:ordinal:1"}),
        );
        let blocked = state(
            "b",
            json!({"status": "running", "latestRunId": "run:thread:b:ordinal:1", "pendingRequestCount": 1}),
        );
        let r = Reported::default();
        let Some(Outcome::Settled(v)) = hub.evaluate(&[snap(done.clone()), snap(failed.clone())], &req, started, &r)
        else {
            panic!("expected settled");
        };
        assert_eq!(v["outcome"], "failed");
        assert_eq!(Outcome::Settled(v).exit_code(), EXIT_FAILED);
        // With --mode any, a blocked thread still wakes at once.
        req.any = true;
        let Some(Outcome::NeedsInput(v)) = hub.evaluate(&[snap(done.clone()), snap(blocked)], &req, started, &r) else {
            panic!("expected needs_input");
        };
        assert_eq!(v["threads"][1]["outcome"], "needs_input");
        assert!(v["action"].as_str().unwrap().contains("approvals"));
        // --follow: a failed run is progress, not the end.
        req.any = false;
        req.predicate.follow = true;
        assert!(hub.evaluate(&[snap(done), snap(failed)], &req, started, &r).is_none());
    }

    #[test]
    fn follow_reports_each_failed_run_once() {
        let hub = Hub::new("test");
        let mut req = request(vec![local_target(std::path::Path::new("/x"), None, T)]);
        req.predicate.follow = true;
        let started = Instant::now();
        let mut events = Vec::new();
        let mut r = Reported::default();
        let failed5 = snap(read(json!({"status": "failed", "latestRunId": run(T, 5)})));
        hub.report_progress(&[failed5], &req, started, &mut r, &mut |v| events.push(v));
        let failed5 = snap(read(json!({"status": "failed", "latestRunId": run(T, 5)})));
        hub.report_progress(&[failed5], &req, started, &mut r, &mut |v| events.push(v));
        let failed6 = snap(read(json!({"status": "interrupted", "latestRunId": run(T, 6)})));
        hub.report_progress(&[failed6], &req, started, &mut r, &mut |v| events.push(v));
        assert_eq!(events.len(), 2);
        assert!(events
            .iter()
            .all(|e| e["event"] == "run_failed" && e["progress"] == true));
        assert_eq!(r.failed.len(), 2);
    }

    #[test]
    fn worker_spawn_failure_reaches_the_subscriber() {
        let (dir, bin) = script("spawn", "running");
        let hub = Hub::new("test");
        let req = request(vec![local_target(&dir, Some(bin), "t")]);
        let (tx, rx) = mpsc::channel();
        util::FAIL_SPAWNS.with(|c| c.set((0, 1)));
        let started = Instant::now();
        let outcome = hub.wait(&req, tx, rx, &mut |_| {});
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
        let Outcome::Error(v) = hub.wait(&req, tx, rx, &mut |_| {}) else {
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
        assert!(matches!(hub.wait(&req, tx, rx, &mut |_| {}), Outcome::Timeout(_)));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !lock(&hub.watches).is_empty() {
            assert!(Instant::now() < deadline, "watch still registered while its call hangs");
            std::thread::sleep(Duration::from_millis(10));
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
