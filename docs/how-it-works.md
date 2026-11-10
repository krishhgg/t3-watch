# How it works

```
 agent A ─┐                                   ┌─ t3_thread_wait (long-poll) ─┐
 agent B ─┼─ t3-watch wait ─▶ daemon-e19.sock ▶ hub ┤   then t3_thread_read     ├─▶ relay /mcp
 agent C ─┘   (thin client,     (0600)          │  one worker per thread,     │   (one Mcp-Session-Id
               blocks in read)                  │  shared by all subscribers  │    per machine)
                                                └─ t3 thread show every N s ──┴─▶ local t3 CLI
```

- The first `t3-watch wait` starts `t3-watch daemon` in its own session
  (`setsid`), with stdout on `/dev/null`, so the harness never waits on it.
  The daemon listens on `daemon-e<N>.sock` in a 0700 directory
  (`$T3_WATCH_HOME`, else `$XDG_RUNTIME_DIR/t3-watch`, else
  `~/.config/t3-watch`), where N is the engine revision. A lock file keeps
  it to one per socket.
- The engine revision (`t3-watch --version` prints it, with the version and
  the source revision the binary was built from) names the result
  semantics. Builds that report the same package version have shipped with
  different semantics, so the version alone is not enough. A daemon from
  another engine revision is never trusted: by default it listens on another
  socket and is never reached, and on a shared `T3_WATCH_SOCKET` the client
  checks the `accepted` line and refuses with `error.kind =
  "incompatible_daemon"` (exit 2), dropping the connection so that daemon's
  wait ends too. The client also requires every capability it depends on
  (`outbox-receipts`, `outbox-legacy-drain`, `outbox-legacy-in-place`,
  `outbox-legacy-retain`, `outbox-legacy-retain-held`,
  `outbox-legacy-checked-convert`, `outbox-legacy-ack`,
  `native-claude-code-hooks`, `native-claude-code-requests`,
  `native-codex-app-server`, `native-codex-notifications`,
  `native-codex-kept-counter`, `linux-io-hardening`,
  `native-codex-hook-log`, `native-codex-hook-cursor`, `suspend-aware`,
  `suspend-invalidation`) in that
  line,
  so a daemon that kept an engine number but lacks a behavior is refused
  too. The daemon likewise refuses a request from another revision
  (`incompatible_client`). An old daemon idles out on its own once its
  clients and its own outbox are done; stop it sooner with its own binary's
  `t3-watch stop`. Engine 19 (this build) does not reach engine-18 daemons
  (`0.16.0`, socket `daemon-e18.sock`), engine-17 ones (`0.15.0`,
  `daemon-e17.sock`), engine-16 ones (`0.14.0`,
  `daemon-e16.sock`), engine-15 ones (`0.13.0`,
  `daemon-e15.sock`), engine-14 ones (`0.12.0`,
  `daemon-e14.sock`), engine-13 ones (`0.11.0`,
  `daemon-e13.sock`), engine-12 ones (`0.10.0`,
  `daemon-e12.sock`), engine-11 ones (`0.9.0`,
  `daemon-e11.sock`), engine-10 ones (`0.8.0`,
  `daemon-e10.sock`), engine-9 ones (`0.7.0`,
  `daemon-e9.sock`), engine-8 ones (`0.6.0`,
  `daemon-e8.sock`), engine-7 ones (`0.5.0`,
  `daemon-e7.sock`), engine-6 ones (`0.4.0`,
  `daemon-e6.sock`), engine-5 ones (`0.3.0`, `daemon-e5.sock`) or
  engine-4 ones (`0.2.0` up to `fa10d88`, `daemon-e4.sock`).
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

## The MCP transport

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
- A watch's first cycle reads the thread header before anything else. Each
  cycle then calls `t3_thread_wait` (`timeoutMs` = the slice, at most 30 s,
  plus `untilDelegatedWorkSettled` in workstream mode and `environmentId`
  when set), then always reads the thread header with `t3_thread_read`
  (`afterPosition` past the end, so no timeline item is returned: reading a
  delegated child's final result would mark it delivered). Reading after
  every slice is how a pending request on a `running` thread is noticed.
- Only when a thread needs input does a cycle read more:
  `t3_pending_request list`, and the last 40 activity items (for a
  delegated child, `t3_pending_request read` per question instead).
  `--final-marker` reads one message, as described in
  [Completion contracts](completion-contracts.md), and is followed by
  one more header read. Answers are cached until the thread's header
  changes.
- The workstream proof is described in [Completion contracts](completion-contracts.md).
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

## Suspend and resume

`--timeout` and every pause count time the host spends suspended (Linux
`CLOCK_BOOTTIME`, macOS `CLOCK_MONOTONIC`, which include sleep; the clock
behind most timers does not). The wait loop never sleeps longer than a
second, so after a resume an expired budget ends the wait at once instead
of after the time still left on a sleeping timer. An upstream call in
flight across a suspend (the two clocks drift apart by more than 2 s) is
cancelled and started again at once, since its connection is usually dead
and its own timeout would hold back a completion that happened during the
sleep; so is any call past its budget (the slice plus 60 s). A completion
during a sleep is therefore seen within one fresh cycle after the resume.
Each restart is traced as `watch.restart`, and `t3-watch status` counts
them per watch.

An answer is checked against the suspend before it is used, not only while
it is awaited. An answer that is ready at the moment a suspend shows up
may describe the thread from before the sleep (say run 2 completed, while
run 3 has started since), so it is discarded and the call made again. The
stopwatch for a call starts before the call does, so a suspend at any
point counts. After a resume, a waiting client ignores observations made
before it and checks its own budget before it accepts any completion: a
budget that expired during the sleep ends the wait as `timeout`, never as
a completion that arrived late.

The budget also survives a lost daemon. The client keeps one deadline,
suspended time included, for the whole wait. Each time it registers again
with a new daemon it sends only what is left of that deadline, and once
nothing is left it ends with `timeout` instead of registering again. It
reads the daemon's answer in one-second steps against the same deadline
(plus 30 s for the daemon's own answer), so a daemon that is alive but
silent cannot hold it past its timeout either. It checks again after
connecting, which can itself take a while.

The client checks the daemon's answer the same way when it reads it, so
an answer that sat in a socket buffer while the host slept is caught
too. Read after the deadline, an answer never counts: the wait ends as
`timeout` (traced as `daemon.late`). With time left, any sleep of more
than 2 s while a registration is open voids it at the client's first
look after the resume, whether that look finds an answer, part of one, or
an empty socket. The client looks after every read, so an answer that
trickles in a byte at a time cannot hold these checks back either. An empty socket proves nothing about an answer still on
its way, or about when the daemon made the observation behind it. The
client drops the registration (`daemon.stale`), registers again with what
is left of the deadline, and the daemon answers the new registration only
from observations made after it.
