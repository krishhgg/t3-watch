# t3-watch

Wait for T3 Code threads to finish without spending AI tokens.

When an agent (Claude Code, Cursor, Codex, Grok Bot, a shell script) starts a
long T3 thread through MCP, it usually either polls `t3_thread_read` or keeps
a subagent open until the thread finishes. Both burn context and tokens.
`t3-watch wait` does the waiting in the operating system instead. The agent
starts it as a background command and ends its turn. When the watched threads
finish, fail, or stop to ask a question, the command prints one line of JSON
and exits with a code that says which, and the harness wakes the agent once.

```sh
t3-watch wait mcp:launch:4c5afd1c... --expect-min-run-ordinal 2
# ... minutes later ...
{"event":"settled","outcome":"completed","until":"turn","mode":"all","elapsedMs":81234,"via":"daemon","threads":[{"machine":"local","threadId":"mcp:launch:4c5afd1c...","status":"completed","outcome":"completed","settled":true,"latestRunId":"run:thread:...:ordinal:2","runOrdinal":2,"title":"Fix the flaky test"}],"startedAt":"2026-11-06T18:01:02.000Z","endedAt":"2026-11-06T18:02:23.234Z","watchId":"5d1c..."}
```

- One 1.7 MB binary with TLS built in (rustls). No Python or Node runtime, 26 runtime crates (29 with build-time ones), no proc-macros.
- Thin clients share one background daemon per user. Twenty agents watching one thread cost one upstream long-poll.
- Watches threads on this machine through T3's own CLI with no setup, and on other machines over T3's Streamable HTTP MCP endpoint with a read-only OAuth token.
- Two completion contracts: a turn (the latest run finished) or a whole workstream (the run, its delegated work, and the parent runs that deliver the results, proven by T3 itself). See [Completion contracts](#completion-contracts).
- A question or approval that blocks a thread wakes the caller with its own outcome and exit code (5), even while the run still says `running`. Failed, interrupted and cancelled runs exit 6, never 0.
- Rides out Cloudflare tunnel errors (502, 504, 530 / error 1033 when a laptop sleeps, a tunnel's challenge-less 401), and expired MCP sessions without waking anyone, and fails fast with exit 2 when a token is really revoked.
- Can also wake a bot that has no background terminal: a signed HTTPS webhook and/or a local program, run when the wait ends. With `--durable`, an undelivered webhook is kept on disk and retried until a 2xx.

## Install

From source (Rust 1.85 or newer):

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
user as the T3 server. `--base-dir`, `--t3-bin`, `$T3CODE_HOME` and
`$T3_WATCH_T3_BIN` are resolved to absolute paths by the waiting client, so a
relative path means the same file however the daemon was started.

The local CLI cannot prove that delegated work has settled, so
`--until workstream` is refused (exit 2) for local threads instead of
silently doing less. `--final-marker` works locally (see below).

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

To skip a run you already know about without doing ordinal arithmetic, pass
its id: `--after-run run:thread:...:ordinal:5` waits for a run newer than 5
(one thread per wait). Every result names `latestRunId` (and `resultRunId`
in workstream mode), so a bot can feed the last one back.

When retries are expected (a failed run gets resent, and the resend becomes
a new run), `--follow` keeps one watcher across them: a failed, interrupted
or cancelled run is reported as `run_failed` progress (a webhook and a line
on stderr), and the wait goes on until a later run of the thread completes,
asks for input, or the timeout passes. The final result lists the failed
runs in `failedRuns`.

## Completion contracts

A T3 turn and the work it starts are different things. On 2026-11-06 a
research thread's run 2 finished its own turn after launching four reviewer
threads; their completions came back as parent runs 3 to 5, and the
synthesis was run 6. A watcher waiting for "run 2 or later, finished" exited
at run 2, as it was asked to. t3-watch now offers two explicit contracts:

| `--until` | Settles when | Proof | Transports |
| --- | --- | --- | --- |
| `turn` (default) | The latest run of the thread (at least `--expect-min-run-ordinal`) has a terminal status and no run is active | `t3_thread_read` (MCP) or `t3 thread show` (local) | both |
| `workstream` | The run, its delegated and background tasks, and the parent runs that deliver their completions have all settled | T3's own `t3_thread_wait` with `untilDelegatedWorkSettled`, whose `resultRun` names the run with the final answer | MCP only |

Turn mode is unchanged for ordinary single turns. Over MCP it also asks T3,
without waiting (`t3_thread_wait` with `timeoutMs: 1`), whether the finished
turn left delegated work running, and reports `delegatedWorkPending: true`
with a diagnostic when it did. That is a hint; it never changes the result.

Workstream mode settles only when all of these hold, checked on each cycle:

1. `t3_thread_wait` returned `timedOut: false` with a settled status, for
   this thread (`threadId` matches when present).
2. It named `resultRun`, and that run is a run of this thread
   (`run:thread:<this thread>:ordinal:N`) whose status, when given, is
   settled. A run of another thread (a child's) never counts.
3. The header read afterwards shows a settled status, no `activeRunId`, and
   a latest run no newer than `resultRun`, or the same latest run a read had
   already seen before this wait was sent (a wait without `runId` covers the
   latest run and its delegated work, so T3 may name an earlier run as the
   one holding the answer).

If a newer run appears (a late child completion continuing the parent),
t3-watch waits again on it. If T3 finishes the wait without `resultRun`,
nothing can prove the workstream, and the result is an error
(`error.kind = "unsupported"`, exit 2), never a completion.

What this cannot prove: that nobody will start a new run later. A user
message or a delegated task created after T3 reported the workstream settled
starts a new workstream. And the proof is only as good as T3's own
bookkeeping of delegated work.

`--final-marker <TEXT>` is an explicit final-result contract for both modes
and both transports: the final assistant message of the deciding run (the
latest run, or `resultRun` in workstream mode) must contain TEXT. Put the
marker in the instructions ("end your final synthesis with
`SYNTHESIS-COMPLETE`"); until it appears, a completed status keeps waiting,
with `finalMarkerSeen: false` and a diagnostic. A failed run ends the wait
without the marker. Over MCP the message is found with a one-character
listing of the last 100 timeline items and then read alone, page by page.
T3 marks a delegated child's final result as delivered when it is read, so
`--final-marker` refuses child threads (exit 2, `unsupported`); watch the
parent instead.

## When a thread needs input

A thread blocked on a question or an approval never finishes by itself. The
wait ends at once with `event` and `outcome` `needs_input` and exit 5, in
either completion mode, when the thread's status is `waiting` or its
`pendingRequestCount` is above zero (an approval can block a run that still
says `running`). That is never reported as completion, and t3-watch never
answers or approves anything: it can only call `t3_thread_wait`,
`t3_thread_read` and `t3_pending_request` with `action` `list` or `read`.

Over MCP each pending request is described from the end of the activity
timeline and `t3_pending_request list` (which lists questions only, not
approvals):

```json
{"kind":"question","itemId":"turn-item:runtime-request:...","requestId":"runtime-request:...","userOnly":false,
 "questions":[{"header":"Deploy","question":"Use the staging key for this run?","options":["Yes (Recommended)","No"],"multiSelect":false}]}
```

- `kind` is `question`, `approval` or `request`. Approvals have
  `userOnly: true`: a bot may answer questions, but approvals are for the
  user in T3.
- Question text is cut to 600 characters, option labels to 120, option
  descriptions are dropped, and anything that looks like a credential (key
  prefixes such as `sk-` or `ghp_`, JWTs, long mixed alphanumeric runs,
  `password=`/`token:` values, URL userinfo, the word after `Bearer`) is
  replaced with `[redacted]`.
- The thread gets a `pendingKey`. Once the caller has passed the request on,
  `--acknowledge <pendingKey>` keeps waiting past that exact set of
  requests; a different or additional request wakes it again.
- The local transport sees `waiting` and `pendingRequestCount` but has no
  way to read the request itself, so `requests` is empty there.
- A request that appears while the run still says `running` is noticed by
  the read after each long-poll slice, so within `--slice` (default 60 s).

### Wake notifications

An agent with a background terminal is woken by the process exiting. A bot
without one can ask t3-watch to call it. The waiting client (never the
shared daemon) delivers the notification after the wait ends and before it
exits.

```sh
export BOT_HOOK_URL='https://bot.example/hooks/t3'      # keep URLs and secrets out of argv
export BOT_HOOK_SECRET='...'
t3-watch wait mcp:launch:abc123 \
  --webhook-url-env BOT_HOOK_URL --webhook-secret-env BOT_HOOK_SECRET \
  --webhook-header-env Authorization=BOT_HOOK_AUTH \
  --on-settle /usr/local/bin/wake-bot --on-settle-arg --channel --on-settle-arg ops
```

- The webhook gets `POST` with the result JSON as the body.
  `X-T3-Watch-Event` is the event (`settled`, `needs_input`, `timeout`,
  `error`, or progress: `run_failed`, `upstream_unreachable`,
  `upstream_restored`, `heartbeat`). `X-T3-Watch-Id` and `Idempotency-Key`
  carry the same id on every retry of one event, inline or from the outbox;
  `X-T3-Watch-Attempt` counts the attempts. With a secret,
  `X-T3-Watch-Timestamp` and `X-T3-Watch-Signature: sha256=<hex>` carry an
  HMAC-SHA256 over `<timestamp>.<id>.<body>`, fresh for each attempt.
- The URL must be `https://` (plain `http://` only on 127.0.0.1, [::1] or
  localhost). Redirects are not followed. Network errors, 408, 429 and 5xx are
  retried, at most 4 attempts within 60 s, honoring a `Retry-After` in seconds;
  if that would overrun the 60 s, delivery stops. Other 4xx fail at once.
- `--webhook-url-env`/`-file`, `--webhook-header-env` and
  `--webhook-secret-env`/`-file` read their values before the daemon or any
  hook starts, and the variables are removed from the environment so no child
  process inherits them. `--webhook-url` also works but puts the URL in `ps`.
  A `-file` must be a regular file owned by you with no group or other
  access (`chmod 600`); a symlink is followed and its target checked, and a
  FIFO or device is refused without blocking.
  Header names and values with CR, LF or NUL are refused. Output reports
  attempts, status and an error category only, never the URL, headers, secret
  or response body.
- `--on-settle` runs the program directly (no shell) with the
  `--on-settle-arg` values, the result JSON on stdin and `T3_WATCH_EVENT` in
  its environment. Its stdout and stderr go to `hooks.log` in the state
  directory (0600), never to t3-watch's stdout or stderr: a hook that leaves
  a background process holding an inherited pipe would otherwise keep a
  harness that reads t3-watch's output waiting after t3-watch has exited.
  After `--on-settle-timeout` (default 60 s) its whole process group is
  killed.
- `--notify-on` takes `all` or a comma list of `settled`, `needs_input`,
  `timeout`, `error`, `run_failed` and `upstream`. The default is everything
  but `timeout` and `error`; `all` adds those, so an expired token cannot
  leave an unattended bot waiting forever. Once the notification options
  themselves are valid, this includes errors before the wait starts: an
  unreadable machines.json, an unknown machine, a bad `--mode` or duration.
- When the threads completed but a notification failed (or was only
  queued), the result adds `"notification":{"delivered":false,...}` and the
  exit code is 4. Exit codes 5 and 6 are kept as they are; check
  `notification.delivered` there.

### Durable delivery

The legacy shell watcher this replaces timed out, then lost its only
notification to a DNS failure (`Could not resolve host`) with no retry.
With `--durable`, a webhook is never fire-and-forget:

1. Before the first attempt, the event is written to
   `<state>/outbox/<id>.json` (0600 in a 0700 directory, through an fsynced
   temp file and a rename). A crash from here on loses nothing.
2. The waiting client tries it inline (up to 4 attempts within 60 s). A 2xx
   removes the entry.
3. Otherwise the client exits (4 if the threads completed) with
   `notification.queued: true` and the outbox id, and makes sure the daemon
   runs. The daemon's sender retries every entry with exponential backoff
   (2 s doubling, with jitter, capped at 5 min, `Retry-After` honored) until
   a 2xx, then removes it. DNS failures, refused connections, 5xx and 4xx
   alike are retried; after `--durable-max-age` (default 7 days) the entry
   moves to `outbox/dead/`.
4. The daemon does not idle-exit while the outbox holds entries. One sender
   runs at a time (`outbox/sender.lock`).

Every attempt for an entry carries the same `X-T3-Watch-Id` and
`Idempotency-Key`. A receiver that processed a request whose answer was lost,
or a sender that crashed between the 2xx and removing the entry, produces a
duplicate with the same id; receivers should drop repeats of an id they
have seen.

```sh
t3-watch outbox list            # ids, events, destination origin, attempts, next attempt, last error
t3-watch outbox run --once      # try every entry now (exit 4 while some remain)
t3-watch outbox run             # the sender in the foreground, for launchd/systemd
t3-watch outbox drop <id>
```

An entry holds the webhook URL, extra headers and signing secret, protected
like the OAuth tokens in machines.json; `outbox list` shows only the
destination's origin. If the machine reboots, nothing resends until a
t3-watch command starts the daemon again (any `wait` does), unless
`t3-watch outbox run` is supervised by launchd or systemd.

### Outages, heartbeats and a dead host

t3-watch separates what it can prove from what only someone else can:

- **T3 unreachable from the watcher.** Network errors, 5xx, Cloudflare 530
  / 1033 and a 401 or 403 without a `WWW-Authenticate: Bearer` challenge
  (what a tunnel answers while its origin is away, for example
  `Unauthorized: Tunnel not found`) are an outage, not an auth failure. The
  watcher keeps retrying, and since only a fresh proof can settle a wait, an
  outage never ends one as completed. Each thread in the result shows
  `upstream: {reachable: false, unreachableSince, unreachableForMs,
  consecutiveFailures, lastError}` while it lasts, `t3-watch status` shows
  the same per watch, and `--notify-unreachable <dur>` sends
  `upstream_unreachable` progress once an outage has lasted that long and
  `upstream_restored` when it ends.
- **The watcher (or its host) is gone.** A dead or powered-off host cannot
  report its own death. `--heartbeat <dur>` POSTs a signed `heartbeat`
  event every `<dur>` with the `watchId`, host, pid, `seq`, start time and
  each watch's upstream state; the final result carries the same `watchId`.
  A receiver-side monitor that has seen no heartbeat for about two intervals
  and no final event should treat the watcher as gone and restart it
  elsewhere. Heartbeats are best effort and never queued: a missing one is
  the signal.


### Options

| Option | Meaning |
| --- | --- |
| `-m, --machine <name>` | Saved machine to use; `local` forces the local CLI |
| `--transport auto\|mcp\|local` | Override transport detection |
| `--env <envId>` | `environmentId` for a peer-linked environment (URLs set it for you) |
| `--mode all\|any` | Wait for every thread (default) or the first |
| `--until turn\|workstream` | Completion contract (default `turn`; see [Completion contracts](#completion-contracts)) |
| `--until-delegated-settled` | Same as `--until workstream` (MCP only; refused for local threads) |
| `--expect-min-run-ordinal <N>` | Ignore runs older than ordinal N |
| `--after-run <runId>` | Ignore that run and older ones (one thread) |
| `--final-marker <text>` | Also require this text in the final assistant message of the deciding run |
| `--follow` | Report a failed run as progress and keep following the thread |
| `--acknowledge <pendingKey>` | Keep waiting past the pending requests this key names |
| `--timeout <dur>` | Give up after this long (default `4h`) |
| `--interval <dur>` | Local poll interval (default `5s`) |
| `--slice <dur>` | MCP long-poll length (default `60s`, capped at `10m`) |
| `--no-daemon` | Watch in this process instead of the shared daemon |
| `--pretty` | Indent the JSON |
| `--webhook-url-env`, `--webhook-url-file`, `--webhook-url` | Where to POST the result (see above) |
| `--webhook-header-env <H>=<VAR>`, `--webhook-secret-env`, `--webhook-secret-file` | Extra header and signing secret |
| `--on-settle <prog>`, `--on-settle-arg <arg>`, `--on-settle-timeout <dur>` | Program to run with the result on stdin |
| `--notify-on <classes>\|all` | Which events notify (default: all but `timeout` and `error`) |
| `--durable`, `--durable-max-age <dur>` | Keep an undelivered webhook in the outbox and retry it until a 2xx |
| `--heartbeat <dur>` | POST a signed heartbeat every `<dur>` while waiting |
| `--notify-unreachable <dur>` | Report an upstream outage once it lasts `<dur>`, and its end |

Durations accept `90`, `90s`, `1500ms`, `5m`, `4h`, `1h30m`.

### Output and exit codes

stdout is always one JSON object.

| Exit | `event` | `outcome` | Meaning |
| --- | --- | --- | --- |
| 0 | `settled` | `completed` | Every awaited thread (or the first, with `--mode any`) completed: status `completed` or `idle`, with the contract's requirements met |
| 6 | `settled` | `failed` | A thread ended `failed`, `error`, `interrupted`, `cancelled`, `stopped` or `rolled_back` |
| 5 | `needs_input` | `needs_input` | A question or approval waits on a thread (see [When a thread needs input](#when-a-thread-needs-input)) |
| 3 | `timeout` | `timeout` | `--timeout` passed first. Each thread shows its last known `status`, `lastError` and `upstream` |
| 2 | `error` | `error` | Hard failure. `error.kind` is `auth` (a token rejection with a bearer challenge that a reload or refresh could not fix), `not_found`, `unsupported`, `fatal`, `usage`, `config`, `limit`, `resource`, `protocol` or `daemon` |
| 4 | `settled` | `completed` | Completed, but a configured notification could not be delivered or is only queued |
| 1 | | | Internal |

Statuses are matched against two explicit lists. Active ones (`preparing`,
`queued`, `starting`, `running`, `resuming`, `cancelling`, `interrupting`,
`stopping`, `rolling_back`, `pending`) keep waiting. A status in neither list,
or an empty one, never counts as settled; the thread gets a `diagnostic`
explaining why.

Each entry in `threads` has `machine`, `threadId`, `status`, `outcome`,
`settled`, `latestRunId`, `runOrdinal`, and when known `activeRunId`,
`title`, `link`, `parentThreadId`, `pendingRequestCount`, `pendingKey`,
`requests`, `resultRunId`, `delegatedSettled`, `delegatedWorkPending`,
`finalMarkerSeen`, `upstream`, `lastError` and `diagnostic`.

The top level also has `until`, `startedAt` and `endedAt` (UTC, from the
waiting client), `elapsedMs` (the client's whole wait), `watchId`, and after
a daemon restart mid-wait `daemonReconnects` and `registrationElapsedMs`
(the daemon's time since the last re-registration).

## How it works

```
 agent A ─┐                                   ┌─ t3_thread_wait (long-poll) ─┐
 agent B ─┼─ t3-watch wait ─▶ daemon-p2.sock ▶ hub ┤   then t3_thread_read     ├─▶ relay /mcp
 agent C ─┘   (thin client,     (0600)          │  one worker per thread,     │   (one Mcp-Session-Id
               blocks in read)                  │  shared by all subscribers  │    per machine)
                                                └─ t3 thread show every N s ──┴─▶ local t3 CLI
```

- The first `t3-watch wait` starts `t3-watch daemon` in its own session
  (`setsid`), with stdout on `/dev/null`, so the harness never waits on it.
  The daemon listens on `daemon-p2.sock` in a 0700 directory
  (`$T3_WATCH_HOME`, else `$XDG_RUNTIME_DIR/t3-watch`, else
  `~/.config/t3-watch`). A lock file keeps it to one per socket. The number
  in the name changes with the protocol, so a daemon left by an older
  t3-watch (which would ignore new options and report a failed run as
  success) never serves a newer client; the old one idles out on its own.
- Each client sends one request line and blocks in `read()`. It uses no CPU
  and no network until the answer arrives.
- The daemon keeps one worker per watch key: (origin, pairing, thread,
  environment, completion mode, final marker) over MCP, (data dir, `t3`
  binary, thread, final marker) locally. Keys never use the machine name, so renaming or re-pairing a
  machine cannot reuse a stale session. Every client evaluates its own
  condition (`mode`, `--expect-min-run-ordinal`) against the worker's shared
  observations, and the result it prints comes from the same snapshot the
  decision used.
- A client only accepts observations made after it subscribed, so a cached
  `completed` from an earlier turn can never wake it.
- The daemon exits 60 seconds after its last client leaves, unless the
  outbox still holds notifications. A killed client is noticed at once (its
  socket closes) and unsubscribed. Progress (`run_failed`, outages) reaches
  the client as extra lines before the final one.
- When a watch loses its last subscriber, its upstream call is cancelled for
  real, not left to finish: the cycle's HTTP agent is shut down (interrupting
  a blocked long-poll), the server gets MCP `notifications/cancelled` for the
  request, and a local `t3 thread show` has its process group killed.
- Every live upstream operation holds a slot in one global budget of
  `T3_WATCH_MAX_WATCHES` (default 1024) until it has actually ended, so
  cancelled calls cannot pile up past it. `t3-watch status` reports
  `upstreamOps.live`, `max` and `cancelled`.
- The daemon serves at most 512 waiting clients and 1024 watches
  (`T3_WATCH_MAX_CONNECTIONS`, `T3_WATCH_MAX_WATCHES`); past that, a wait
  fails with `error.kind = "limit"`. `status` and `stop` do not count against
  the limit, so a full daemon can still be inspected and stopped. Requests and replies are single
  newline-terminated lines with size limits and I/O timeouts.
- If the daemon dies mid-wait, clients reconnect, start a new one and
  re-register with the remaining time. `--no-daemon` runs the same engine
  in-process.

### The MCP transport

- Speaks MCP `2025-03-26` Streamable HTTP: `initialize`,
  `notifications/initialized`, then `tools/call`, with `POST <origin>/mcp`,
  `Accept: application/json, text/event-stream` and `Authorization: Bearer`.
  The server must answer `initialize` with protocol `2025-03-26` or
  `2025-06-18`; with `2025-06-18`, every later request, including
  `notifications/initialized`, carries `MCP-Protocol-Version`. Both JSON and
  SSE responses are parsed, and `Mcp-Session-Id` is reused across every watch
  on the same pairing. Each poll cycle uses its own connection, so it can be
  cancelled on its own.
- Requests the server sends inside an SSE response are answered: `ping` with
  an empty result, anything else with "method not found" (t3-watch declares
  no client capabilities), at most 16 per response.
- The origin must be `https://` (plain `http://` only on loopback), so a
  bearer token never crosses a network in clear text.
- Responses are size-bounded: 1 MiB per SSE line, 8 MiB per event, 16 MiB per
  response, comments included. A server that goes past that gets a retry,
  not the daemon's memory.
- Each cycle calls `t3_thread_wait` (`timeoutMs` = the slice, plus
  `untilDelegatedWorkSettled` in workstream mode and `environmentId` when
  set), then always reads the thread header with `t3_thread_read`
  (`afterPosition` past the end, so no timeline item is returned: reading a
  delegated child's final result would mark it delivered). Reading after
  every slice is how a pending request on a `running` thread is noticed.
- Only when a thread needs input does a cycle read more: the last 40
  activity items (not for a finished child thread) and
  `t3_pending_request list`. `--final-marker` reads one message, as
  described above. Answers are cached until the thread's header changes.
- The workstream proof is described in [Completion contracts](#completion-contracts).
  Missing `timedOut` or `status` fields never count as proof, and a missing
  `latestRunId` is unresolved unless the read reports `runCount: 0`.
- `--expect-min-run-ordinal` only counts a `latestRunId` that belongs to the
  watched thread; a response about another thread never settles.
- Network errors, 408, 425, 429, 5xx (including Cloudflare's 52x and 530),
  a 401 or 403 without a bearer challenge or credential error code, JSON-RPC
  internal errors, and tool errors other than not-found, permission and
  validation errors are retried with exponential backoff and jitter (0.5 to
  1 s at first, capped at 30 s). They never wake the caller as completed;
  the result and `t3-watch status` show the outage (`upstream`).
  A 504 or 524 shortens that watch's long-poll to 90 s, so it finishes before
  Cloudflare's idle limit.
- A 404 or 400 on a known session re-runs `initialize` once and retries. If
  the fresh session gets the same 400, that is a hard error.
- A 401 or 403 counts as a token rejection only with a
  `WWW-Authenticate: Bearer` challenge (RFC 6750) or an `invalid_token`,
  `insufficient_scope` or `invalid_mcp_credential` error in the body, which
  is what the T3 relay sends.
- On such a 401, t3-watch rereads machines.json once (another `auth login` or
  another process's refresh may have replaced the token) and, if the server
  issued a refresh token, refreshes it under the config lock and saves the
  result. The outcome is typed:
  - definitive (no saved credentials, no refresh token, `invalid_grant` or
    another 4xx from the token endpoint, or a replacement token that is
    rejected too): exit 2 with `error.kind = "auth"` and the reason;
  - temporary (network error, 408, 429 or 5xx from the token endpoint, a busy
    config lock, a failed write): the watch keeps going with the usual
    backoff, and `lastError` says why. A refreshed token that could not be
    saved is still used.
  Such a 403 exits 2 at once.

### Security

- Tokens live in `machines.json` (0600) inside the 0700 state directory and
  are never printed, logged or passed on the command line. Every change to
  machines.json (login, `oauth-finish`, `default`, `logout`, token refresh)
  happens under `config.lock`, rereads the file inside the lock, and is
  written through a fresh `O_EXCL` temp file that is fsynced, renamed and
  followed by an fsync of the directory.
- t3-watch only requests `orchestration:read` and refuses servers whose
  metadata lists scopes without it. Whatever a server grants, the MCP client
  refuses any tool call other than `t3_thread_wait`, `t3_thread_read` and
  `t3_pending_request` with `action` `list` or `read`; it cannot answer a
  question, approve, send or interrupt.
- Pending request text is redacted and bounded before it leaves the
  machine (see [When a thread needs input](#when-a-thread-needs-input)).
- Outbox entries hold webhook URLs, headers and signing secrets in 0600
  files in a 0700 directory, like tokens in machines.json; listings show the
  destination origin only.
- Discovery is strict. The protected resource metadata must name
  `<origin>/mcp`, the authorization server metadata (fetched from the RFC 8414
  location, including issuers with a path) must name the same issuer, PKCE
  `S256` must be advertised, and the authorization, token and registration
  endpoints must be `https://` (loopback `http://` excepted). Refresh tokens
  are only requested from servers that list the `refresh_token` grant.
- OAuth uses PKCE S256, a random `state`, and checks the `iss` callback
  parameter when the server sends it. Headless logins expire after an hour,
  a callback URL works once, and `oauth-finish` saves the credentials before
  it deletes the pending login. `oauth-finish` first claims the pending login
  with an atomic rename, so concurrent finishes of one callback exchange the
  code once; a failed exchange or save puts the claim back. The loopback login
  ignores stray requests (a favicon, a stale tab with another `state`) and
  only shows "paired" once the credentials are saved. Its `--timeout` is an
  absolute deadline: it bounds every accept and every header read, so slow or
  endless connections cannot stretch it. Errors from the server show
  its OAuth `error` code at most, never the response body.
- The daemon socket is 0600 inside a 0700 directory, and the daemon runs with
  umask 077 so the socket is never briefly open to others. Clients refuse a
  socket or directory that is not theirs or is open to group or others, even
  with `T3_WATCH_SOCKET`; a directory you choose must already be private and
  is never chmodded. Lock files are opened with `O_NOFOLLOW`, and anything
  that is not a socket at the socket path is left alone. Both ends check the
  other's uid (`getpeereid` on macOS, `SO_PEERCRED` on Linux).

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
  107.32 CPU-seconds for twenty copies of the Python polling watcher. That is
  about 28 times less CPU. The CPU goes to T3's own CLI (about 0.4 s per call);
  the twenty blocked t3-watch clients use none that `/usr/bin/time` can see.
- With a single watcher both tools make about one call per interval, so their
  CPU is about the same. The gain comes from the daemon sharing one poller.
- The 9th call in the twenty-client run comes from the burst of clients
  that registered while the first `t3 thread show` was in flight. Those
  clients wait for an observation newer than their registration, so the
  daemon polls once more right away.
- Memory: 52.2 MiB for twenty clients plus the daemon, against 166.8 MiB for
  twenty Python watchers. A blocked client is 2.4 MiB RSS by `ps`, which is
  above the 1 to 2 MB goal. Its private footprint is 1.27 MiB, and a process
  that only sleeps (`/bin/sleep`) already shows 1.1 MiB RSS on this machine.
- `t3-watch --version` starts in 1.68 ms median, 0.56 ms above the cost of
  running `/usr/bin/true` from the same loop.

The remote (MCP) path was not benchmarked against a live relay because that
needs a browser-approved token. Its deduplication is checked by the
integration test instead: 20 concurrent daemon clients on one thread make
exactly 1 `initialize`, 1 `t3_thread_wait` and 1 `t3_thread_read` upstream
(plus, since turn mode reports pending delegated work, one non-blocking
delegated-work probe).

Measured on `krishs-mini.local` (Apple M6, macOS 27.0.1), rustc 1.99.1 (58cf8f9c3 2026-10-13), Python 3.9.6, T3 `0.1.7-nightly.20261102.4066`, on 2026-11-06.
Raw results: [`bench/results/krishs-mini-2026-11-06.json`](bench/results/krishs-mini-2026-11-06.json). Reproduce with `bench/run.py`.

### Size and startup

| | Result |
| --- | --- |
| Release binary (`opt-level = "z"`, LTO, stripped) | 1,687,264 bytes (1.61 MiB) |
| `t3-watch --version`, median of 300 runs | 1.68 ms (floor: `/usr/bin/true` 1.12 ms) |
| `python3 -c pass`, median of 100 runs | 8.25 ms |
| Python watcher `--help`, median of 100 runs | 16.85 ms |
| `wait` on a settled thread via the warm daemon, stub `t3` (`sh`), median of 100 | 3.40 ms |
| Same with `--no-daemon` | 3.29 ms |
| `wait` on a settled thread via the daemon, real `t3`, median of 10 | 306.0 ms (`t3 thread show` alone: 304.1 ms) |

The benchmarks were measured on an earlier build of the same day; the
current macOS arm64 release binary is 1,803,776 bytes (1.72 MiB), and the
Linux x86_64 and aarch64 ones are 2,345,696 and 2,015,152 bytes.

Startup times are wall clock from a Python `subprocess.run` loop, so they include about 1 ms of fork and exec cost; the `/usr/bin/true` row is that floor.

### Memory of a single process

| Process | Max RSS (median of 20) | Peak footprint (median of 20) |
| --- | --- | --- |
| `/usr/bin/true` (floor) | 1.06 MiB | 1.08 MiB |
| `t3-watch --version` | 2.02 MiB | 1.27 MiB |
| `t3-watch wait` client, settled thread | 2.38 MiB | 1.27 MiB |
| `python3 -c pass` | 7.75 MiB | 4.70 MiB |

A process blocked in `sleep` has 1.1 MiB RSS for `/bin/sleep` and 7.8 MiB for `python3 -c 'time.sleep(30)'`.

### Watching one running thread for 60 s on the real local T3 server

Both tools poll the local `t3` CLI every 8 s (the Python watcher's default) and time out after the window. CPU is user plus system time from `/usr/bin/time -l`, including the `t3` processes each tool started. RSS is the largest of three `ps` samples taken while the watchers were blocked. `t3` calls were counted by a wrapper around the real CLI.

| Watchers | Tool | `t3 thread show` calls | CPU time | Total RSS | RSS per waiting process |
| --- | --- | --- | --- | --- | --- |
| 1 | t3-watch (clients + daemon) | 8 | 3.53 s | 5.3 MiB | client 2.4 MiB, daemon 2.9 MiB |
| 1 | Python watcher | 9 | 4.02 s | 11.7 MiB | 11.7 MiB |
| 20 | t3-watch (clients + daemon) | 9 | 3.89 s | 52.2 MiB | client 2.4 MiB, daemon 3.9 MiB |
| 20 | Python watcher | 160 | 107.32 s | 166.8 MiB | 8.3 MiB |

Each of the 20 t3-watch clients reported 0.00 s of CPU for the whole window, which is below the 10 ms resolution of `/usr/bin/time`. All of t3-watch's CPU time is the daemon and the `t3` processes it ran. The watched thread was still `running` at the end, and every watcher timed out rather than settling, so both tools did the same job.

### Checked against the live T3 server on this Mac mini (completion-contract build)

- `e2e/live.sh` (below), 36 checks, all passed on 2026-11-06 against T3
  `0.1.7-nightly.20261102.4066`: completion with `outcome`, stale ordinals and
  `--after-run`, local refusal of `--until workstream`, a missing
  `--final-marker`, shared polling, cancellation back to baseline threads and
  descriptors, signed webhooks with Authorization and `Idempotency-Key`,
  heartbeats, a durable delivery queued while the receiver was down and
  delivered with the same id by `outbox run --once`, hook output in
  `hooks.log`, and strict discovery against the relay.
- A real watcher on thread `mcp:launch:398d7180...` (local transport,
  `--durable --heartbeat 5m --notify-on all`) reported `outcome: completed`
  at run 3 through the daemon and delivered one signed, authorized `settled`
  webhook to the loopback receiver. An earlier start with a state directory
  whose socket path was too long reported that as an `error` event, delivered
  the same way.
- The live T3 MCP tools answered the requests this build relies on: a header
  read with `afterPosition` past the end returns `items: []` with
  `activeRunId`, `runCount`, `itemCount` and `pendingRequestCount`; an
  `itemId` read pages one message with `textTruncated`/`nextTextOffset`;
  `t3_thread_wait` with `untilDelegatedWorkSettled` and `timeoutMs: 1` answers
  at once, with `resultRun {runId, status}` when settled and without it when
  timed out.

See [docs/live-integration.md](docs/live-integration.md) for the supported
plan against paired machines and real bots.

### Checked against the live T3 server on this Mac mini (earlier build)

- Local transport, read-only `t3 thread show`: a completed thread settles in
  about 0.31 s, a stale `ordinal:1` with `--expect-min-run-ordinal 2` keeps
  waiting until `--timeout` (exit 3), and an unknown thread exits 2 with
  `not_found`. Bare ids and `http://127.0.0.1:3773/<envId>/<threadId>` URLs
  both resolve to the local transport.
- OAuth discovery (`auth discover`) against the real relay
  `https://prod-05055d8a66f766cb.t3coderelay.com` over TLS and against
  `http://127.0.0.1:3773`: both return the expected endpoints with
  `orchestration:read` available.
- Strict discovery (resource and issuer match, PKCE S256, https endpoints)
  accepts both the real relay and the local server. The relay lists no
  `refresh_token` grant, so t3-watch registers for `authorization_code` only.
- MCP with an invalid token against the relay through Cloudflare: exit 2 with
  `error.kind = "auth"` in 0.3 s, including one reread of machines.json. A
  relay thread URL picked the saved machine by its origin.
- `--until-delegated-settled` on a local thread exits 2 before anything runs,
  and an `--on-settle` hook received the 561-byte result on stdin.

Not run live: the browser approval step of `auth login`, and a long-poll
through the relay with a real token. Those paths are covered by the mock
servers in `tests/`, whose responses copy the shapes the real
`t3_thread_wait` and `t3_thread_read` tools returned during development.

## Development

```sh
cargo test                      # unit, integration, hardening, adversarial, workstream and outbox tests (about 30 s)
cargo clippy --all-targets -- -D warnings
python3 bench/run.py --thread <a running local thread id> --settled-thread <a settled one>
```

The integration tests run the real binary against:

- a mock Streamable HTTP MCP server that answers in JSON or SSE, rotates and
  expires `Mcp-Session-Id`, injects Cloudflare 530 (error 1033), 502 and 504
  pages, and blocks `t3_thread_wait` like the real server;
- a mock OAuth authorization server that checks PKCE S256, the redirect URI
  and the requested scope;
- a mock local `t3` script;
- a mock webhook receiver with scripted 3xx, 4xx, 429 and 5xx answers.

They cover JSON and SSE parsing, session re-initialization, retry without
false wakes, fail-fast on 401, `--expect-min-run-ordinal`, `--mode any|all`,
thread URL resolution, 20 concurrent daemon clients costing exactly one
upstream `t3_thread_wait`, local poll dedup, daemon idle exit, killed-client
cleanup, file permissions and both OAuth flows. `tests/hardening.rs` adds
signed webhooks, retries, `Retry-After`, redirects, exit 4, hooks without a
shell, hook timeouts, secret scrubbing, closed stdout, delegated work bound
to the run, unknown statuses, strict discovery, concurrent logins, refresh
and reload on 401, SSE floods, socket and lock-file attacks, re-pairing,
limits, client-side path resolution, IPC framing, the MCP protocol version
check and the loopback login's handling of stray requests.
`tests/workstream.rs` covers the completion contracts: the research
incident (run 2 with four delegated children, synthesis in run 6), a late
child completion after the proof, an earlier `resultRun`, a child's run as
`resultRun`, a single turn, `--final-marker` (missing, split across pages,
local, refused on a child), questions and approvals while `running`,
`--acknowledge`, failed, interrupted and cancelled runs, a tunnel's 401
without a challenge, outages reported both ways and on timeout, `--follow`,
`--after-run` and signed heartbeats. `tests/outbox.rs` covers durable
delivery: a DNS failure kept privately, a receiver that comes back, a
SIGKILL mid-delivery, an answer lost after the receiver accepted (duplicates
with one id), dead letters, and the daemon retrying a needs-input wake
while staying up. `tests/adversarial.rs` adds a delegated child's `resultRun` posing as a newer
parent run, missing run identity, foreign `latestRunId`s, cancelled
long-polls checked on the wire (each cancelled before the next starts) and in
the OS (threads, descriptors, TCP connections back to baseline), killed local
`t3` processes, token refresh failing with 503, an unreachable token
endpoint, `invalid_grant`, concurrent refresh-token rotation, a strict
2025-06-18 server, SSE `ping` and `roots/list` requests, startup errors with
`--notify-on all`, concurrent `oauth-finish` of one login, slow and endless
callback connections, and FIFO, symlinked and world-readable secret files.

CI also builds and tests with Rust 1.85, the minimum supported version.

### Live end-to-end check

`e2e/live.sh` runs against the T3 server on the machine it runs on, through
the real daemon, and is read-only on T3:

```sh
cargo build --release
e2e/live.sh <settled-thread-id> <running-thread-id> target/release/t3-watch [relay-origin]
```

It checks a real completion, a stale run ordinal and `--after-run`, the
local workstream refusal, a missing `--final-marker`, five waits sharing one
poller, killed and timed-out clients leaving no watch, no upstream operation
and no `t3` process behind, cancelled calls, daemon threads and descriptors
back to baseline, signed webhooks (completed, timeout with `--notify-on all`,
heartbeats) delivered to `e2e/receiver.py` on 127.0.0.1 with Authorization
and `Idempotency-Key` verified, a durable delivery that waits for the
receiver to come back, an `--on-settle` hook whose output lands in
`hooks.log`, optionally a thread blocked on input (`WAITING_THREAD=<id>`),
and optionally strict OAuth discovery against a relay. The running thread
must stay active for about two minutes.

## Limitations

- `--until workstream` needs the MCP transport and a T3 that returns
  `resultRun`; without it the wait fails closed (exit 2). Locally, use
  `--final-marker` with instructions that end the final synthesis with the
  marker. Neither contract can know about work started after T3 reported the
  workstream settled.
- Over MCP, a question or approval on a thread that still says `running` is
  seen within one `--slice` (default 60 s), not instantly. The local
  transport reports needs-input without the request text.
- The local transport costs one `t3 thread show` per watched thread per
  interval, about 0.4 s of CPU on this Mac mini. For many local threads, the
  local server's own MCP endpoint should give long-polling instead
  (`t3-watch auth login http://127.0.0.1:3773`). Its OAuth discovery answers
  correctly, but that login needs a browser approval and was not run end to
  end here.
- T3's authorization server currently lists only the `authorization_code`
  grant, so it issues no refresh tokens. When its token expires or is
  revoked, `wait` exits 2 (after rereading machines.json) and you pair again.
  Servers that do issue refresh tokens are refreshed automatically.
- Windows is not supported (Unix sockets, `setsid`).
- Linux is covered by CI (not yet run, since the branch is unpushed) and by
  cross-builds on macOS with zig as the C compiler and linker: x86_64 and
  aarch64 `-unknown-linux-gnu` binaries need glibc 2.17 at most, and clippy
  and the test binaries build for Linux. They have not been executed on Linux
  from this machine.
- Without `--durable`, a notification still being delivered when the
  waiting process is killed is lost. With it, delivery is at least once:
  receivers must deduplicate on `X-T3-Watch-Id` / `Idempotency-Key`. After a
  reboot, queued entries wait for the next t3-watch command (or a supervised
  `t3-watch outbox run`). Heartbeats and progress events from before a
  daemon restart are not replayed.
- A watcher cannot report its own host's death; that needs a receiver-side
  monitor of `--heartbeat` events.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
