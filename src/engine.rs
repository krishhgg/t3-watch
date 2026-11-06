//! The watch engine shared by the daemon and `--no-daemon` mode.
//!
//! A `Hub` owns one `Watch` per (machine, thread, environment, delegated)
//! key. Each watch has one worker thread that talks upstream; every waiting
//! client subscribes to the watches it needs and evaluates its own condition
//! (`mode`, `expect_min_run_ordinal`) against their shared observations. So
//! 20 clients on one thread cost one upstream long-poll, not 20.

use crate::config::{self, Config};
use crate::local;
use crate::mcp::{CallError, McpClient};
use crate::model::{is_active, Predicate, ThreadState};
use crate::util::{lock, Jitter};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const LOCAL_MACHINE: &str = "local";

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
    pub t3_bin: Option<PathBuf>,
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
                transport: if s(t, "transport").as_deref() == Some("local") {
                    Transport::Local
                } else {
                    Transport::Mcp
                },
                machine: s(t, "machine").ok_or("target missing machine")?,
                thread_id: s(t, "threadId").ok_or("target missing threadId")?,
                environment_id: s(t, "environmentId"),
                t3_bin: s(t, "t3Bin").map(PathBuf::from),
                base_dir: s(t, "baseDir").map(PathBuf::from),
            });
        }
        if targets.is_empty() {
            return Err("no threads to watch".into());
        }
        Ok(WaitRequest {
            targets,
            any: s(v, "mode").as_deref() == Some("any"),
            predicate: Predicate {
                min_run_ordinal: v.get("expectMinRunOrdinal").and_then(Value::as_u64).unwrap_or(0),
                until_delegated: v.get("untilDelegatedSettled").and_then(Value::as_bool).unwrap_or(false),
            },
            timeout: ms("timeoutMs", 4 * 3_600_000),
            interval: ms("intervalMs", 5_000).max(Duration::from_millis(50)),
            slice: ms("sliceMs", 240_000).max(Duration::from_millis(100)),
        })
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
struct WatchKey {
    machine: String,
    thread_id: String,
    environment_id: Option<String>,
    until_delegated: bool,
    /// Local watches with a different data dir or `--t3-bin` must not share
    /// a worker.
    local: Option<(PathBuf, Option<PathBuf>)>,
}

enum Backend {
    Mcp(Arc<McpClient>),
    /// `t3_bin` is the explicit `--t3-bin`; otherwise the newest install is
    /// looked up on every poll, so a T3 auto-update mid-wait is picked up.
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

pub struct Hub {
    watches: Mutex<HashMap<WatchKey, Arc<Watch>>>,
    clients: Mutex<HashMap<String, Arc<McpClient>>>,
    active_waits: AtomicUsize,
    next_sub: AtomicU64,
    idle_since: Mutex<Option<Instant>>,
    started: Instant,
    pub label: &'static str,
}

impl Hub {
    pub fn new(label: &'static str) -> Arc<Self> {
        Arc::new(Hub {
            watches: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            active_waits: AtomicUsize::new(0),
            next_sub: AtomicU64::new(1),
            idle_since: Mutex::new(Some(Instant::now())),
            started: Instant::now(),
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

    fn mcp_client(&self, name: &str, cfg: &Config) -> Result<Arc<McpClient>, Value> {
        let m = cfg.machines.get(name).ok_or_else(|| {
            error_json(
                "config",
                &format!("no saved machine named {name:?}; run `t3-watch auth login <origin>` first"),
            )
        })?;
        if m.access_token.is_empty() {
            return Err(error_json(
                "auth",
                &format!("machine {name:?} has no token; run `t3-watch auth login {}`", m.origin),
            ));
        }
        let endpoint = format!("{}/mcp", m.origin.trim_end_matches('/'));
        let existing = lock(&self.clients)
            .get(name)
            .filter(|c| c.endpoint() == endpoint)
            .cloned();
        if let Some(c) = existing {
            // Outside the clients lock: picks up a token from a fresh login.
            c.set_token(&m.access_token);
            return Ok(c);
        }
        let c = Arc::new(McpClient::new(name, &m.origin, &m.access_token));
        lock(&self.clients).insert(name.to_string(), c.clone());
        Ok(c)
    }

    fn resolve(&self, req: &WaitRequest) -> Result<Vec<(WatchKey, Backend)>, Value> {
        let mut cfg: Option<Config> = None;
        let mut out = Vec::new();
        for t in &req.targets {
            let backend = match t.transport {
                Transport::Mcp => {
                    if cfg.is_none() {
                        cfg = Some(Config::load().map_err(|e| error_json("config", &e))?);
                    }
                    Backend::Mcp(self.mcp_client(&t.machine, cfg.as_ref().unwrap())?)
                }
                Transport::Local => {
                    let base_dir = t.base_dir.clone().unwrap_or_else(config::t3_base_dir);
                    match &t.t3_bin {
                        Some(bin) if !bin.is_file() => {
                            return Err(error_json(
                                "config",
                                &format!("--t3-bin {} does not exist", bin.display()),
                            ))
                        }
                        None if config::find_t3_bin(&base_dir).is_none() => {
                            return Err(error_json("config", "cannot find T3's `t3` CLI; pass --t3-bin"))
                        }
                        _ => {}
                    }
                    Backend::Local {
                        t3_bin: t.t3_bin.clone(),
                        base_dir,
                    }
                }
            };
            let key = WatchKey {
                machine: t.machine.clone(),
                thread_id: t.thread_id.clone(),
                environment_id: t.environment_id.clone(),
                until_delegated: req.predicate.until_delegated && t.transport == Transport::Mcp,
                local: match &backend {
                    Backend::Local { t3_bin, base_dir } => Some((base_dir.clone(), t3_bin.clone())),
                    Backend::Mcp(_) => None,
                },
            };
            out.push((key, backend));
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
    ) -> Arc<Watch> {
        let mut map = lock(&self.watches);
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
        drop(map);
        w.cv.notify_all();
        if spawn {
            let (hub, watch) = (self.clone(), w.clone());
            let spawned = thread::Builder::new()
                .name("t3-watch-worker".into())
                .stack_size(256 * 1024)
                .spawn(move || watch.run(&hub));
            if spawned.is_err() {
                lock(&w.inner).running = false;
            }
        }
        w
    }

    /// Blocks until the request is satisfied, times out, fails, or the
    /// client goes away (`ClientGone` on `rx`).
    pub fn wait(self: &Arc<Self>, req: &WaitRequest, tx: Sender<SubMsg>, rx: Receiver<SubMsg>) -> Outcome {
        let started = Instant::now();
        let deadline = started + req.timeout;
        let resolved = match self.resolve(req) {
            Ok(r) => r,
            Err(e) => return Outcome::Error(e),
        };
        self.active_waits.fetch_add(1, Ordering::SeqCst);
        let sub = self.next_sub.fetch_add(1, Ordering::Relaxed);
        let watches: Vec<Arc<Watch>> = resolved
            .into_iter()
            .map(|(key, backend)| self.subscribe(key, backend, req, sub, tx.clone()))
            .collect();
        drop(tx);

        let outcome = loop {
            if let Some(done) = self.evaluate(&watches, req, started) {
                break done;
            }
            let now = Instant::now();
            if now >= deadline {
                break Outcome::Timeout(self.report("timeout", &watches, req, started, None));
            }
            match rx.recv_timeout(deadline - now) {
                Ok(SubMsg::Update) => {}
                Ok(SubMsg::ClientGone) | Err(RecvTimeoutError::Disconnected) => break Outcome::Cancelled,
                Err(RecvTimeoutError::Timeout) => {}
            }
        };

        for w in &watches {
            w.unsubscribe(sub);
        }
        if self.active_waits.fetch_sub(1, Ordering::SeqCst) == 1 {
            *lock(&self.idle_since) = Some(Instant::now());
        }
        outcome
    }

    fn evaluate(&self, watches: &[Arc<Watch>], req: &WaitRequest, started: Instant) -> Option<Outcome> {
        let mut settled = 0;
        for w in watches {
            // Bind first: a guard in the match scrutinee would live through
            // the arms, and `report` locks the same mutex.
            let last = lock(&w.inner).last.clone();
            match last {
                Some(Observation::Failed { error, at }) if at >= started => {
                    let err = json!({
                        "kind": error.kind(),
                        "message": error.message(),
                        "machine": w.key.machine,
                        "threadId": w.key.thread_id,
                    });
                    return Some(Outcome::Error(self.report("error", watches, req, started, Some(err))));
                }
                Some(Observation::State(s)) if s.observed_at >= started && s.satisfies(&w.predicate(req)) => {
                    settled += 1
                }
                _ => {}
            }
        }
        let done = if req.any { settled > 0 } else { settled == watches.len() };
        done.then(|| Outcome::Settled(self.report("settled", watches, req, started, None)))
    }

    fn report(
        &self,
        event: &str,
        watches: &[Arc<Watch>],
        req: &WaitRequest,
        started: Instant,
        error: Option<Value>,
    ) -> Value {
        let threads: Vec<Value> = watches
            .iter()
            .map(|w| {
                let g = lock(&w.inner);
                let mut m = Map::new();
                m.insert("machine".into(), json!(w.key.machine));
                m.insert("threadId".into(), json!(w.key.thread_id));
                if let Some(env) = &w.key.environment_id {
                    m.insert("environmentId".into(), json!(env));
                }
                match &g.last {
                    Some(Observation::State(s)) => {
                        let pred = w.predicate(req);
                        if let Value::Object(fields) = s.to_json(&pred) {
                            m.extend(fields);
                        }
                        let fresh = s.observed_at >= started;
                        m.insert("settled".into(), json!(fresh && s.satisfies(&pred)));
                        if !fresh {
                            m.insert("stale".into(), json!(true));
                        }
                    }
                    _ => {
                        m.insert("status".into(), Value::Null);
                        m.insert("settled".into(), json!(false));
                    }
                }
                if let Some(e) = &g.last_transient {
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
        if req.predicate.until_delegated && req.targets.iter().any(|t| t.transport == Transport::Local) {
            out["warnings"] = json!(["--until-delegated-settled is only enforced over the mcp transport"]);
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
                json!({
                    "machine": w.key.machine,
                    "threadId": w.key.thread_id,
                    "subscribers": g.subs.len(),
                    "upstreamCalls": w.upstream_calls.load(Ordering::Relaxed),
                    "status": status,
                    "lastError": g.last_transient,
                    "ageMs": w.created.elapsed().as_millis() as u64,
                })
            })
            .collect();
        let machines: Vec<Value> = lock(&self.clients)
            .values()
            .map(|c| {
                json!({
                    "machine": c.name,
                    "endpoint": c.endpoint(),
                    "hasSession": c.session_id().is_some(),
                    "initialize": c.stats.initialize.load(Ordering::Relaxed),
                    "threadWait": c.stats.thread_wait.load(Ordering::Relaxed),
                    "threadRead": c.stats.thread_read.load(Ordering::Relaxed),
                    "sessionResets": c.stats.session_resets.load(Ordering::Relaxed),
                    "retries": c.stats.retries.load(Ordering::Relaxed),
                })
            })
            .collect();
        json!({
            "pid": std::process::id(),
            "version": env!("CARGO_PKG_VERSION"),
            "uptimeMs": self.started.elapsed().as_millis() as u64,
            "activeWaits": self.active_waits.load(Ordering::SeqCst),
            "watches": watches,
            "machines": machines,
        })
    }
}

impl Watch {
    /// The request's condition as it applies to this watch: local watches
    /// cannot see delegated work, so that part is dropped for them.
    fn predicate(&self, req: &WaitRequest) -> Predicate {
        Predicate {
            until_delegated: req.predicate.until_delegated && self.key.until_delegated,
            ..req.predicate
        }
    }

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

    fn run(self: Arc<Self>, hub: &Hub) {
        let mut jitter = Jitter::new();
        let mut failures: u32 = 0;
        let mut quiet_polls: u32 = 0;
        while self.keep_running(hub) {
            let result = match &self.backend {
                Backend::Mcp(client) => self.mcp_cycle(client),
                Backend::Local { t3_bin, base_dir } => match t3_bin.clone().or_else(|| config::find_t3_bin(base_dir)) {
                    Some(bin) => {
                        self.upstream_calls.fetch_add(1, Ordering::Relaxed);
                        local::show(&bin, base_dir, &self.key.thread_id).map(|s| (s, false))
                    }
                    None => Err(CallError::Transient {
                        message: format!("no t3 CLI under {}", base_dir.display()),
                        status: None,
                    }),
                },
            };
            let delay = match result {
                Ok((state, long_poll_timed_out)) => {
                    failures = 0;
                    self.publish(Observation::State(state));
                    match self.backend {
                        Backend::Local { .. } => self.poll_interval(),
                        // The long poll already blocked for a whole slice.
                        Backend::Mcp(_) if long_poll_timed_out => {
                            quiet_polls = 0;
                            Duration::ZERO
                        }
                        // The thread is idle but someone is still waiting
                        // (e.g. for a higher run ordinal): poll gently.
                        Backend::Mcp(_) => {
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
    /// the poll timed out on a still-active thread. Returns the observation
    /// and whether the long-poll timed out.
    fn mcp_cycle(&self, client: &McpClient) -> Result<(ThreadState, bool), CallError> {
        let slice_ms = self.slice_ms.load(Ordering::Relaxed);
        let mut args = json!({ "threadId": self.key.thread_id, "timeoutMs": slice_ms });
        if self.key.until_delegated {
            args["untilDelegatedWorkSettled"] = json!(true);
        }
        if let Some(env) = &self.key.environment_id {
            args["environmentId"] = json!(env);
        }
        let wait_started = Instant::now();
        self.upstream_calls.fetch_add(1, Ordering::Relaxed);
        let w = client.call_tool(
            "t3_thread_wait",
            args,
            Duration::from_millis(slice_ms) + Duration::from_secs(30),
        )?;
        let timed_out = w.get("timedOut").and_then(Value::as_bool).unwrap_or(false);
        let wait_status = w.get("status").and_then(Value::as_str).unwrap_or("");
        if timed_out && is_active(wait_status) {
            let mut s = ThreadState::from_thread_json(
                &json!({ "status": wait_status, "latestRunId": w.get("runId") }),
                wait_started,
            );
            s.wait_returned = false;
            return Ok((s, true));
        }

        let mut args = json!({ "threadId": self.key.thread_id, "limit": 1, "maxCharsPerItem": 200 });
        if let Some(env) = &self.key.environment_id {
            args["environmentId"] = json!(env);
        }
        let read_started = Instant::now();
        let r = client.call_tool("t3_thread_read", args, Duration::from_secs(30))?;
        let mut s = ThreadState::from_thread_json(&r, read_started);
        s.wait_returned = !timed_out && !is_active(wait_status);
        s.result_run_id = match w.get("resultRun") {
            Some(Value::String(id)) => Some(id.clone()),
            Some(v @ Value::Object(_)) => v.get("runId").and_then(Value::as_str).map(str::to_string),
            _ => None,
        };
        Ok((s, timed_out))
    }
}
