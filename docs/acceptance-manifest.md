# Release acceptance matrix: 0.14.0, engine 16

This build is **ready for independent review and not accepted**. Review
ordinal 16 (`5cdc4b9`, 0.13.0) asked for changes, and the commits below
make them. They have not been reviewed yet. Code approval is not release
acceptance either: every gate marked BLOCKED needs the parent's bot, a
person, or a machine this host cannot reach. Nothing is pushed, deployed,
installed over a shared binary, or wired to a production bot.

**Evidence kinds**

- **synthetic**: test fixtures (mock MCP or app-server, a fake `t3` CLI, a
  loopback receiver);
- **real, isolated**: real binaries and real Claude Code or Codex
  processes, in disposable sessions, isolated homes and loopback
  receivers;
- **real, production**: the parent's bot, its routine and its receipts.

An exit code, or an HTTP 200 from a loopback receiver, is never counted as
a bot's continuation. In the Codex runs, approvals in the disposable
threads are declined by the probe's owner client (standing in for the
person who owns the thread). t3-watch answers none, and the mock
app-server tests assert that it sends no answer.

Times are UTC on 2026-11-09 unless a date is given. The build host is the
maintainer's Mac mini: macOS 27.0.1, arm64, rustc 1.99.1, codex-cli
0.166.0, Claude Code 2.1.319.

## Commits after review ordinal 16 (branch `feat/t3-watch-v1`, unpushed)

| Commit | What |
|---|---|
| `5cdc4b9` | 0.13.0 engine 15, reviewed in ordinal 16 (changes required) |
| `0fa3b58` | P3: personal identifiers removed from the tracked tree |
| `bf58d75` | P1 and the three P2s: the Codex keeper keeps its counting connection, fresh counts recount, a batch's newest status decides a turn's end, a bounded `list_turns` retry. Engine 16 (0.14.0) |
| `04a7f1e` | The real-server probe's connected wait starts a fresh count |
| this commit | This matrix |

The code is identical from `bf58d75` on; only docs and `e2e/` change.

## Artifacts

They live in the worktree's ignored `dist/0.14.0/`, with the logs in
`dist/0.14.0/evidence/`. All of them are built from `bf58d75`, and
`--version` prints `t3-watch 0.14.0 (engine 16, rev
bf58d758bb53ae0b838f79c1f78fdabe6248ddae)`. A rebuild is not claimed to
match byte for byte.

| File | SHA-256 | Built with | Executed |
|---|---|---|---|
| `t3-watch-bf58d75-aarch64-apple-darwin` (1,970,016 bytes) | `9abbd7d12076258a41b342c5b4f9909b05b1f8505300789ef0ef5bd21e8c95f9` | `cargo build --release --locked` | Every real run below |
| `t3-watch-bf58d75-universal-apple-darwin` (arm64 + x86_64) | `becf53ab3aca2ab95bc8742ecfa031cf89711aefd836de8138ff40655b2da1b5` | `lipo` of the above and an x86_64 build | arm64 only (`--version`); x86_64 not run here (no Rosetta) |
| `t3-watch-0.14.0-x86_64-unknown-linux-gnu.tar.gz` | `c92ade2a7532d395366275d440c2d671435406c5e42bf412c4aa15bf5fdd3a55` (binary `fac78ac3…6903`) | zig 0.17.0 cross-build, glibc 2.17 or older | **No** |
| `t3-watch-0.14.0-aarch64-unknown-linux-gnu.tar.gz` | `2953caa5889a3d13db294afa7322a25ea1d32f0719583d15c8f672cf9ecf15bd` (binary `5f5bd61f…58b3`) | same | **No** |
| `t3-watch-tests-0.14.0-x86_64-unknown-linux-gnu.tar.gz` | `58359f192921650a457736af56e7717e107a0af7771c7b0a1cab2ecdaaa6575e` | the 12 test binaries (LTO off) and the source; `linux.sh` runs them against the archive's binary | **No** |
| `linux.sh`, `desktop-check.sh` | copies of `e2e/acceptance/linux.sh` and `e2e/native/codex/desktop-check.sh` | | |

## Build checks

| Check | Commit | Toolchain | Result | When | Log |
|---|---|---|---|---|---|
| `cargo test --locked` | `bf58d75` | rustc 1.99.1 | **PASS**: 283 passed, 0 failed | 00:10:47–00:12:34 | `evidence/suite-bf58d75.txt` |
| `cargo +1.85.1 test --locked` (MSRV) | `04a7f1e` (code = `bf58d75`) | rustc 1.85.1 | **PASS**: 283 passed, 0 failed | 00:21:03–00:23:04 | `evidence/msrv-bf58d75.txt` |
| `cargo fmt --check`; `cargo clippy --locked --all-targets -- -D warnings` | `04a7f1e` (code = `bf58d75`) | rustc 1.99.1 | **PASS**: clean | 00:20:54 | `evidence/lint-bf58d75.txt` |

## Review ordinal 16 findings

| Finding | Outcome |
|---|---|
| P1. A drain round took kept connections out of `PARKED`; a wait in that window opened a second connection, and `park` then replaced the counting one | Fixed (`bf58d75`). Rounds read kept connections in place under the lock, and `park` and `unpark` wait for the round; waiters are let through before the next round. `park` keeps the connection holding the counter. A kept connection without the counter takes it when it has events. Regression test `a_wait_that_starts_during_a_drain_round_keeps_the_counting_connection`: a test hook stretches rounds to 3 s back to back, so the wait starts inside one. On the old keeper code it fails 3 of 3 runs with a second connection opened; on `bf58d75` it passes |
| P2. On a fresh counter a still-on flag is not recounted, so a replaced request is invisible | Fixed: a connection that takes the counter over counts a flag still on once more (an extra wake, never a missed one). Without the daemon each wait takes it over, so a still-open request gets a new key per `--no-daemon` process: documented, with bots told to use the daemon. A request opened and closed while nothing counts is an explicit limit (X3b). `thread/resume` is still never sent |
| P2. Any "not supported yet" retried until `--timeout` | Fixed: only `list_turns is not supported yet` is retried, for 30 s, then the wait fails with exit 2 (test: 29 s to 120 s bound). Any other refusal fails at once |
| P2. Any non-active status in a drain batch closed the keeper | Fixed: the batch's newest status decides. A batch that ends one turn and starts the next keeps the connection, and the new turn is counted from 1 (test: `a_drain_batch_that_ends_one_turn_and_starts_the_next_keeps_the_connection`, which fails on the old code) |
| P3. Personal identifiers in `docs/native-sessions.md` | Fixed (`0fa3b58`) across the tracked tree: an account, two hostnames, a project path, a person's name, real thread and environment ids, and a bench file named after the host. The remaining hit is the copyright line in `LICENSE-MIT`. **Local commit messages** from earlier rounds still mention the research project and the MacBook's hostname: reword or squash them before publishing |

## Gates

| # | Gate | Kind | Status | Evidence (the `bf58d75` arm64 artifact unless noted) |
|---|---|---|---|---|
| T1 | The correct bot woken by a real completion webhook, and continuing | real, production | **BLOCKED (parent)** | Passed with engine 12 (parent-held, review ordinal 14); needs a rerun with this build (step 1) |
| T2 | A real `needs_input` notification, and the parent's continuation | real, production | **BLOCKED (parent)** | Mechanism shown by N3 and X2 (step 2) |
| T3 | Durable delivery through a controlled outage and a daemon restart | real, isolated | **PASS** | `e2e/durable.sh`, 00:12:52–00:14:01. With the receiver down, the client exited 4 with the event queued (4 attempts). After the daemon stop and restart, the receiver came up and the event was delivered on attempt 5 with the same id (`674e8017…`), 2.0 s later. `evidence/durable-bf58d75.txt` |
| T3b | Durable replay to the **real** receiver, and the bot continuing | real, production | **BLOCKED (parent)** | Fixture: `e2e/acceptance/real-receiver-replay.sh` with `e2e/relay.py` (step 3) |
| T4 | Crash recovery: SIGKILL of the daemon mid-delivery | real, isolated | **PASS** | Same run. Attempt 1 of `2dbfbea4…` was held open, then `kill -9` of the daemon; a client restarted it. Delivered on attempt 4 with the same id, 1.8 s after the receiver answered again. Delivery is at least once: receivers deduplicate on `X-T3-Watch-Id` |
| T5 | A real host suspend and resume, with awake and wall times | real | **BLOCKED (person)** | This host is shared and kept awake on purpose, and a VM suspend needs a macOS VM image (30 to 80 GB) against 3.5 GB free. Fixture: `e2e/acceptance/suspend.sh` (step 4). A sleeping host sends nothing until it wakes: a completion during a sleep is reported late by the sleep, which can be hours. Never promise instant notification from sleeping hardware |
| T6 | Live local T3, read only | real | **PASS** | 00:14:09. A completed fixture thread through the local CLI: run 2 completed, directly (508 ms, watchId `c3d30eb98c1b154f`) and through the daemon (321 ms, `1a3d6df8bf36c61b`); `--until workstream` on local exits 2. `evidence/live-local-bf58d75.txt` |
| T7 | Native Linux run of the latest archives and tests | real | **BLOCKED (no Linux host reachable)** | The tailnet's Linux node refuses this host's SSH key, and the other is offline. A local Linux VM needs a multi-GB image against 3.5 GB free (the host's rule is to keep 15 GB). Archives, test package and `linux.sh` are ready (step 5) |
| N1 | Claude Code: synthetic hook events | synthetic | **PASS** | `tests/native.rs`, 11 tests |
| N3 | Claude Code: real interactive permission dialog | real, isolated | **PASS** | Disposable session `380d6aec-3587-4e45-8d52-189b72cd09f3`, 00:19:51–00:20:43. `needs_input` and its webhook arrived (tool `Bash`). The dialog was rejected with Escape and never approved, and the command did not run. The hook log holds `at`, `event`, `type`, `tool` and `subagent` only, with no prompt or tool input. `evidence/claude-interactive-bf58d75.txt` |
| N4 | Claude Code: same-session and `--resume` reuse wake pinned waits | real, isolated | **PASS (mechanism)**; the bot's continuation is **BLOCKED (parent)** | Same session, pinned and acknowledging the relayed request: `settled` run 2, 1.04 s after the prompt. Print session `380d6aec-…-09f2` resumed: `settled` run 2, 4.1 s after the resume started |
| X1 | Codex: mock app-server | synthetic | **PASS** | `tests/codex.rs`, 17 tests, including the five for review ordinal 16 |
| X2 | Codex: real app-server, disposable threads | real, isolated | **PASS** | `e2e/native/codex/run.sh` at `04a7f1e`, 00:16:17–00:19:10, thread `01a1bda9-b753-7d90-bb81-17779e2432d2`: <ul><li>a fresh thread keeps waiting;</li><li>needs-input exit 5 and webhook, 913 ms after the approval request;</li><li>in the watcher and control windows alike, only the app-server's own log database changed;</li><li>the owner's request stayed its own;</li><li>completion;</li><li>pinned wake 1.0 s after `turn/completed`;</li><li>unload at 60 s while polled, then a read after unload.</li></ul> `evidence/codex-run-sh-bf58d75.txt` |
| X3 | Codex: two approvals in one turn | real, isolated | **PASS** | Same run, at model pauses of 3 s, 0.5 s and 0 s: <ul><li>One wait, connected from request 1 on, woke for request 2 under its own key (`…-approval-2`, 47 to 579 ms after it opened).</li><li>Through the daemon, request 2 opened between two waits and got its own key.</li><li>**Keeper drain path:** with drain rounds stretched to 3 s by the test hook, the second wait started inside a round. It kept the same request (exit 3, one kept connection), and request 2, opened while no wait ran, reached the third wait as `approval-2`.</li><li>A second `--no-daemon` wait counted the still-open request 1 again (`approval-2`, as documented).</li></ul> |
| X3b | Codex: a request opened and closed while nothing counts | protocol limit | **FAIL (explicit)** | The app-server keeps no record of past requests: one answered while no counting connection is open (`--no-daemon` between waits, a stopped daemon, a dropped connection, more than 15 minutes) leaves no trace. Its owner answered it, so nobody is left waiting, but it is not reported. Tests: `a_request_that_opens_and_closes_while_nothing_counts_is_never_seen`; and the replaced-request case, now counted again (`a_request_replaced_while_nothing_counts_is_counted_again`) |
| X4 | Codex: default socket through the official shared daemon | real, isolated | **PASS** | 00:19:35. `codex app-server daemon start` in an isolated `CODEX_HOME`, with no socket override: completed run 1, then a pinned run 2 504 ms after `turn/completed`. The daemon was stopped, and no LaunchAgent changed. `evidence/codex-daemon-socket-bf58d75.txt` |
| X5 | Codex: the desktop thread on the MacBook, read only | real | **BLOCKED (MacBook not reachable)** | The MacBook on the tailnet refuses this host's SSH key. Desktop compatibility is **unverified**. Fixture: `desktop-check.sh` with the universal binary (step 6) |
| X6 | Codex: the bot reuses a thread and continues | real, production | **BLOCKED (parent)** | Step 7 |

## Not run, and why

- `/concil`: no command, skill or plugin by that name exists. A `council`
  skill exists, but its default panel includes the review provider that
  must not be retried, so neither was run.
- **The runtime on this host**, checked by names only, with no file
  contents read:
  - always on: the T3 server (a LaunchAgent);
  - not present: a t3-watch install, a running t3-watch daemon, a saved
    machine pairing, and any webhook receiver or bot routine for t3-watch.
  So T1, T2, T3b and X6 have to run where the parent holds the webhook URL
  and secret.

## Steps for the parent or a person

1. **T1, completion (parent).** Run `t3-watch 0.14.0` on the watcher host
   with a disposable T3 test thread:
   `t3-watch wait --durable --notify-on all <thread URL>`, with the bot's
   webhook (`--webhook-url`, `--webhook-secret-env`). Record the bot's
   receipt and continuation with `e2e/timeline.py`
   (`docs/live-integration.md`, step 5).
2. **T2, needs-input (parent).** The same, with a test thread told to ask
   a question. Expect exit 5 and a `needs_input` event; nobody answers
   from the watcher. Record the bot relaying the question.
3. **T3b, durable replay to the real receiver (parent).** Run:

   ```sh
   T3W_RELAY_TARGET=<webhook URL> T3W_SECRET=<secret> \
     sh e2e/acceptance/real-receiver-replay.sh <t3-watch> <thread URL>
   ```

   The relay is a loopback process that starts down: an outage of this
   watcher's path only. Complete the thread when the script says, then
   record the bot's receipt and continuation.
4. **T5, suspend (person, on a machine that may sleep).** Run
   `sh e2e/acceptance/suspend.sh <t3-watch> <thread URL> [webhook URL]`,
   with the thread running elsewhere. Sleep the machine, then wake it a
   few minutes later. Compare the printed `suspendedMs`, `elapsedAwakeMs`
   and `elapsedWallMs`, and the system's sleep and wake lines, with the
   webhook's arrival.
5. **T7, Linux (anyone with a route to a Linux host).** Copy
   `dist/0.14.0/` there and run
   `sh linux.sh <that dir> bf58d758bb53ae0b838f79c1f78fdabe6248ddae`. It
   checks the hashes and `--version`, runs the 12 test binaries against
   the archive's binary, and runs `e2e/durable.sh`.
6. **X5, the Codex desktop thread (person, on the MacBook as its user,
   with the desktop app open on the thread).** Run
   `sh desktop-check.sh <universal binary> <thread id>`. It sends
   `initialize`, `thread/read` and `thread/turns/list` only. It never
   starts a daemon, resumes or prompts, and it keeps its state in a
   temporary home. With no socket it reports BLOCKED, and starting
   `codex app-server daemon` is then the user's decision.
7. **N4 and X6, bot reuse (parent).** On a disposable Claude Code session
   or Codex thread, the bot reads `runOrdinal`, waits through the daemon
   with `--expect-min-run-ordinal <that + 1>` (plus `--acknowledge` for a
   request it already relayed), and continues.

## Before any public release

- Reword or squash the local commit messages that name the research
  project or the MacBook's hostname.
- Every BLOCKED gate above needs evidence, and the commits after review
  ordinal 16 need an independent review.

## Side effects of these runs

- **Claude Code.** It recorded folder-trust answers for the disposable
  `/private/tmp/t3wcc*/proj` folders in `~/.claude.json` and the test
  prompts in `~/.claude/history.jsonl`; neither file was edited by hand.
  The sessions' `~/.claude/projects` and `~/.claude/session-env` entries
  were removed.
- **Codex.** Every app-server, daemon and mock started here was stopped.
  Their fixtures and sockets were removed. `~/.codex` was not used, and
  no real thread was read or touched.
