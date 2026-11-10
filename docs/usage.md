# Usage

`t3-watch wait` blocks until the threads it watches settle, then prints
one line of JSON and exits:

```sh
t3-watch wait mcp:launch:4c5afd1c... --expect-min-run-ordinal 2
# ... minutes later ...
{"event":"settled","outcome":"completed","until":"turn","mode":"all","elapsedMs":81234,"via":"daemon","threads":[{"machine":"local","threadId":"mcp:launch:4c5afd1c...","status":"completed","outcome":"completed","settled":true,"latestRunId":"run:thread:...:ordinal:2","runOrdinal":2,"title":"Fix the flaky test"}],"startedAt":"2026-11-06T18:01:02.000Z","endedAt":"2026-11-06T18:02:23.234Z","watchId":"5d1c..."}
```

## Threads on this machine

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
silently doing less. `--final-marker` works locally (see
[Completion contracts](completion-contracts.md)).

## Threads on another machine

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

## Waiting for a follow-up turn

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

## Options

| Option | Meaning |
| --- | --- |
| `-m, --machine <name>` | Saved machine to use; `local` forces the local CLI |
| `--transport auto\|mcp\|local` | Override transport detection |
| `--env <envId>` | `environmentId` for a peer-linked environment (URLs set it for you) |
| `--mode all\|any` | Wait for every thread (default) or the first |
| `--until turn\|workstream` | Completion contract (default `turn`; see [Completion contracts](completion-contracts.md)) |
| `--until-delegated-settled` | Same as `--until workstream` (MCP only; refused for local threads) |
| `--expect-min-run-ordinal <N>` | Ignore runs older than ordinal N |
| `--after-run <runId>` | Ignore that run and older ones (one thread) |
| `--final-marker <text>` | Also require this text in the final assistant message of the deciding run |
| `--follow` | Report a failed run as progress and keep following the thread |
| `--acknowledge <pendingKey>` | Keep waiting past the pending requests this key names |
| `--timeout <dur>` | Give up after this long, suspended time included (default `4h`) |
| `--interval <dur>` | Local poll interval (default `5s`) |
| `--slice <dur>` | MCP long-poll length (default `30s`; no long-poll exceeds the 30 s attention interval) |
| `--no-daemon` | Watch in this process instead of the shared daemon |
| `--pretty` | Indent the JSON |
| `--webhook-url-env`, `--webhook-url-file`, `--webhook-url` | Where to POST the result (see [Wake notifications](notifications.md#wake-notifications)) |
| `--webhook-header-env <H>=<VAR>`, `--webhook-secret-env`, `--webhook-secret-file` | Extra header and signing secret |
| `--on-settle <prog>`, `--on-settle-arg <arg>`, `--on-settle-timeout <dur>` | Program to run with the result on stdin |
| `--notify-on <classes>\|all` | Which events notify (default: all but `timeout` and `error`) |
| `--durable`, `--durable-max-age <dur>` | Keep an undelivered webhook in the outbox and retry it until a 2xx |
| `--heartbeat <dur>` | POST a signed heartbeat every `<dur>` while waiting |
| `--notify-unreachable <dur>` | Report an upstream outage once it lasts `<dur>`, and its end |

Durations accept `90`, `90s`, `1500ms`, `5m`, `4h`, `1h30m`.

## Output and exit codes

stdout is always one JSON object.

| Exit | `event` | `outcome` | Meaning |
| --- | --- | --- | --- |
| 0 | `settled` | `completed` | Every awaited thread (or the first, with `--mode any`) completed: status `completed` or `idle`, with the contract's requirements met |
| 6 | `settled` | `failed` | A thread ended `failed`, `error`, `interrupted`, `cancelled`, `stopped` or `rolled_back` |
| 5 | `needs_input` | `needs_input` | A question or approval waits on a thread (see [When a thread needs input](needs-input.md)) |
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

The top level also has `until`, `watchId`, `build` (this binary's
`version`, `engine`, `rev` and `capabilities`), `daemon` (the same for the
daemon that served the wait, with its pid), and after a daemon restart
mid-wait `daemonReconnects` and `registrationElapsedMs` (the daemon's time
since the last re-registration), and `daemonReregisteredAfterSuspend`
when a registration the host slept through was dropped and made again. Times, all from the waiting
client:

- `startedAt`, and `endedAt` (also `observationEndedAt`): when the wait
  began and when its outcome was decided, UTC. `exitAt` (only in the printed
  result, after delivery) and `notification.finishedAt` say when delivery
  ended and the process exited.
- `elapsedMs` counts time the host spent suspended, like `--timeout` does;
  `elapsedAwakeMs` does not; `elapsedWallMs` is `endedAt` minus
  `startedAt`. When the host slept during the wait, `suspendedMs` and a
  `clock` note say for how long. Each thread also has `observedAt` (when the
deciding observation was made) and, over MCP, `runCompletedAt` (T3's own
completion time for the latest run).

## Thread arguments

Thread arguments are parsed as URIs: in a `t3-thread://` link and an
`http(s)` thread URL only the path names the thread, the query and fragment
are ignored, and a URL with credentials (`user:password@`) or another
scheme is refused with an error that does not repeat them. A bare thread id
with `?`, `#`, `@` or whitespace is refused without being echoed.
