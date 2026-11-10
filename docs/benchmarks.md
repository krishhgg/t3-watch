# Benchmarks

All numbers below are from one run on the maintainer's Mac mini, recorded in the raw
results file. Nothing is estimated. The Python baseline is
[`bench/baseline_python_watcher.py`](../bench/baseline_python_watcher.py), the
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

Measured on the maintainer's Mac mini (Apple M6, macOS 27.0.1), rustc 1.99.1 (58cf8f9c3 2026-10-13), Python 3.9.6, T3 `0.1.7-nightly.20261102.4066`, on 2026-11-06.
Raw results: [`bench/results/mac-mini-2026-11-06.json`](../bench/results/mac-mini-2026-11-06.json). Reproduce with `bench/run.py`.

## Size and startup

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

## Memory of a single process

| Process | Max RSS (median of 20) | Peak footprint (median of 20) |
| --- | --- | --- |
| `/usr/bin/true` (floor) | 1.06 MiB | 1.08 MiB |
| `t3-watch --version` | 2.02 MiB | 1.27 MiB |
| `t3-watch wait` client, settled thread | 2.38 MiB | 1.27 MiB |
| `python3 -c pass` | 7.75 MiB | 4.70 MiB |

A process blocked in `sleep` has 1.1 MiB RSS for `/bin/sleep` and 7.8 MiB for `python3 -c 'time.sleep(30)'`.

## Watching one running thread for 60 s on the real local T3 server

Both tools poll the local `t3` CLI every 8 s (the Python watcher's default) and time out after the window. CPU is user plus system time from `/usr/bin/time -l`, including the `t3` processes each tool started. RSS is the largest of three `ps` samples taken while the watchers were blocked. `t3` calls were counted by a wrapper around the real CLI.

| Watchers | Tool | `t3 thread show` calls | CPU time | Total RSS | RSS per waiting process |
| --- | --- | --- | --- | --- | --- |
| 1 | t3-watch (clients + daemon) | 8 | 3.53 s | 5.3 MiB | client 2.4 MiB, daemon 2.9 MiB |
| 1 | Python watcher | 9 | 4.02 s | 11.7 MiB | 11.7 MiB |
| 20 | t3-watch (clients + daemon) | 9 | 3.89 s | 52.2 MiB | client 2.4 MiB, daemon 3.9 MiB |
| 20 | Python watcher | 160 | 107.32 s | 166.8 MiB | 8.3 MiB |

Each of the 20 t3-watch clients reported 0.00 s of CPU for the whole window, which is below the 10 ms resolution of `/usr/bin/time`. All of t3-watch's CPU time is the daemon and the `t3` processes it ran. The watched thread was still `running` at the end, and every watcher timed out rather than settling, so both tools did the same job.

## Checked against the live T3 server on the maintainer's Mac mini (completion-contract build)

- `e2e/live.sh` ([Development](development.md#live-end-to-end-check)), 37 checks, all passed on 2026-11-06 (Pacific) against T3
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

See [docs/live-integration.md](live-integration.md) for the supported
plan against paired machines and real bots.

## Checked against the live T3 server on the maintainer's Mac mini (earlier build)

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
