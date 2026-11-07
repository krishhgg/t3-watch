//! t3-watch: wait for T3 Code threads to settle without spending AI tokens.

mod cancel;
mod config;
mod daemon;
mod engine;
mod local;
mod mcp;
mod model;
mod notify;
mod oauth;
mod outbox;
mod trace;
mod util;
mod version;

use config::Config;
use engine::{error_json, Hub, Outcome, TargetSpec, Transport, WaitRequest, LOCAL_MACHINE};
use model::Predicate;
use notify::{Notify, NotifyArgs};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// The threads completed but a configured notification could not be
/// delivered (or, with `--durable`, is queued for retry).
const EXIT_NOTIFY_FAILED: i32 = 4;

const USAGE: &str = "\
t3-watch: wait for T3 Code threads to finish without spending AI tokens.

USAGE
  t3-watch wait [OPTIONS] <THREAD>...     block until threads finish, fail or need input
  t3-watch status                         show the daemon's watches, sessions and outbox
  t3-watch stop                           stop the daemon
  t3-watch daemon [--idle-exit 60s]       run the multiplexer in the foreground
  t3-watch outbox list | run [--once] | drop <ID>
                                          durable notifications not yet delivered
  t3-watch auth login <ORIGIN|URL>        pair with a T3 MCP server (browser on this machine)
  t3-watch auth oauth-start <ORIGIN|URL>  pair from a headless machine, step 1
  t3-watch auth oauth-finish <CALLBACK>   pair from a headless machine, step 2
  t3-watch auth discover <ORIGIN|URL>     show OAuth metadata without registering
  t3-watch auth list | default <NAME> | logout <NAME>

THREAD is a thread id, a T3 thread URL (https://<relay>/<envId>/<threadId>),
or a t3-thread://v2/<envId>/<threadId> link. URLs pick the saved machine
with the same origin.

WAIT OPTIONS
  -m, --machine <NAME>             saved machine to use (`local` for the local CLI)
      --transport <auto|mcp|local>
      --env <ENV_ID>               environmentId for a peer-linked environment
      --mode <all|any>             wait for all threads (default) or the first
      --until <turn|workstream>    turn (default): the latest run finished.
                                   workstream (mcp only): the run, its delegated
                                   work and the parent runs that deliver its
                                   results all settled, proven by T3
      --until-delegated-settled    same as --until workstream
      --expect-min-run-ordinal <N> ignore runs older than ordinal N
      --after-run <RUN_ID>         ignore that run and older ones (one thread)
      --final-marker <TEXT>        also require TEXT in the final assistant message
                                   of the deciding run (not on delegated child threads)
      --follow                     a failed run does not end the wait; report it as
                                   progress and follow the next run of the thread
      --acknowledge <PENDING_KEY>  keep waiting past the pending requests named by
                                   this key (from a needs_input result)
      --timeout <DUR>              give up after DUR (default 4h)
      --interval <DUR>             local poll interval (default 5s)
      --slice <DUR>                mcp long-poll length (default 30s; no long-poll
                                   exceeds 30s, so input is seen within about 30s)
      --base-dir <DIR>             T3 data dir for local (default $T3CODE_HOME or ~/.t3)
      --t3-bin <PATH>              T3 CLI for local (default: newest under base dir)
      --no-daemon                  watch in this process instead of the shared daemon
      --pretty                     indent the JSON output

NOTIFICATIONS (sent by this process; the daemon only sends queued ones)
      --webhook-url-env <VAR>      POST the result JSON to the https URL in $VAR
      --webhook-url-file <PATH>    ... or to the URL stored in a file
      --webhook-url <URL>          ... or to this URL (visible in `ps`; prefer -env/-file)
      --webhook-header-env <H>=<VAR>  add header H with the value of $VAR (repeatable)
      --webhook-secret-env <VAR>   sign with HMAC-SHA256 using the secret in $VAR
      --webhook-secret-file <PATH> ... or the secret stored in a file
      --durable                    keep an undelivered webhook in the outbox and retry
                                   it (from the daemon) until a 2xx
      --durable-max-age <DUR>      give up on a queued webhook after DUR (default 7d)
      --heartbeat <DUR>            POST a heartbeat every DUR while waiting, for a
                                   receiver-side monitor (not retried, not queued)
      --notify-unreachable <DUR>   report T3 unreachable from here for DUR, and back
      --on-settle <PROG>           run PROG (no shell) with the result JSON on stdin;
                                   its output goes to hooks.log in the state dir
      --on-settle-arg <ARG>        pass ARG to PROG (repeatable)
      --on-settle-timeout <DUR>    kill PROG after DUR (default 60s)
      --notify-on <CLASSES|all>    comma list of settled, needs_input, timeout, error,
                                   run_failed, upstream (default: all but timeout
                                   and error)

AUTH OPTIONS
      --name <NAME>                machine name (default from the host)
      --default                    make it the default machine for bare thread ids
      --no-browser                 print the URL instead of opening a browser
      --port <N>                   callback port for login (default: any free port)
      --redirect-port <N>          callback port written into oauth-start (default 8976)

EXIT CODES
  0 completed     5 needs input (question or approval)     6 failed run
  3 timed out     2 error (auth revoked, unknown thread, unsupported, usage)
  4 completed, but a notification was not delivered (or is only queued)
  1 internal
  Output is one JSON object on stdout.
";

struct Args {
    positional: Vec<String>,
    values: HashMap<&'static str, Vec<String>>,
    flags: Vec<&'static str>,
}

impl Args {
    /// The last value given for an option.
    fn get(&self, k: &str) -> Option<&str> {
        self.values.get(k).and_then(|v| v.last()).map(String::as_str)
    }

    /// Every value given for a repeatable option, in order.
    fn get_all(&self, k: &str) -> Vec<String> {
        self.values.get(k).cloned().unwrap_or_default()
    }

    fn has(&self, k: &str) -> bool {
        self.flags.contains(&k)
    }
}

/// Parses `--key value`, `--key=value`, boolean `--flag`s and positionals.
/// `value_opts` and `flag_opts` list long names; `-m` maps to `--machine`.
fn parse_args(raw: &[String], value_opts: &[&'static str], flag_opts: &[&'static str]) -> Result<Args, String> {
    let mut a = Args {
        positional: Vec::new(),
        values: HashMap::new(),
        flags: Vec::new(),
    };
    let mut i = 0;
    while i < raw.len() {
        let arg = raw[i].as_str();
        i += 1;
        if arg == "--" {
            a.positional.extend(raw[i..].iter().cloned());
            break;
        }
        let long = if arg == "-m" {
            Some("--machine")
        } else if arg.starts_with("--") {
            Some(arg)
        } else {
            None
        };
        let Some(long) = long else {
            a.positional.push(arg.to_string());
            continue;
        };
        let (name, inline) = match long[2..].split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (&long[2..], None),
        };
        if let Some(&k) = value_opts.iter().find(|k| **k == name) {
            let v = match inline {
                Some(v) => v,
                None => {
                    let v = raw.get(i).ok_or_else(|| format!("--{name} needs a value"))?;
                    i += 1;
                    v.clone()
                }
            };
            a.values.entry(k).or_default().push(v);
        } else if let Some(&k) = flag_opts.iter().find(|k| **k == name) {
            a.flags.push(k);
        } else {
            return Err(format!("unknown option --{name}"));
        }
    }
    Ok(a)
}

fn usage_error(msg: &str) -> ! {
    util::warn(&format!("t3-watch: {msg}\nRun `t3-watch --help` for usage."));
    exit(2)
}

/// Writes to stdout without panicking when the reader has gone away
/// (`println!` panics on EPIPE). The result of a wait must never depend on
/// whether anyone is still reading.
fn write_stdout(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes()).and_then(|_| out.flush());
}

fn print_json(v: &Value, pretty: bool) {
    let mut s = if pretty {
        serde_json::to_string_pretty(v).unwrap()
    } else {
        v.to_string()
    };
    s.push('\n');
    write_stdout(&s);
}

fn fail_json(kind: &str, msg: &str) -> ! {
    print_json(&error_json(kind, msg), false);
    exit(2)
}

struct WaitDefaults<'a> {
    machine: Option<&'a str>,
    transport: Option<Transport>,
    env: Option<&'a str>,
    /// Absolute T3 data directory, resolved in this process.
    base_dir: PathBuf,
    /// Absolute `t3` binary, resolved in this process.
    t3_bin: Option<PathBuf>,
    /// The user named a binary (`--t3-bin` or `$T3_WATCH_T3_BIN`). A `t3`
    /// that merely happens to be on `PATH` does not make bare ids local.
    explicit_bin: bool,
}

impl WaitDefaults<'_> {
    fn local_available(&self) -> bool {
        self.base_dir.is_dir() || self.explicit_bin
    }
}

/// Resolves the local `t3` paths here, in the client, so the daemon (which
/// runs from `/` with whatever environment its first client had) watches
/// exactly what this caller meant. Returns the data dir, a binary if the
/// daemon cannot find one under that dir, and whether the user named it.
fn resolve_local_paths(
    base_dir: Option<&str>,
    t3_bin: Option<&str>,
) -> Result<(PathBuf, Option<PathBuf>, bool), String> {
    let base = match base_dir {
        Some(p) => config::absolute(&PathBuf::from(p))?,
        None => config::absolute(&config::t3_base_dir())?,
    };
    let explicit = match t3_bin {
        Some(p) => Some(PathBuf::from(p)),
        None => config::t3_bin_override(),
    };
    let named = explicit.is_some();
    let bin = match explicit {
        Some(p) => {
            let abs = config::absolute(&p)?;
            if !abs.is_file() {
                return Err(format!("t3 binary {} does not exist", abs.display()));
            }
            Some(abs)
        }
        // The daemon finds the newest install under the data dir itself.
        None if config::find_t3_in_base(&base).is_some() => None,
        None => config::find_t3_on_path().map(|p| config::absolute(&p)).transpose()?,
    };
    Ok((base, bin, named))
}

/// Turns one THREAD argument into a target: which machine and transport,
/// the thread id, and the environment id when the URL carries one.
fn resolve_target(arg: &str, d: &WaitDefaults, cfg: &Config) -> Result<TargetSpec, String> {
    let arg = arg.trim().trim_start_matches('<').trim_end_matches('>');
    // Every message below shows the argument without userinfo, query or
    // fragment: any of them can carry a credential.
    let shown = util::without_secrets(arg);
    let mut origin: Option<(String, String)> = None; // (origin, host)
    let (thread_id, env) = if let Some((scheme, rest)) = arg.split_once("://") {
        // A URL: only its path names the thread. The query and fragment
        // are never part of it, and credentials in the authority are
        // refused rather than sent anywhere or treated as a thread id.
        let scheme = scheme.to_ascii_lowercase();
        let rest = rest.split(['?', '#']).next().unwrap_or("");
        let authority = rest.split('/').next().unwrap_or("");
        if authority.contains('@') {
            return Err(format!(
                "thread URLs must not carry credentials (user:password@...): {shown}"
            ));
        }
        match scheme.as_str() {
            "t3-thread" => {
                let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
                match segs.as_slice() {
                    [_version, env, thread @ ..] if !thread.is_empty() => {
                        (util::pct_decode(&thread.join("/"), false), Some(env.to_string()))
                    }
                    _ => return Err(format!("cannot parse thread link {shown}")),
                }
            }
            "http" | "https" => {
                let u = util::parse_url(arg).ok_or_else(|| format!("cannot parse thread URL {shown}"))?;
                let segs: Vec<&str> = u.path.split('/').filter(|s| !s.is_empty()).collect();
                let [env, thread @ ..] = segs.as_slice() else {
                    return Err(format!("URL has no thread id: {shown}"));
                };
                if thread.is_empty() {
                    return Err(format!("URL has no thread id: {shown}"));
                }
                origin = Some((u.origin.clone(), u.host.clone()));
                (util::pct_decode(&thread.join("/"), false), Some(env.to_string()))
            }
            _ => return Err(format!("unsupported thread URL scheme: {shown}")),
        }
    } else {
        (arg.to_string(), None)
    };
    // Thread ids have none of these; one that does is a mangled URL or a
    // pasted query, and is refused before it can be logged or sent.
    if thread_id
        .chars()
        .any(|c| matches!(c, '?' | '#' | '@') || c.is_whitespace())
    {
        // Not echoed: what follows such a character may be a credential.
        return Err("invalid thread id: it contains '?', '#', '@' or whitespace".into());
    }
    if thread_id.is_empty() {
        return Err("empty thread id".into());
    }
    let env = d.env.map(str::to_string).or(env);

    let local = |thread_id: String| TargetSpec {
        transport: Transport::Local,
        machine: LOCAL_MACHINE.into(),
        thread_id,
        environment_id: None,
        t3_bin: d.t3_bin.clone(),
        base_dir: Some(d.base_dir.clone()),
    };
    let mcp = |name: &str, thread_id: String, env: Option<String>| TargetSpec {
        transport: Transport::Mcp,
        machine: name.to_string(),
        thread_id,
        environment_id: env,
        t3_bin: None,
        base_dir: None,
    };

    if d.transport == Some(Transport::Local) || d.machine == Some(LOCAL_MACHINE) {
        return Ok(local(thread_id));
    }
    if let Some(name) = d.machine {
        if !cfg.machines.contains_key(name) {
            return Err(format!(
                "no saved machine named {name:?}; run `t3-watch auth login <origin> --name {name}`"
            ));
        }
        return Ok(mcp(name, thread_id, env));
    }
    if let Some((origin, host)) = origin {
        if let Some((name, _)) = cfg.by_origin(&origin) {
            return Ok(mcp(name, thread_id, env));
        }
        if util::is_loopback_host(&host) && d.transport != Some(Transport::Mcp) {
            return Ok(local(thread_id));
        }
        return Err(format!(
            "no saved machine for {origin}; run `t3-watch auth login {origin}`"
        ));
    }
    if let Some(name) = cfg.default.as_deref().filter(|n| cfg.machines.contains_key(*n)) {
        return Ok(mcp(name, thread_id, env));
    }
    if d.transport != Some(Transport::Mcp) && d.local_available() {
        return Ok(local(thread_id));
    }
    if cfg.machines.len() == 1 {
        let name = cfg.machines.keys().next().unwrap();
        return Ok(mcp(name, thread_id, env));
    }
    Err(format!(
        "cannot tell which machine {thread_id} is on; pass a thread URL, --machine <name>, or --transport local"
    ))
}

fn parse_dur(a: &Args, key: &str, default: Duration) -> Duration {
    match a.get(key) {
        Some(s) => util::parse_duration(s).unwrap_or_else(|e| usage_error(&format!("--{key}: {e}"))),
        None => default,
    }
}

/// Builds the wait request from the parsed options. Errors are the JSON the
/// CLI prints, so they can go through notification delivery.
fn prepare_wait(a: &Args) -> Result<WaitRequest, Value> {
    let usage = |m: String| error_json("usage", &m);
    let dur = |key: &str, default: Duration| -> Result<Duration, Value> {
        match a.get(key) {
            Some(s) => util::parse_duration(s).map_err(|e| usage(format!("--{key}: {e}"))),
            None => Ok(default),
        }
    };
    let transport = match a.get("transport") {
        None | Some("auto") => None,
        Some("mcp") => Some(Transport::Mcp),
        Some("local") => Some(Transport::Local),
        Some(t) => return Err(usage(format!("unknown transport {t:?}"))),
    };
    let any = match a.get("mode") {
        None | Some("all") => false,
        Some("any") => true,
        Some(m) => return Err(usage(format!("unknown mode {m:?}"))),
    };
    let mut min_run_ordinal = match a.get("expect-min-run-ordinal") {
        Some(s) => s
            .parse::<u64>()
            .map_err(|_| usage("--expect-min-run-ordinal needs a whole number".into()))?,
        None => 0,
    };
    let until_delegated = match (a.get("until"), a.has("until-delegated-settled")) {
        (None, flag) => flag,
        (Some("workstream"), _) => true,
        (Some("turn"), false) => false,
        (Some("turn"), true) => return Err(usage("--until turn contradicts --until-delegated-settled".into())),
        (Some(u), _) => return Err(usage(format!("--until takes turn or workstream, not {u:?}"))),
    };
    let cfg = Config::load().map_err(|e| error_json("config", &e))?;
    let (base_dir, t3_bin, explicit_bin) =
        resolve_local_paths(a.get("base-dir"), a.get("t3-bin")).map_err(|e| error_json("config", &e))?;
    let defaults = WaitDefaults {
        machine: a.get("machine"),
        transport,
        env: a.get("env"),
        base_dir,
        t3_bin,
        explicit_bin,
    };
    let targets = a
        .positional
        .iter()
        .map(|t| resolve_target(t, &defaults, &cfg).map_err(usage))
        .collect::<Result<Vec<TargetSpec>, Value>>()?;
    if let Some(run) = a.get("after-run") {
        let [target] = targets.as_slice() else {
            return Err(usage(
                "--after-run names a run of one thread; pass exactly one thread".into(),
            ));
        };
        let ordinal = model::run_ordinal_of(&target.thread_id, run).ok_or_else(|| {
            usage(format!(
                "--after-run {:?} is not a run of thread {}",
                util::sanitize_text(run, 200),
                target.thread_id
            ))
        })?;
        min_run_ordinal = min_run_ordinal.max(ordinal + 1);
    }
    let notify_unreachable = match a.get("notify-unreachable") {
        Some(_) => Some(dur("notify-unreachable", Duration::ZERO)?),
        None => None,
    };
    let req = WaitRequest {
        targets,
        any,
        predicate: Predicate {
            min_run_ordinal,
            until_delegated,
            final_marker: a.get("final-marker").map(str::to_string),
            acknowledged: a.get("acknowledge").map(str::to_string),
            follow: a.has("follow"),
        },
        timeout: dur("timeout", Duration::from_secs(4 * 3600))?,
        interval: dur("interval", Duration::from_secs(5))?,
        slice: dur("slice", engine::DEFAULT_SLICE)?,
        notify_unreachable,
    }
    .normalized();
    // Refuse up front what the daemon would refuse, such as delegated-work
    // waits on the local transport, so nothing starts for a doomed request.
    req.validate().map_err(usage)?;
    Ok(req)
}

/// Sends heartbeats from a background thread while the wait runs. A
/// receiver that stops hearing them knows this watcher (process, host or
/// network) is gone; a dead host cannot say so itself.
struct Heartbeat {
    stop: Arc<AtomicBool>,
    thread: std::thread::Thread,
}

impl Heartbeat {
    fn start(
        notify: Arc<Notify>,
        every: Duration,
        base: Value,
        status: Box<dyn Fn() -> Option<Value> + Send>,
    ) -> Option<Heartbeat> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let sw = util::Stopwatch::start();
        let handle = util::spawn_thread("heartbeat", 128 * 1024, move || {
            let mut seq: u64 = 0;
            while !flag.load(Ordering::SeqCst) {
                let mut body = base.clone();
                body["event"] = json!("heartbeat");
                body["seq"] = json!(seq);
                body["sentAt"] = json!(util::rfc3339_ms(util::now_ms()));
                body["elapsedMs"] = json!(sw.elapsed_ms());
                body["intervalMs"] = json!(every.as_millis() as u64);
                body["upstream"] = status().unwrap_or(Value::Null);
                let id = util::hex(&util::random_bytes(16));
                notify.heartbeat(&id, body.to_string().as_bytes());
                seq += 1;
                let next = Instant::now() + every;
                while !flag.load(Ordering::SeqCst) && Instant::now() < next {
                    std::thread::park_timeout(next.saturating_duration_since(Instant::now()));
                }
            }
        })
        .ok()?;
        Some(Heartbeat {
            stop,
            thread: handle.thread().clone(),
        })
    }

    /// Stops sending without waiting for a POST in flight.
    fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.unpark();
    }
}

/// The parts of a daemon or hub status that matter to a heartbeat: per
/// watched thread, its last status and whether T3 is reachable from here.
fn upstream_of(status: &Value, threads: &[String]) -> Value {
    let watches: Vec<Value> = status["watches"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|w| w["threadId"].as_str().is_some_and(|t| threads.iter().any(|x| x == t)))
        .map(|w| {
            json!({
                "threadId": w["threadId"],
                "status": w["status"],
                "reachable": w.get("upstream").is_none(),
                "upstream": w.get("upstream"),
            })
        })
        .collect();
    json!(watches)
}

fn cmd_wait(raw: &[String]) -> ! {
    // Elapsed times are reported three ways: suspend-inclusive (`elapsedMs`,
    // what `--timeout` counts), awake only, and by the wall clock.
    let sw = util::Stopwatch::start();
    let started_ms = sw.wall_ms;
    let a = parse_args(
        raw,
        &[
            "machine",
            "transport",
            "env",
            "mode",
            "until",
            "expect-min-run-ordinal",
            "after-run",
            "final-marker",
            "acknowledge",
            "timeout",
            "interval",
            "slice",
            "base-dir",
            "t3-bin",
            "webhook-url",
            "webhook-url-env",
            "webhook-url-file",
            "webhook-header-env",
            "webhook-secret-env",
            "webhook-secret-file",
            "durable-max-age",
            "heartbeat",
            "notify-unreachable",
            "on-settle",
            "on-settle-arg",
            "on-settle-timeout",
            "notify-on",
        ],
        &["until-delegated-settled", "follow", "durable", "no-daemon", "pretty"],
    )
    .unwrap_or_else(|e| usage_error(&e));
    if a.positional.is_empty() {
        usage_error("wait needs at least one thread id or URL");
    }

    // Secrets are read (and scrubbed from the environment) before anything
    // else runs, in particular before the daemon is started.
    let notify = Notify::from_args(&NotifyArgs {
        webhook_url: a.get("webhook-url").map(str::to_string),
        webhook_url_env: a.get("webhook-url-env").map(str::to_string),
        webhook_url_file: a.get("webhook-url-file").map(str::to_string),
        header_envs: a.get_all("webhook-header-env"),
        secret_env: a.get("webhook-secret-env").map(str::to_string),
        secret_file: a.get("webhook-secret-file").map(str::to_string),
        on_settle: a.get("on-settle").map(str::to_string),
        on_settle_args: a.get_all("on-settle-arg"),
        on_settle_timeout: a
            .get("on-settle-timeout")
            .map(|_| parse_dur(&a, "on-settle-timeout", notify::DEFAULT_HOOK_TIMEOUT)),
        notify_on: a.get("notify-on").map(str::to_string),
        durable: a.has("durable"),
        durable_max_age: a
            .get("durable-max-age")
            .map(|_| parse_dur(&a, "durable-max-age", outbox::DEFAULT_MAX_AGE)),
    })
    .unwrap_or_else(|e| fail_json("usage", &e));
    let notify = Arc::new(notify);
    let heartbeat_every = a
        .get("heartbeat")
        .map(|_| parse_dur(&a, "heartbeat", Duration::from_secs(60)));
    if heartbeat_every.is_some_and(|d| d < Duration::from_secs(1)) {
        fail_json("usage", "--heartbeat must be at least 1s");
    }
    if heartbeat_every.is_some() && !notify.has_webhook() {
        fail_json("usage", "--heartbeat posts to the webhook; it needs a webhook URL");
    }

    let pretty = a.has("pretty");
    let no_daemon = a.has("no-daemon") || std::env::var_os("T3_WATCH_NO_DAEMON").is_some();
    let watch_id = util::hex(&util::random_bytes(8));
    let host = hostname();

    // Every event of this wait, progress and final, gets the next
    // `eventSeq`, so a receiver can order them however they arrive (an
    // outbox retry can land after a later event).
    let event_seq = std::cell::Cell::new(0u64);
    let next_seq = || {
        event_seq.set(event_seq.get() + 1);
        event_seq.get()
    };

    // Progress events are prepared (with --durable: persisted) as they
    // happen, then sent from their own thread, in order, so a slow
    // receiver never stops this process from reading the daemon. If that
    // thread cannot start, they are sent inline instead, never dropped.
    let (progress_tx, progress_rx) = mpsc::channel::<notify::Prepared>();
    let progress_sender = {
        let notify = notify.clone();
        util::spawn_thread("progress", 128 * 1024, move || {
            for p in progress_rx {
                let _ = notify.send(p);
            }
        })
    };
    let (progress_tx, progress_sender) = match progress_sender {
        Ok(handle) => (Some(progress_tx), Some(handle)),
        Err(e) => {
            util::warn(&format!(
                "t3-watch: cannot start the progress sender ({e}); progress notifications are sent inline"
            ));
            (None, None)
        }
    };
    // A daemon reconnect re-registers the wait, which may report the same
    // failed run again; pass each one on once.
    let mut failed_seen = std::collections::HashSet::new();
    let mut on_progress = |mut p: Value| {
        if p["event"] == "run_failed" {
            let key = p["threads"][0]["latestRunId"].to_string();
            if !failed_seen.insert(key) {
                return;
            }
        }
        p["watchId"] = json!(watch_id);
        p["eventSeq"] = json!(next_seq());
        p["emittedAt"] = json!(util::rfc3339_ms(util::now_ms()));
        let event = p["event"].as_str().unwrap_or("progress").to_string();
        // Persist first; the diagnostic line is best effort and comes last.
        if let Some(prepared) = notify.prepare(&event, p.to_string().as_bytes()) {
            match &progress_tx {
                Some(tx) => {
                    if let Err(mpsc::SendError(prepared)) = tx.send(prepared) {
                        let _ = notify.send(prepared);
                    }
                }
                None => {
                    let _ = notify.send(prepared);
                }
            }
        }
        util::warn(&format!("t3-watch: progress {p}"));
    };

    // From here on the notification config is valid, so every failure,
    // operational or not, becomes an outcome that goes through delivery: an
    // unattended bot with --notify-on all hears about a broken config too.
    let mut reconnects = 0;
    let mut stale_answers = 0;
    let mut daemon_build: Option<Value> = None;
    let mut heartbeat = None;
    // Threads are traced as parsed ids only: an argument can be a URL whose
    // query or userinfo carries a credential.
    let mut outcome = match prepare_wait(&a) {
        Err(e) => {
            trace::record(
                "wait.start",
                json!({ "watchId": watch_id, "argCount": a.positional.len(), "error": e["error"]["kind"] }),
            );
            Outcome::Error(e)
        }
        Ok(req) => {
            trace::record(
                "wait.start",
                json!({
                    "watchId": watch_id,
                    "targets": req.targets.iter().map(|t| json!({
                        "machine": t.machine,
                        "threadId": t.thread_id,
                        "environmentId": t.environment_id,
                        "transport": if t.transport == Transport::Local { "local" } else { "mcp" },
                    })).collect::<Vec<_>>(),
                    "until": if req.predicate.until_delegated { "workstream" } else { "turn" },
                    "noDaemon": no_daemon,
                    "durable": a.has("durable"),
                }),
            );
            let base = json!({
                "watchId": watch_id,
                "host": host,
                "pid": std::process::id(),
                "startedAt": util::rfc3339_ms(started_ms),
                "threadIds": req.targets.iter().map(|t| t.thread_id.clone()).collect::<Vec<_>>(),
                "note": "liveness of this watcher only; if heartbeats stop, the watcher host, process or network is gone",
            });
            if no_daemon {
                let hub = Hub::new("direct");
                if let Some(every) = heartbeat_every {
                    let h = hub.clone();
                    let ids: Vec<String> = req.targets.iter().map(|t| t.thread_id.clone()).collect();
                    heartbeat = Heartbeat::start(
                        notify.clone(),
                        every,
                        base,
                        Box::new(move || Some(upstream_of(&h.status_json(), &ids))),
                    );
                }
                let (tx, rx) = mpsc::channel();
                hub.wait(&req, tx, rx, &mut on_progress)
            } else {
                if let Some(every) = heartbeat_every {
                    let ids: Vec<String> = req.targets.iter().map(|t| t.thread_id.clone()).collect();
                    heartbeat = Heartbeat::start(
                        notify.clone(),
                        every,
                        base,
                        Box::new(move || daemon::request("status").ok().flatten().map(|s| upstream_of(&s, &ids))),
                    );
                }
                match daemon::wait(&req, &mut on_progress) {
                    Ok((o, info)) => {
                        reconnects = info.reconnects;
                        stale_answers = info.stale;
                        daemon_build = info.build;
                        o
                    }
                    Err(e) => Outcome::Error(error_json("daemon", &e)),
                }
            }
        }
    };
    if let Some(h) = heartbeat {
        h.stop();
    }

    let mut code = outcome.exit_code();
    let event = outcome.event();
    // The final event is prepared (with --durable: persisted) before
    // waiting for any progress delivery still in flight.
    let mut prepared = None;
    if let Some(v) = outcome.json_mut() {
        if let Some(daemon_elapsed) = v.get("elapsedMs").cloned() {
            if reconnects > 0 || stale_answers > 0 {
                v["registrationElapsedMs"] = daemon_elapsed;
            }
        }
        // The observation ended now; delivery and exit come after, and are
        // reported separately (`notification.finishedAt`, `exitAt`).
        let ended_ms = util::now_ms();
        v["elapsedMs"] = json!(sw.elapsed_ms());
        v["elapsedAwakeMs"] = json!(sw.awake_ms());
        v["elapsedWallMs"] = json!(ended_ms.saturating_sub(started_ms));
        v["startedAt"] = json!(util::rfc3339_ms(started_ms));
        v["endedAt"] = json!(util::rfc3339_ms(ended_ms));
        v["observationEndedAt"] = json!(util::rfc3339_ms(ended_ms));
        let suspended = sw.suspended_ms();
        if suspended > 1_000 {
            v["suspendedMs"] = json!(suspended);
            v["clock"] = json!(format!(
                "the host was suspended for about {} s during this wait; --timeout counts that time, \
                 and calls in flight across it were restarted on resume",
                suspended / 1000
            ));
        }
        v["watchId"] = json!(watch_id);
        v["eventSeq"] = json!(next_seq());
        v["build"] = version::json();
        if let Some(d) = &daemon_build {
            v["daemon"] = d.clone();
        }
        if reconnects > 0 {
            v["daemonReconnects"] = json!(reconnects);
        }
        if stale_answers > 0 {
            v["daemonAnswersDiscarded"] = json!(stale_answers);
        }
        trace::record(
            "wait.outcome",
            json!({
                "watchId": watch_id,
                "event": event,
                "eventSeq": v["eventSeq"],
                "outcome": v["outcome"],
                "endedAt": v["endedAt"],
                "elapsedMs": v["elapsedMs"],
                "elapsedAwakeMs": v["elapsedAwakeMs"],
                "elapsedWallMs": v["elapsedWallMs"],
                "suspendedMs": v.get("suspendedMs"),
                "daemon": daemon_build,
                "threads": v["threads"].as_array().map(|ts| ts.iter().map(|t| json!({
                    "threadId": t["threadId"],
                    "status": t["status"],
                    "runOrdinal": t["runOrdinal"],
                    "resultRunId": t.get("resultRunId"),
                    "observedAt": t.get("observedAt"),
                    "runCompletedAt": t.get("runCompletedAt"),
                })).collect::<Vec<_>>()),
            }),
        );
        prepared = notify.prepare(event, v.to_string().as_bytes());
    }
    // Earlier progress goes out (or is queued) before the final event.
    drop(progress_tx);
    if let Some(sender) = progress_sender {
        let _ = sender.join();
    }
    if let (Some(p), Some(v)) = (prepared, outcome.json_mut()) {
        let mut summary = notify.send(p);
        summary["finishedAt"] = json!(util::rfc3339_ms(util::now_ms()));
        if summary["delivered"] == false && code == engine::EXIT_COMPLETED {
            code = EXIT_NOTIFY_FAILED;
        }
        if summary["queued"] == true && !no_daemon {
            // The daemon sends queued notifications; make sure it runs.
            if let Err(e) = daemon::ensure_running() {
                util::warn(&format!(
                    "t3-watch: cannot start the daemon to send the queued notification: {e}"
                ));
            }
        }
        v["notification"] = summary;
    }
    let exit_ms = util::now_ms();
    if let Some(v) = outcome.json_mut() {
        v["exitAt"] = json!(util::rfc3339_ms(exit_ms));
    }
    if let Some(v) = outcome.json() {
        print_json(v, pretty);
    }
    trace::record(
        "wait.exit",
        json!({ "watchId": watch_id, "code": code, "elapsedMs": sw.elapsed_ms(), "suspendedMs": sw.suspended_ms() }),
    );
    exit(code)
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    if ok {
        util::sanitize_text(&String::from_utf8_lossy(&buf[..end]), 255)
    } else {
        String::new()
    }
}

fn cmd_outbox(raw: &[String]) -> ! {
    let Some(sub) = raw.first() else {
        usage_error("outbox needs a subcommand: list, run, drop")
    };
    let a = parse_args(&raw[1..], &[], &["once", "pretty"]).unwrap_or_else(|e| usage_error(&e));
    let pretty = a.has("pretty");
    match sub.as_str() {
        "list" => {
            let (entries, bad) = outbox::entries();
            print_json(
                &json!({
                    "entries": entries.iter().map(outbox::Entry::public_json).collect::<Vec<_>>(),
                    "legacy": outbox::legacy_public_json(),
                    "retained": outbox::retained_public_json(),
                    "unreadable": bad,
                    "dir": config::outbox_dir().display().to_string(),
                }),
                pretty,
            );
            exit(0)
        }
        "run" if a.has("once") => match outbox::pass(true) {
            Ok(results) => {
                let left = outbox::pending_count();
                print_json(&json!({ "results": results, "pending": left }), pretty);
                exit(if left == 0 { 0 } else { EXIT_NOTIFY_FAILED })
            }
            Err(e) => fail_json("outbox", &e),
        },
        "run" => outbox::run_forever(&|line| util::warn(&format!("t3-watch: {line}"))),
        "drop" => {
            let [id] = a.positional.as_slice() else {
                usage_error("outbox drop needs one entry id")
            };
            if !outbox::valid_id(id) {
                fail_json("usage", "not an outbox entry id");
            }
            match outbox::drop_entry(id) {
                Ok(Some(how)) => {
                    let mut v = json!({ "ok": true, "id": id });
                    if let (Value::Object(o), Value::Object(h)) = (&mut v, how) {
                        o.extend(h);
                    }
                    print_json(&v, pretty);
                    exit(0)
                }
                Ok(None) => {
                    print_json(&json!({ "ok": false, "id": id }), pretty);
                    exit(2)
                }
                Err(e) => fail_json("outbox", &e),
            }
        }
        other => usage_error(&format!("unknown outbox subcommand {other:?}")),
    }
}

fn cmd_auth(raw: &[String]) -> ! {
    let Some(sub) = raw.first() else {
        usage_error("auth needs a subcommand: login, oauth-start, oauth-finish, discover, list, default, logout")
    };
    run_auth(sub, &raw[1..])
}

fn run_auth(sub: &str, raw: &[String]) -> ! {
    let a = parse_args(
        raw,
        &["name", "port", "redirect-port", "timeout"],
        &["no-browser", "default", "pretty"],
    )
    .unwrap_or_else(|e| usage_error(&e));
    let pretty = a.has("pretty");
    let port = |k: &str, d: u16| {
        a.get(k)
            .map(|p| {
                p.parse::<u16>()
                    .unwrap_or_else(|_| usage_error(&format!("--{k} needs a port number")))
            })
            .unwrap_or(d)
    };
    let one = |what: &str| -> String {
        match a.positional.as_slice() {
            [x] => x.clone(),
            _ => usage_error(&format!("auth {sub} needs exactly one {what}")),
        }
    };
    let result = match sub {
        "login" => oauth::login(
            &one("origin or thread URL"),
            a.get("name"),
            port("port", 0),
            a.has("no-browser"),
            a.has("default"),
            parse_dur(&a, "timeout", Duration::from_secs(300)),
        ),
        "oauth-start" => oauth::start(
            &one("origin or thread URL"),
            a.get("name"),
            port("redirect-port", 8976),
            a.has("default"),
        ),
        "oauth-finish" => oauth::finish(&one("callback URL")),
        "discover" => oauth::origin_from_arg(&one("origin or thread URL"))
            .and_then(|o| oauth::discover(&o))
            .map(|d| {
                serde_json::json!({
                    "origin": d.origin,
                    "resource": d.resource,
                    "issuer": d.issuer,
                    "authorizationEndpoint": d.authorization_endpoint,
                    "tokenEndpoint": d.token_endpoint,
                    "registrationEndpoint": d.registration_endpoint,
                    "refreshTokens": d.supports_refresh,
                    "scope": oauth::SCOPE,
                })
            }),
        "list" => Config::load().map(|cfg| {
            let ms: Vec<Value> = cfg
                .machines
                .iter()
                .map(|(n, m)| m.public_json(n, cfg.default.as_deref() == Some(n)))
                .collect();
            serde_json::json!({ "machines": ms, "config": config::machines_path().display().to_string() })
        }),
        "default" | "logout" => {
            let name = one("machine name");
            Config::transaction(|cfg| {
                if !cfg.machines.contains_key(&name) {
                    return Err(format!("no saved machine named {name:?}"));
                }
                if sub == "logout" {
                    cfg.machines.remove(&name);
                    if cfg.default.as_deref() == Some(&name) {
                        cfg.default = None;
                    }
                } else {
                    cfg.default = Some(name.clone());
                }
                Ok(serde_json::json!({ "ok": true, "machine": name, "default": cfg.default }))
            })
        }
        _ => usage_error(&format!("unknown auth subcommand {sub:?}")),
    };
    match result {
        Ok(v) => {
            print_json(&v, pretty);
            exit(0)
        }
        Err(e) => fail_json("auth", &e),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or(&[]);
    match args.first().map(String::as_str) {
        Some("wait") => cmd_wait(rest),
        Some("auth") => cmd_auth(rest),
        Some("outbox") => cmd_outbox(rest),
        Some(sub @ ("login" | "oauth-start" | "oauth-finish")) => run_auth(sub, rest),
        Some("daemon") => {
            let a = parse_args(rest, &["idle-exit"], &[]).unwrap_or_else(|e| usage_error(&e));
            let env_idle = std::env::var("T3_WATCH_IDLE_EXIT")
                .ok()
                .and_then(|s| util::parse_duration(&s).ok());
            let idle = parse_dur(&a, "idle-exit", env_idle.unwrap_or(daemon::DEFAULT_IDLE_EXIT));
            if let Err(e) = daemon::run(idle) {
                util::warn(&format!("t3-watch daemon: {e}"));
                exit(1)
            }
        }
        Some("status") => match daemon::request("status") {
            Ok(Some(mut v)) => {
                v["running"] = Value::Bool(true);
                print_json(&v, rest.iter().any(|a| a == "--pretty"))
            }
            Ok(None) => write_stdout("{\"running\":false}\n"),
            Err(e) => fail_json("daemon", &e),
        },
        Some("stop") => match daemon::request("stop") {
            Ok(Some(v)) => print_json(&v, false),
            Ok(None) => write_stdout("{\"ok\":true,\"running\":false}\n"),
            Err(e) => fail_json("daemon", &e),
        },
        Some("-V" | "--version" | "version") => write_stdout(&format!("{}\n", version::line())),
        Some("-h" | "--help" | "help") | None => write_stdout(USAGE),
        Some(other) => usage_error(&format!("unknown command {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        let mut c = Config::default();
        c.machines.insert(
            "relay".into(),
            config::Machine {
                origin: "https://prod-1.t3coderelay.com".into(),
                access_token: "t".into(),
                ..Default::default()
            },
        );
        c
    }

    fn defaults() -> WaitDefaults<'static> {
        WaitDefaults {
            machine: None,
            transport: None,
            env: None,
            base_dir: "/nonexistent".into(),
            t3_bin: None,
            explicit_bin: false,
        }
    }

    #[test]
    fn relay_url_picks_machine_and_env() {
        let t = resolve_target(
            "https://prod-1.t3coderelay.com/b60c6dd8/mcp%3Alaunch%3Aabc",
            &defaults(),
            &cfg(),
        )
        .unwrap();
        assert_eq!(t.machine, "relay");
        assert_eq!(t.transport, Transport::Mcp);
        assert_eq!(t.thread_id, "mcp:launch:abc");
        assert_eq!(t.environment_id.as_deref(), Some("b60c6dd8"));
    }

    #[test]
    fn t3_thread_link_and_bare_ids() {
        let t = resolve_target("<t3-thread://v2/env1/mcp:launch:abc>", &defaults(), &cfg()).unwrap();
        assert_eq!(
            (t.machine.as_str(), t.thread_id.as_str(), t.environment_id.as_deref()),
            ("relay", "mcp:launch:abc", Some("env1"))
        );
        let t = resolve_target("mcp:launch:abc", &defaults(), &cfg()).unwrap();
        assert_eq!(t.machine, "relay");
        // A `t3` found on PATH alone does not pull bare ids to local.
        let mut on_path = defaults();
        on_path.t3_bin = Some("/usr/local/bin/t3".into());
        assert_eq!(
            resolve_target("mcp:launch:abc", &on_path, &cfg()).unwrap().machine,
            "relay"
        );
        on_path.explicit_bin = true;
        assert_eq!(
            resolve_target("mcp:launch:abc", &on_path, &cfg()).unwrap().transport,
            Transport::Local
        );
        let mut d = defaults();
        d.transport = Some(Transport::Local);
        let t = resolve_target("mcp:launch:abc", &d, &cfg()).unwrap();
        assert_eq!(t.transport, Transport::Local);
        assert_eq!(t.base_dir.as_deref(), Some(std::path::Path::new("/nonexistent")));
    }

    #[test]
    fn thread_arguments_never_keep_or_echo_credentials() {
        // A t3-thread link: the query and fragment are not part of the id.
        let t = resolve_target(
            "t3-thread://v2/env1/mcp:launch:abc?token=QUERY_CANARY#FRAG_CANARY",
            &defaults(),
            &cfg(),
        )
        .unwrap();
        assert_eq!(t.thread_id, "mcp:launch:abc");
        assert_eq!(t.environment_id.as_deref(), Some("env1"));
        // Credentials in a URL: refused, and not echoed.
        for bad in [
            "https://user:PASS_CANARY@prod-1.t3coderelay.com/env/mcp%3Alaunch%3Aabc?token=QUERY_CANARY",
            "t3-thread://user:PASS_CANARY@v2/env/x",
            "ftp://prod-1.t3coderelay.com/env/x?token=QUERY_CANARY",
            "mcp:launch:abc?token=QUERY_CANARY",
            "https://prod-1.t3coderelay.com/env/mcp%3Alaunch%3Aabc%3Ftoken%3DQUERY_CANARY",
        ] {
            let e = resolve_target(bad, &defaults(), &cfg()).unwrap_err();
            assert!(!e.contains("CANARY"), "{bad} -> {e}");
        }
    }

    #[test]
    fn unknown_origin_is_an_error_and_loopback_is_local() {
        assert!(resolve_target("https://other.example/e/t", &defaults(), &cfg()).is_err());
        let t = resolve_target("http://127.0.0.1:3773/env/mcp%3Alaunch%3Ax", &defaults(), &cfg()).unwrap();
        assert_eq!(t.transport, Transport::Local);
        assert_eq!(t.thread_id, "mcp:launch:x");
    }

    #[test]
    fn local_paths_are_made_absolute_here() {
        let (base, bin, _) = resolve_local_paths(Some("."), None).unwrap();
        assert!(base.is_absolute());
        assert_eq!(base, std::fs::canonicalize(".").unwrap());
        assert!(bin.as_ref().is_none_or(|b| b.is_absolute()));
        assert!(resolve_local_paths(Some("."), Some("no/such/t3")).is_err());
    }

    #[test]
    fn args() {
        let raw: Vec<String> = [
            "a",
            "--mode=any",
            "-m",
            "x",
            "--pretty",
            "b",
            "--on-settle-arg",
            "--v",
            "--on-settle-arg=2",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let a = parse_args(&raw, &["mode", "machine", "on-settle-arg"], &["pretty"]).unwrap();
        assert_eq!(a.positional, ["a", "b"]);
        assert_eq!(a.get("mode"), Some("any"));
        assert_eq!(a.get("machine"), Some("x"));
        assert_eq!(a.get_all("on-settle-arg"), ["--v", "2"]);
        assert!(a.has("pretty"));
        assert!(parse_args(&raw, &["mode"], &[]).is_err());
    }
}
