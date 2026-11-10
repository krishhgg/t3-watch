# Development

## Build

From source (Rust 1.85 or newer):

```sh
cargo install --path . --locked
```

On a disk-constrained machine, a minimal toolchain is enough:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
  sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path
~/.cargo/bin/cargo build --release   # target/release/t3-watch
```

## Tests

```sh
cargo test                      # unit, integration, hardening, adversarial, workstream, outbox, consistency and delivery tests (about 40 s)
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
while staying up. `tests/consistency.rs` covers a newer running parent
seen by the marker listing, a message page, the post-marker header read
or the turn probe; failed, interrupted and cancelled result runs ahead of,
behind and equal to the header's run; input pending before the first
long-poll with a 900 ms timeout; and a running child with pending input and
an earlier final result, read without touching its timeline.
`tests/compat.rs` covers daemons of other engine revisions (an old
`accepted` line on a shared custom socket, no `accepted` line at all, an old
daemon at the old default socket, this daemon refusing old requests) and
old outbox senders (migration of their entries, this build's entries out of
their reach, and a young entry left by a crashed old client kept pending
and delivered). `tests/delivery.rs` covers closed stdout and stderr together for
`run_failed`, outage progress, completion and needs-input (with the
receiver up and down), `Retry-After` across the hand-off and past the
sender's cap, and the final event persisted while a progress delivery
hangs, a 429 with `Retry-After` recorded while a slow hook still runs, the
sender leaving a live inline delivery alone, the trace and build fields, a
final event that expires unsent reported undelivered (exit 4), no thread
URL query in the trace, and FIFOs at the trace, hook and daemon logs. `tests/adversarial.rs` adds a delegated child's `resultRun` posing as a newer
parent run, missing run identity, foreign `latestRunId`s, cancelled
long-polls checked on the wire (each cancelled before the next starts) and in
the OS (threads, descriptors, TCP connections back to baseline), killed local
`t3` processes, token refresh failing with 503, an unreachable token
endpoint, `invalid_grant`, concurrent refresh-token rotation, a strict
2025-06-18 server, SSE `ping` and `roots/list` requests, startup errors with
`--notify-on all`, concurrent `oauth-finish` of one login, slow and endless
callback connections, and FIFO, symlinked and world-readable secret files.

CI also builds and tests with Rust 1.85, the minimum supported version.

## Live end-to-end check

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

Three more harnesses run isolated, without T3 threads:
`e2e/durable.sh` (receiver outage, daemon restart, SIGKILL during
delivery), `e2e/native/codex/run.sh` (a real `codex app-server` with a
mock model provider) and `e2e/native/claude_interactive.py` (a disposable
interactive Claude Code session). `docs/acceptance-manifest.md` records
their latest runs, each gate's status and the steps that need a person.
