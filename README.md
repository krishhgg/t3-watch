# t3-watch

Wait for T3 Code threads to finish without spending AI tokens.

When an agent (Claude Code, Cursor, Codex, Grok Bot, a shell script) starts a
long T3 thread through MCP, it usually either polls `t3_thread_read` or keeps
a subagent open until the thread finishes. Both burn context and tokens.
`t3-watch wait` does the waiting in the operating system instead. The agent
starts it as a background command and ends its turn. When the watched threads
leave the active statuses (`preparing`, `queued`, `starting`, `running`), the
command prints one line of JSON and exits, and the harness wakes the agent
once.

```sh
t3-watch wait mcp:launch:4c5afd1c... --expect-min-run-ordinal 2
# ... minutes later ...
{"event":"settled","mode":"all","elapsedMs":81234,"via":"daemon","threads":[{"machine":"local","threadId":"mcp:launch:4c5afd1c...","status":"completed","settled":true,"latestRunId":"run:thread:...:ordinal:2","runOrdinal":2,"title":"Fix the flaky test"}]}
```

- One 1.5 MB binary with TLS built in (rustls). No Python or Node runtime, 26 runtime crates (29 with build-time ones), no proc-macros.
- Thin clients share one background daemon per user. Twenty agents watching one thread cost one upstream long-poll.
- Watches threads on this machine through T3's own CLI with no setup, and on other machines over T3's Streamable HTTP MCP endpoint with a read-only OAuth token.
- Rides out Cloudflare tunnel errors (502, 504, 530 / error 1033 when a laptop sleeps) and expired MCP sessions without waking anyone, and fails fast with exit 2 when a token is revoked.

## Install

From source (Rust 1.89 or newer):

```sh
cargo install --path . --locked
```

Release builds for macOS (arm64, x86_64) and Linux (x86_64, arm64) are
attached to tagged GitHub releases by the CI workflow.

On a disk-constrained machine, a minimal toolchain is enough:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
  sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path
~/.cargo/bin/cargo build --release   # target/release/t3-watch
```

## Usage

### Threads on this machine

If T3 Code runs here (`~/.t3` exists), bare thread ids just work:

```sh
t3-watch wait mcp:launch:abc123
t3-watch wait mcp:launch:abc mcp:launch:def --mode any   # first one to settle
t3-watch wait http://127.0.0.1:3773/<envId>/<threadId>   # a local thread URL
```

The local transport runs `t3 thread show --base-dir <dir> --messages 0 --json
<threadId>` with the newest `~/.t3/runtime/versions/*/t3` (or `t3` on `PATH`)
every `--interval` (default 5s). It needs no OAuth because it runs as the same
user as the T3 server.

### Threads on another machine

Pair once with the machine's T3 Connect relay. t3-watch registers a public
OAuth client and asks only for `orchestration:read`, so the token can read
thread state and nothing else.

```sh
# On a desktop: opens the browser, catches the callback on 127.0.0.1.
t3-watch auth login https://prod-05055d8a66f766cb.t3coderelay.com --name mini

# On a headless agent box: copy-paste flow.
t3-watch auth oauth-start https://prod-05055d8a66f766cb.t3coderelay.com --name mini
#   open authorizeUrl anywhere, approve, copy the URL of the page that fails to load
t3-watch auth oauth-finish 'http://127.0.0.1:8976/callback?code=...&state=...'
```

Then pass a thread URL and t3-watch picks the machine and environment from it:

```sh
t3-watch wait 'https://prod-05055d8a66f766cb.t3coderelay.com/<envId>/mcp%3Alaunch%3Aabc123'
t3-watch wait --machine mini mcp:launch:abc123
```

`t3-watch auth discover <origin>` prints a server's OAuth metadata without
registering anything. `auth list`, `auth default <name>` and `auth logout
<name>` manage saved machines.

### Waiting for a follow-up turn

Right after `t3_thread_send`, a thread can still report the previous run as
`completed`. Pass the ordinal of the run you want (run ids end in
`:ordinal:N`) so the stale state never wakes you:

```sh
t3-watch wait mcp:launch:abc123 --expect-min-run-ordinal 2
```

### Options

| Option | Meaning |
| --- | --- |
| `-m, --machine <name>` | Saved machine to use; `local` forces the local CLI |
| `--transport auto\|mcp\|local` | Override transport detection |
| `--env <envId>` | `environmentId` for a peer-linked environment (URLs set it for you) |
| `--mode all\|any` | Wait for every thread (default) or the first |
| `--expect-min-run-ordinal <N>` | Ignore runs older than ordinal N |
| `--until-delegated-settled` | Also wait for delegated and background work (MCP only) |
| `--timeout <dur>` | Give up after this long (default `4h`) |
| `--interval <dur>` | Local poll interval (default `5s`) |
| `--slice <dur>` | MCP long-poll length (default `240s`) |
| `--no-daemon` | Watch in this process instead of the shared daemon |
| `--pretty` | Indent the JSON |

Durations accept `90`, `90s`, `1500ms`, `5m`, `4h`, `1h30m`.

### Output and exit codes

stdout is always one JSON object.

| Exit | `event` | Meaning |
| --- | --- | --- |
| 0 | `settled` | The threads left the active statuses (`completed`, `waiting`, `failed`, `interrupted`, `cancelled`, `rolled_back`, `idle`) |
| 2 | `error` | Hard failure. `error.kind` is `auth` (401/403, revoked token), `not_found`, `fatal`, `usage` or `config` |
| 3 | `timeout` | `--timeout` passed first. Each thread shows its last known `status` and any `lastError` |

Each entry in `threads` has `machine`, `threadId`, `status`, `settled`,
`latestRunId`, `runOrdinal`, and when known `title`, `link`,
`pendingRequestCount` and `resultRunId`.

## How it works

```
 agent A ─┐                                   ┌─ t3_thread_wait (long-poll) ─┐
 agent B ─┼─ t3-watch wait ─▶ daemon.sock ─▶ hub ┤   then t3_thread_read       ├─▶ relay /mcp
 agent C ─┘   (thin client,     (0600)          │  one worker per thread,     │   (one Mcp-Session-Id
               blocks in read)                  │  shared by all subscribers  │    per machine)
                                                └─ t3 thread show every N s ──┴─▶ local t3 CLI
```

- The first `t3-watch wait` starts `t3-watch daemon` in its own session
  (`setsid`), with stdout on `/dev/null`, so the harness never waits on it.
  The daemon listens on `daemon.sock` in a 0700 directory (`$T3_WATCH_HOME`,
  else `$XDG_RUNTIME_DIR/t3-watch`, else `~/.config/t3-watch`). A lock file
  keeps it to one per socket.
- Each client sends one request line and blocks in `read()`. It uses no CPU
  and no network until the answer arrives.
- The daemon keeps one worker per (machine, thread, environment,
  delegated-flag). Every client evaluates its own condition (`mode`,
  `--expect-min-run-ordinal`) against the worker's shared observations.
- A client only accepts observations made after it subscribed, so a cached
  `completed` from an earlier turn can never wake it.
- The daemon exits 60 seconds after its last client leaves. A killed client is
  noticed at once (its socket closes) and unsubscribed.
- If the daemon dies mid-wait, clients reconnect, start a new one and
  re-register with the remaining time. `--no-daemon` runs the same engine
  in-process.

### The MCP transport

- Speaks MCP `2025-03-26` Streamable HTTP: `initialize`,
  `notifications/initialized`, then `tools/call`, with `POST <origin>/mcp`,
  `Accept: application/json, text/event-stream` and `Authorization: Bearer`.
  Both JSON and SSE responses are parsed, and `Mcp-Session-Id` is reused
  across every watch on the machine.
- Each cycle calls `t3_thread_wait` (`timeoutMs` = the slice, plus
  `untilDelegatedWorkSettled` and `environmentId` when set). If the thread is
  no longer active, it verifies with `t3_thread_read` (`limit: 1`,
  `maxCharsPerItem: 200`) before reporting.
- Network errors, 408, 425, 429, 5xx (including Cloudflare's 52x and 530),
  JSON-RPC internal errors, and tool errors other than not-found, permission
  and validation errors are retried with exponential backoff and jitter (0.5
  to 1 s at first, capped at 30 s). They never wake the caller.
  A 504 or 524 shortens that watch's long-poll to 90 s, so it finishes before
  Cloudflare's idle limit.
- A 404 or 400 on a known session re-runs `initialize` once and retries. If
  the fresh session gets the same 400, that is a hard error.
- 401 and 403 exit 2 at once with `error.kind = "auth"`.

### Security

- Tokens live in `machines.json` (0600) inside the 0700 state directory and
  are never printed, logged or passed on the command line.
- t3-watch only requests `orchestration:read` and refuses servers whose
  metadata lists scopes without it. If a server grants more, t3-watch warns and still only calls
  `t3_thread_wait` and `t3_thread_read`.
- OAuth uses PKCE S256, a random `state`, and checks the `iss` callback
  parameter when the server sends it. Headless logins expire after an hour
  and a callback URL works once.
- The daemon socket is 0600 inside a 0700 directory, so only your user can
  connect.

## Agent integration

[`skills/t3-code-watch/SKILL.md`](skills/t3-code-watch/SKILL.md) is a
drop-in agent skill. It covers when to start a watcher, how to pick the
target, what each exit code means, and recipes for Claude Code
(`run_in_background`), Cursor and Codex background terminals, Grok Bot and
plain shell hooks.

## Benchmarks

All numbers below are from one run on this Mac mini, recorded in the raw
results file. Nothing is estimated. The Python baseline is
[`bench/baseline_python_watcher.py`](bench/baseline_python_watcher.py), the
polling watcher t3-watch replaces, copied unchanged.

What they show:

- Twenty agents watching the same local thread cost 9 `t3 thread show` calls
  and 3.89 CPU-seconds per minute through t3-watch, against 160 calls and
  107.99 CPU-seconds for twenty copies of the Python polling watcher. That is
  about 28 times less CPU. The CPU goes to T3's own CLI (about 0.4 s per call);
  the twenty blocked t3-watch clients use none that `/usr/bin/time` can see.
- With a single watcher both tools make about one call per interval, so their
  CPU is about the same. The gain comes from the daemon sharing one poller.
- The 9th call in the twenty-client run comes from the burst of clients
  that registered while the first `t3 thread show` was in flight. Those
  clients wait for an observation newer than their registration, so the
  daemon polls once more right away.
- Memory: 49.2 MiB for twenty clients plus the daemon, against 178.9 MiB for
  twenty Python watchers. A blocked client is 2.3 MiB RSS by `ps`, which is
  above the 1 to 2 MB goal. Its private footprint is 1.25 MiB, and a process
  that only sleeps (`/bin/sleep`) already shows 1.1 MiB RSS on this machine.
- `t3-watch --version` starts in 1.63 ms median, 0.5 ms above the cost of
  running `/usr/bin/true` from the same loop.

The remote (MCP) path was not benchmarked against a live relay because that
needs a browser-approved token. Its deduplication is checked by the
integration test instead: 20 concurrent daemon clients on one thread make
exactly 1 `initialize`, 1 `t3_thread_wait` and 1 `t3_thread_read` upstream.

Measured on `krishs-mini.local` (Apple M6, macOS 27.0.1), rustc 1.99.1 (58cf8f9c3 2026-10-13), Python 3.9.6, T3 `0.1.7-nightly.20261102.4066`, on 2026-11-06.
Raw results: [`bench/results/krishs-mini-2026-11-06.json`](bench/results/krishs-mini-2026-11-06.json). Reproduce with `bench/run.py`.

### Size and startup

| | Result |
| --- | --- |
| Release binary (`opt-level = "z"`, LTO, stripped) | 1,587,120 bytes (1.51 MiB) |
| `t3-watch --version`, median of 300 runs | 1.63 ms (floor: `/usr/bin/true` 1.11 ms) |
| `python3 -c pass`, median of 100 runs | 8.22 ms |
| Python watcher `--help`, median of 100 runs | 16.83 ms |
| `wait` on a settled thread via the warm daemon, stub `t3` (`sh`), median of 100 | 3.30 ms |
| Same with `--no-daemon` | 3.25 ms |
| `wait` on a settled thread via the daemon, real `t3`, median of 10 | 302.0 ms (`t3 thread show` alone: 299.9 ms) |

Startup times are wall clock from a Python `subprocess.run` loop, so they include about 1 ms of fork and exec cost; the `/usr/bin/true` row is that floor.

### Memory of a single process

| Process | Max RSS (median of 20) | Peak footprint (median of 20) |
| --- | --- | --- |
| `/usr/bin/true` (floor) | 1.06 MiB | 1.08 MiB |
| `t3-watch --version` | 2.00 MiB | 1.25 MiB |
| `t3-watch wait` client, settled thread | 2.30 MiB | 1.25 MiB |
| `python3 -c pass` | 7.75 MiB | 4.70 MiB |

A process blocked in `sleep` has 1.1 MiB RSS for `/bin/sleep` and 7.7 MiB for `python3 -c 'time.sleep(30)'`.

### Watching one running thread for 60 s on the real local T3 server

Both tools poll the local `t3` CLI every 8 s (the Python watcher's default) and time out after the window. CPU is user plus system time from `/usr/bin/time -l`, including the `t3` processes each tool started. RSS is the largest of three `ps` samples taken while the watchers were blocked. `t3` calls were counted by a wrapper around the real CLI.

| Watchers | Tool | `t3 thread show` calls | CPU time | Total RSS | RSS per waiting process |
| --- | --- | --- | --- | --- | --- |
| 1 | t3-watch (clients + daemon) | 8 | 3.49 s | 5.0 MiB | client 2.3 MiB, daemon 2.7 MiB |
| 1 | Python watcher | 9 | 3.99 s | 11.7 MiB | 11.7 MiB |
| 20 | t3-watch (clients + daemon) | 9 | 3.89 s | 49.2 MiB | client 2.3 MiB, daemon 3.9 MiB |
| 20 | Python watcher | 160 | 107.99 s | 178.9 MiB | 9.0 MiB |

Each of the 20 t3-watch clients reported 0.00 s of CPU for the whole window, which is below the 10 ms resolution of `/usr/bin/time`. All of t3-watch's CPU time is the daemon and the `t3` processes it ran. The watched thread was still `running` at the end, and every watcher timed out rather than settling, so both tools did the same job.

### Checked against the live T3 server on this Mac mini (final build)

- Local transport, read-only `t3 thread show`: a completed thread settles in
  about 0.31 s, a stale `ordinal:1` with `--expect-min-run-ordinal 2` keeps
  waiting until `--timeout` (exit 3), and an unknown thread exits 2 with
  `not_found`. Bare ids and `http://127.0.0.1:3773/<envId>/<threadId>` URLs
  both resolve to the local transport.
- OAuth discovery (`auth discover`) against the real relay
  `https://prod-05055d8a66f766cb.t3coderelay.com` over TLS and against
  `http://127.0.0.1:3773`: both return the expected endpoints with
  `orchestration:read` available.
- MCP with an invalid token, against the relay through Cloudflare and against
  the local server: exit 2 with `error.kind = "auth"`, in 102 ms for the relay. A relay
  thread URL picked the saved machine by its origin.

Not run live: the browser approval step of `auth login`, and a long-poll
through the relay with a real token. Those paths are covered by the mock
servers in `tests/`, whose responses copy the shapes the real
`t3_thread_wait` and `t3_thread_read` tools returned during development.

## Development

```sh
cargo test                      # unit + integration tests (about 6 s)
cargo clippy --all-targets -- -D warnings
python3 bench/run.py --thread <a running local thread id> --settled-thread <a settled one>
```

The integration tests run the real binary against:

- a mock Streamable HTTP MCP server that answers in JSON or SSE, rotates and
  expires `Mcp-Session-Id`, injects Cloudflare 530 (error 1033), 502 and 504
  pages, and blocks `t3_thread_wait` like the real server;
- a mock OAuth authorization server that checks PKCE S256, the redirect URI
  and the requested scope;
- a mock local `t3` script.

They cover JSON and SSE parsing, session re-initialization, retry without
false wakes, fail-fast on 401, `--expect-min-run-ordinal`, `--mode any|all`,
thread URL resolution, 20 concurrent daemon clients costing exactly one
upstream `t3_thread_wait`, local poll dedup, daemon idle exit, killed-client
cleanup, file permissions, and both OAuth flows.

## Limitations

- `--until-delegated-settled` is only enforced over MCP. The local CLI cannot
  see delegated work, so local results carry a warning instead.
- The local transport costs one `t3 thread show` per watched thread per
  interval, about 0.4 s of CPU on this Mac mini. For many local threads, the
  local server's own MCP endpoint should give long-polling instead
  (`t3-watch auth login http://127.0.0.1:3773`). Its OAuth discovery answers
  correctly, but that login needs a browser approval and was not run end to
  end here.
- T3's authorization server issues no refresh tokens. When a token expires or
  is revoked, `wait` exits 2 and you pair again.
- Windows is not supported (Unix sockets, `setsid`).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
