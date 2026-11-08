# Release acceptance matrix: 0.13.0, engine 15

This build is **ready for independent acceptance and not accepted**. The
last code approval was review ordinal 15 of `a52bb9b` (0.12.0, engine 14).
Everything after it, listed below, has not been reviewed. Code approval is
not release acceptance: the gates marked BLOCKED need the parent's bot, a
person, or a machine this host cannot reach. Nothing is pushed, deployed,
installed over a shared binary, or wired to a production bot.

**Evidence kinds**

- **synthetic**: test fixtures (mock MCP or app-server, a fake `t3` CLI, a
  loopback receiver);
- **real, isolated**: real binaries and real Claude Code or Codex
  processes, in disposable sessions, isolated homes and loopback receivers;
- **real, production**: the parent's bot, its routine and its receipts.

An exit code, or an HTTP 200 from a loopback receiver, is never counted as
a bot's continuation.

Times are UTC on 2026-11-08 unless a date is given. The build host is the
maintainer's Mac mini: macOS 27.0.1, arm64, rustc 1.99.1, codex-cli
0.166.0, Claude Code 2.1.319.

## Commits after review ordinal 15 (branch `feat/t3-watch-v1`, unpushed)

| Commit | What |
|---|---|
| `a52bb9b` | 0.12.0 engine 14: code approved by review ordinal 15 (baseline) |
| `0c31bc8` | Review ordinal 15, findings 1 and 3: the outbox-v2 exposure claims are scoped to engines 4, 6, 7 and 8 (engine 5 moves entries whatever their state). The liveness probe's lock is what finds a hung daemon; a test fills a listener's backlog and finds it by its lock |
| `8ad6dfc` | Finding 2, engine 15 (0.13.0): Codex requests counted from `thread/status/changed` notifications (one counting connection per thread), and connections kept between waits mid-turn |
| `08d4b24` | Fixtures for the gates that need another actor; the Codex harness refuses a busy mock port |
| `312fc85` | A Codex thread that cannot list its turns yet (seen once in six real runs while it materializes) is retried, not failed |
| this commit | This matrix; `linux.sh` uses the shipped source copy |

The code is identical from `312fc85` on; only docs and `e2e/` change.

## Artifacts

They live in the worktree's ignored `dist/0.13.0/`, with the logs in
`dist/0.13.0/evidence/`. All of them are built from `312fc85`, and
`--version` prints `t3-watch 0.13.0 (engine 15, rev
312fc8519fd332e0c904d290a0f008605aa1e92d)`. A rebuild is not claimed to
match byte for byte.

| File | SHA-256 | Built with | Executed |
|---|---|---|---|
| `t3-watch-312fc85-aarch64-apple-darwin` (1,970,032 bytes) | `80b73d401c6faffe18991acfc4f3970d98c5e2b6dcfdbc1845e4a129ba3c67e6` | `cargo build --release --locked` | Every real run below |
| `t3-watch-312fc85-universal-apple-darwin` (arm64 + x86_64) | `0a3b828e4a3aeae974f89c2c41977f3baac77261bf5cd7d49911d706cc2e2778` | `lipo` of the above and an x86_64 build | arm64 only (`--version`); x86_64 not run here (no Rosetta) |
| `t3-watch-0.13.0-x86_64-unknown-linux-gnu.tar.gz` | `a493217d5ca39ab8c866e8a815eaf517426173ccec7b6a3f4cf988a2b9587fc7` (binary `a0f6d9df…97cb`) | zig 0.17.0 cross-build, glibc 2.17 or older | **No** |
| `t3-watch-0.13.0-aarch64-unknown-linux-gnu.tar.gz` | `53d8972e3d4d96764989986f49d43c7048d52a15f6ed711b5a1216a5fcd8267b` (binary `e4161778…45f3`) | same | **No** |
| `t3-watch-tests-0.13.0-x86_64-unknown-linux-gnu.tar.gz` | `5c3a3c22f40946ba88722c069e814665e9beab7ac48df52908d36fad3cd73ef3` | the 12 test binaries (LTO off) and the source; `linux.sh` runs them against the archive's binary | **No** |
| `linux.sh` | (copy of `e2e/acceptance/linux.sh`) | | |

## Build checks at `312fc85`

| Check | Toolchain | Result | When | Log |
|---|---|---|---|---|
| `cargo test --locked` | rustc 1.99.1 | **PASS**: 278 passed, 0 failed | 23:17:12–23:18:23 | `evidence/suite-312fc85.txt` |
| `cargo +1.85.1 test --locked` (MSRV) | rustc 1.85.1 | **PASS**: 278 passed, 0 failed | 23:22:20–23:23:38 | `evidence/msrv-312fc85.txt` |
| `cargo fmt --check`; `cargo clippy --locked --all-targets -- -D warnings` | rustc 1.99.1 | **PASS**: clean | 23:22:03 | `evidence/lint-312fc85.txt` |

## Review ordinal 15 findings

| Finding | Outcome |
|---|---|
| 1. `live-integration.md` said every step leaves an entry no more exposed than before | It was overstated. Engine 5 moves `outbox-v2/` entries whatever their state, held ones included. The claim is now scoped to engines 4, 6, 7 and 8 in `live-integration.md`, README item 7 and the code comments (`0c31bc8`) |
| 2. A Codex second approval could be missed for longer than 250 ms, and a brief one entirely | Confirmed, and fixed for every request that opens while a t3-watch connection is open: requests are counted from status notifications, which reach every connection without a resume. The case with nothing connected is an explicit FAIL (X3b) |
| 3. macOS refuses a connect to a full backlog, so a queued connect proves nothing | Confirmed (backlog 1: one connect queued, then `ECONNREFUSED`). The lock decides: in the engine-5 (`9fcc2f8`) and engine-6 (`4caca1b`) sources, the daemon binds its socket lock at the start of `run()` and keeps it until return. New test: `a_hung_engine_5_daemon_with_a_full_backlog_is_found_by_its_lock` |

## Gates

| # | Gate | Kind | Status | Evidence (all with the `312fc85` arm64 artifact unless noted) |
|---|---|---|---|---|
| T1 | The correct bot woken by a real completion webhook, and continuing | real, production | **BLOCKED (parent)** for 0.13.0 | Passed with engine 12 on the mini (parent-held, review ordinal 14, under 1 s; earlier watchId `c2366ae100a095fb`). Needs a rerun with this build: step 1 below |
| T2 | A real `needs_input` notification, and the parent's continuation | real, production | **BLOCKED (parent)** | Mechanism shown by N3 and X2. Step 2 below |
| T3 | Durable delivery through a controlled outage and a daemon restart | real, isolated | **PASS** | `e2e/durable.sh`, 23:18:41–23:19:49. The receiver was down: the client exited 4 with the event queued after 4 attempts. `t3-watch stop`; the entry was still listed with no daemon. Receiver up, daemon restarted: delivered with the same id `a2a305fd…` on attempt 5, 1.9 s later. `evidence/durable-312fc85.txt` |
| T3b | Durable replay to the **real** receiver, and the bot continuing | real, production | **BLOCKED (parent)** | Fixture: `e2e/acceptance/real-receiver-replay.sh` with `e2e/relay.py` (step 3) |
| T4 | Crash recovery: SIGKILL of the daemon mid-delivery | real, isolated | **PASS** | Same run. The receiver held attempt 1 of `b5cf5a5f…` open; `kill -9` of the daemon; a client restarted a daemon. Delivered on attempt 4, same id, 1.8 s after the receiver answered again. It was delivered at least once: the receiver logged 3 unanswered attempts with that id, so receivers must deduplicate on `X-T3-Watch-Id` |
| T5 | A real host suspend and resume, with awake and wall times | real | **BLOCKED (person)** | Not done: sleeping a shared host disrupts other work. Fixture: `e2e/acceptance/suspend.sh` (step 4). A host that sleeps sends nothing until it wakes: a completion during a sleep is reported late by the sleep, which can be hours on a laptop. Never promise instant notification from sleeping hardware |
| T6 | Live local T3, read only | real | **PASS** | 23:19:59. The completed fixture thread `mcp:launch:7c9315ba…` through the local CLI: run 2 completed, directly (368 ms, watchId `3d0be7c6cf232ee7`) and through the daemon (315 ms, `05608fba8eee23b4`); `--until workstream` on local still exits 2. `evidence/live-local-312fc85.txt` |
| T7 | Native Linux run of the latest archives and tests | real | **BLOCKED (no Linux host reachable from here)** | The Linux node on the tailnet refuses this host's SSH key. Archives, test package and `linux.sh` are ready (step 5). The last Linux execution is the parent's smoke of `910b684` |
| N1 | Claude Code: synthetic hook events | synthetic | **PASS** | `tests/native.rs`, 11 tests |
| N2 | Claude Code: real print session | real, isolated | **PASS** | Claude Code 2.1.312, earlier round (`docs/native-sessions.md`, check 2); print sessions also ran in N4 |
| N3 | Claude Code: real interactive permission dialog | real, isolated | **PASS** | Disposable session `fee59b7e-eea0-4999-a083-b0a8087c4686`, 23:20:58–23:21:44. `needs_input` with its webhook (tool `Bash`); the dialog was rejected with Escape, never approved, and the command did not run. The hook log holds `at`, `event`, `type`, `tool` and `subagent` only, with no prompt or tool input. `evidence/claude-interactive-312fc85.txt`, `evidence/claude-hooklogs-312fc85/` |
| N4 | Claude Code: same-session and `--resume` reuse wake pinned waits | real, isolated | **PASS (mechanism)**; the bot's continuation is **BLOCKED (parent)** | Same session, pinned to run 2 and acknowledging the relayed request: `settled` 1.03 s after the prompt. Print session `fee59b7e-…-4687`, then `--resume`: `settled` run 2, 1.04 s after the resume started |
| X1 | Codex: mock app-server | synthetic | **PASS** | `tests/codex.rs`, 12 tests, including the three for finding 2 and the materialization retry |
| X2 | Codex: real app-server, disposable threads | real, isolated | **PASS** | `e2e/native/codex/run.sh`, 23:18:33–23:20:41, thread `01a1bd74-d8ed-7f92-8fa7-fd0b05ce8710`: <ul><li>a fresh thread keeps waiting;</li><li>needs-input exit 5 and webhook, 671 ms after the approval request;</li><li>in the watcher and control windows alike, only the app-server's own log database changed;</li><li>the owner's request stayed its own;</li><li>completion;</li><li>pinned wake 1.0 s after `turn/completed`;</li><li>unload at 60 s while polled, then a read after unload.</li></ul> `evidence/codex-run-sh-312fc85.txt` |
| X3 | Codex: two approvals in one turn | real, isolated | **PASS** | Same run. A wait that acknowledged request 1 woke for request 2 under its own key (`…-approval-2`) at model pauses of 3 s, 0.5 s and **0 s** (69 to 556 ms). Through the daemon, request 2 also opened **between two waits** and got its own key, at all three pauses |
| X3b | Codex: a request that opens while nothing is connected | protocol limit | **FAIL (explicit)** | The app-server keeps no record of past requests. With no wait running and no daemon keeping a connection (`--no-daemon`, a stopped daemon, a dropped connection, more than 15 minutes), or during a handover between counting connections, such a request shares its predecessor's key, and an acknowledged wait wakes only at the turn's end. Stated as `tests/codex.rs::with_no_connection_open_a_request_in_between_is_not_seen`. Bots should use the daemon |
| X4 | Codex: default socket through the official shared daemon | real, isolated | **PASS** | 23:27:06. `codex app-server daemon start` in an isolated `CODEX_HOME`; t3-watch with no socket override: completed run 1, pinned run 2 510 ms after `turn/completed`; daemon stopped; no LaunchAgent change. `evidence/codex-daemon-socket-312fc85.txt` |
| X5 | Codex: the desktop thread on the MacBook, read only | real | **BLOCKED (MacBook not reachable from here)** | The MacBook on the tailnet refuses this host's SSH key. Desktop compatibility is **unverified**. Fixture: `e2e/native/codex/desktop-check.sh` with the universal binary (step 6) |
| X6 | Codex: the bot reuses a thread and continues | real, production | **BLOCKED (parent)** | Step 7 |

## Not run, and why

- `/concil`: no command, skill or plugin by that name exists. I checked
  `~/.claude/commands`, `~/.claude/skills`, `~/.claude/plugins`,
  `~/.agents/skills` and the project's `.claude`. A `council` skill
  exists. It sends the work to three providers, and its default includes
  GPT Sol, the review provider that must not be retried. So neither was
  run. Independent review stays with the Cursor Grok thread.
- **The runtime on this host**, checked by names only, with no file
  contents read:
  - always on: the T3 server (a LaunchAgent);
  - not present: a t3-watch install, a running t3-watch daemon, a saved
    machine pairing (`~/.config/t3-watch` holds only outbox directories),
    and any webhook receiver or bot routine for t3-watch.
  So the real receiver and its secret live with the parent, and T1, T2,
  T3b and X6 run there.

## Steps for the parent or a person

Each step uses the artifacts above and the scripts in `e2e/`, and needs
the stated actor.

1. **T1, completion (parent).** Use `t3-watch 0.13.0` on the watcher host
   and a disposable T3 test thread. Run
   `t3-watch wait --durable --notify-on all <thread URL>` with the bot's
   webhook (`--webhook-url` and `--webhook-secret-env`). Record the bot's
   receipt and continuation with `e2e/timeline.py`
   (`docs/live-integration.md`, step 5).
2. **T2, needs-input (parent).** The same, with a test thread told to ask
   a question. Expect exit 5 and a `needs_input` event. Nobody answers
   from the watcher. Record the bot relaying the question.
3. **T3b, durable replay to the real receiver (parent).** On the watcher
   host, run:

   ```sh
   T3W_RELAY_TARGET=<webhook URL> T3W_SECRET=<secret> \
     sh e2e/acceptance/real-receiver-replay.sh <t3-watch> <thread URL>
   ```

   The relay is a loopback process that starts down, so the outage covers
   this watcher's path only. Make the test thread complete when the script
   says so. Then record the bot's receipt and continuation.
4. **T5, suspend (person, on a machine that may sleep).** Run
   `sh e2e/acceptance/suspend.sh <t3-watch> <thread URL> [webhook URL]`,
   with the thread running on another machine. Sleep the machine and wake
   it a few minutes later. The script prints the result's `suspendedMs`,
   `elapsedAwakeMs` and `elapsedWallMs`, and the system's sleep and wake
   lines, to compare with the webhook's arrival.
5. **T7, Linux (anyone with a route to a Linux host, for example from the
   MacBook).** Copy `dist/0.13.0/` there and run
   `sh linux.sh <that dir> 312fc8519fd332e0c904d290a0f008605aa1e92d`. It
   checks the hashes and `--version`, runs the 12 test binaries against
   the archive's binary, and runs `e2e/durable.sh`.
6. **X5, the Codex desktop thread (person, on the MacBook as its user,
   with the desktop app open).** Run
   `sh desktop-check.sh <universal binary>`. It sends `initialize`,
   `thread/read` and `thread/turns/list` only. It never starts a daemon,
   resumes or prompts, and it keeps its state in a temporary home. If no
   socket exists it reports BLOCKED. Starting `codex app-server daemon` is
   then the user's decision.
7. **N4 and X6, bot reuse (parent).** On a disposable Claude Code session
   or Codex thread, the bot reads `runOrdinal`, waits with
   `--expect-min-run-ordinal <that + 1>` (plus `--acknowledge` for a
   request it already relayed) through the daemon, and continues.

## Before any public release

- The docs name the maintainer's machines, accounts and local paths (this
  file, `docs/native-sessions.md`, the README's live-check sections). Scrub
  them before anything is published.
- All BLOCKED gates above need evidence, and the work after review
  ordinal 15 needs an independent review.

## Side effects of these runs

- **Claude Code.** It recorded folder-trust answers for
  `/private/tmp/t3wcc*/proj` in `~/.claude.json` and the test prompts in
  `~/.claude/history.jsonl`; neither file was edited by hand. The sessions'
  `~/.claude/projects/-private-tmp-t3wcc*-proj` and
  `~/.claude/session-env/<id>` entries were removed.
- **Codex.** Every app-server, daemon and mock started here was stopped.
  Their fixtures, and their sockets under `/private/tmp/codex-daemon-501/`,
  were removed. `~/.codex` was not used.
- **A stray process.** A mock left running by an earlier probe in this
  round answered one harness run on its port with other settings, and made
  the owner's turn look stuck. It was found and stopped, and `run.sh` now
  refuses to start when the port is taken. That run used a working-tree
  build and is not counted.
- **Superseded runs.** `dist/0.13.0/superseded/` holds the runs with the
  `08d4b24` build. Its Codex run hit the materialization race (exit 2 at
  needs-input), which `312fc85` fixes. They are kept for the record and
  not counted.
