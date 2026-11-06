//! t3-watch: wait for T3 Code threads to settle without spending AI tokens.

mod config;
mod daemon;
mod engine;
mod local;
mod mcp;
mod model;
mod notify;
mod oauth;
mod util;

use config::Config;
use engine::{error_json, Hub, Outcome, TargetSpec, Transport, WaitRequest, LOCAL_MACHINE};
use model::Predicate;
use notify::{Notify, NotifyArgs};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::exit;
use std::sync::mpsc;
use std::time::Duration;

/// The threads settled but a configured notification could not be delivered.
const EXIT_NOTIFY_FAILED: i32 = 4;

const USAGE: &str = "\
t3-watch: wait for T3 Code threads to settle without spending AI tokens.

USAGE
  t3-watch wait [OPTIONS] <THREAD>...     block until threads leave active statuses
  t3-watch status                         show the daemon's watches and sessions
  t3-watch stop                           stop the daemon
  t3-watch daemon [--idle-exit 60s]       run the multiplexer in the foreground
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
      --expect-min-run-ordinal <N> ignore runs older than ordinal N
      --until-delegated-settled    also wait for delegated work (mcp transport only;
                                   rejected for local threads)
      --timeout <DUR>              give up after DUR (default 4h)
      --interval <DUR>             local poll interval (default 5s)
      --slice <DUR>                mcp long-poll length (default 240s, at most 10m)
      --base-dir <DIR>             T3 data dir for local (default $T3CODE_HOME or ~/.t3)
      --t3-bin <PATH>              T3 CLI for local (default: newest under base dir)
      --no-daemon                  watch in this process instead of the shared daemon
      --pretty                     indent the JSON output

NOTIFICATIONS (run by this process when the wait ends)
      --webhook-url-env <VAR>      POST the result JSON to the https URL in $VAR
      --webhook-url-file <PATH>    ... or to the URL stored in a file
      --webhook-url <URL>          ... or to this URL (visible in `ps`; prefer -env/-file)
      --webhook-header-env <H>=<VAR>  add header H with the value of $VAR (repeatable)
      --webhook-secret-env <VAR>   sign with HMAC-SHA256 using the secret in $VAR
      --webhook-secret-file <PATH> ... or the secret stored in a file
      --on-settle <PROG>           run PROG (no shell) with the result JSON on stdin
      --on-settle-arg <ARG>        pass ARG to PROG (repeatable)
      --on-settle-timeout <DUR>    kill PROG after DUR (default 60s)
      --notify-on <settled|all>    notify on settle only (default) or also on
                                   timeout and error

AUTH OPTIONS
      --name <NAME>                machine name (default from the host)
      --default                    make it the default machine for bare thread ids
      --no-browser                 print the URL instead of opening a browser
      --port <N>                   callback port for login (default: any free port)
      --redirect-port <N>          callback port written into oauth-start (default 8976)

EXIT CODES
  0 settled   2 error (auth revoked, unknown thread, bad usage)   3 timed out
  4 settled, but a notification could not be delivered
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
    eprintln!("t3-watch: {msg}\nRun `t3-watch --help` for usage.");
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
    let mut origin: Option<(String, String)> = None; // (origin, host)
    let (thread_id, env) = if let Some(rest) = arg.strip_prefix("t3-thread://") {
        let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        match segs.as_slice() {
            [_version, env, thread @ ..] if !thread.is_empty() => {
                (util::pct_decode(&thread.join("/"), false), Some(env.to_string()))
            }
            _ => return Err(format!("cannot parse thread link {arg}")),
        }
    } else if let Some(u) = util::parse_url(arg) {
        let segs: Vec<&str> = u.path.split('/').filter(|s| !s.is_empty()).collect();
        let [env, thread @ ..] = segs.as_slice() else {
            return Err(format!("URL has no thread id: {arg}"));
        };
        if thread.is_empty() {
            return Err(format!("URL has no thread id: {arg}"));
        }
        origin = Some((u.origin.clone(), u.host.clone()));
        (util::pct_decode(&thread.join("/"), false), Some(env.to_string()))
    } else {
        (arg.to_string(), None)
    };
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

fn cmd_wait(raw: &[String]) -> ! {
    let a = parse_args(
        raw,
        &[
            "machine",
            "transport",
            "env",
            "mode",
            "expect-min-run-ordinal",
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
            "on-settle",
            "on-settle-arg",
            "on-settle-timeout",
            "notify-on",
        ],
        &["until-delegated-settled", "no-daemon", "pretty"],
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
    })
    .unwrap_or_else(|e| fail_json("usage", &e));

    let transport = match a.get("transport") {
        None | Some("auto") => None,
        Some("mcp") => Some(Transport::Mcp),
        Some("local") => Some(Transport::Local),
        Some(t) => usage_error(&format!("unknown transport {t:?}")),
    };
    let any = match a.get("mode") {
        None | Some("all") => false,
        Some("any") => true,
        Some(m) => usage_error(&format!("unknown mode {m:?}")),
    };
    let min_run_ordinal = a
        .get("expect-min-run-ordinal")
        .map(|s| {
            s.parse::<u64>()
                .unwrap_or_else(|_| usage_error("--expect-min-run-ordinal needs a whole number"))
        })
        .unwrap_or(0);
    let pretty = a.has("pretty");
    let cfg = Config::load().unwrap_or_else(|e| fail_json("config", &e));
    let (base_dir, t3_bin, explicit_bin) =
        resolve_local_paths(a.get("base-dir"), a.get("t3-bin")).unwrap_or_else(|e| fail_json("config", &e));
    let defaults = WaitDefaults {
        machine: a.get("machine"),
        transport,
        env: a.get("env"),
        base_dir,
        t3_bin,
        explicit_bin,
    };
    let targets: Vec<TargetSpec> = a
        .positional
        .iter()
        .map(|t| resolve_target(t, &defaults, &cfg).unwrap_or_else(|e| fail_json("usage", &e)))
        .collect();
    let req = WaitRequest {
        targets,
        any,
        predicate: Predicate {
            min_run_ordinal,
            until_delegated: a.has("until-delegated-settled"),
        },
        timeout: parse_dur(&a, "timeout", Duration::from_secs(4 * 3600)),
        interval: parse_dur(&a, "interval", Duration::from_secs(5)),
        slice: parse_dur(&a, "slice", Duration::from_secs(240)),
    }
    .normalized();
    // Refuse up front what the daemon would refuse, such as delegated-work
    // waits on the local transport, so nothing starts for a doomed request.
    if let Err(e) = req.validate() {
        fail_json("usage", &e);
    }

    let mut outcome = if a.has("no-daemon") || std::env::var_os("T3_WATCH_NO_DAEMON").is_some() {
        let hub = Hub::new("direct");
        let (tx, rx) = mpsc::channel();
        hub.wait(&req, tx, rx)
    } else {
        daemon::wait(&req).unwrap_or_else(|e| Outcome::Error(error_json("daemon", &e)))
    };

    let mut code = outcome.exit_code();
    let event = outcome.event();
    if let Some(v) = outcome.json_mut() {
        let body = v.to_string().into_bytes();
        if let Some(summary) = notify.deliver(event, &body) {
            if summary["delivered"] == false && code == 0 {
                code = EXIT_NOTIFY_FAILED;
            }
            v["notification"] = summary;
        }
    }
    if let Some(v) = outcome.json() {
        print_json(v, pretty);
    }
    exit(code)
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
        Some(sub @ ("login" | "oauth-start" | "oauth-finish")) => run_auth(sub, rest),
        Some("daemon") => {
            let a = parse_args(rest, &["idle-exit"], &[]).unwrap_or_else(|e| usage_error(&e));
            let env_idle = std::env::var("T3_WATCH_IDLE_EXIT")
                .ok()
                .and_then(|s| util::parse_duration(&s).ok());
            let idle = parse_dur(&a, "idle-exit", env_idle.unwrap_or(daemon::DEFAULT_IDLE_EXIT));
            if let Err(e) = daemon::run(idle) {
                eprintln!("t3-watch daemon: {e}");
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
        Some("-V" | "--version" | "version") => write_stdout(&format!("t3-watch {}\n", env!("CARGO_PKG_VERSION"))),
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
