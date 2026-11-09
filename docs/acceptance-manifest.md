# Release acceptance matrix: 0.16.0, engine 18

Build: 0.16.0, engine 18, rev `4632305883c87283dc646218654904a576cf01a6`.
Not yet reviewed. Review ordinal 19 approved 0.15.0 as a final
candidate, with X3b failing.

**The fleet and the release are not accepted.** Code approval is not
release acceptance, and this build has had no review yet. X3b, the Codex
request opened and closed while nothing counts, now passes on this host
(real app-server, isolated) through Codex's `PermissionRequest` hook, with
the limits listed under X3b. The gates the parent ran for 0.15.0 (T1, T2,
T3b, T7, N4, X5, X6) are pending for this build, T5 still needs a person,
and installing and trusting the hook in the user's own Codex (X7) is the
user's step. Nothing is pushed, deployed, installed over a shared binary,
or wired to a production bot.

**Evidence kinds**

- **synthetic**: test fixtures (mock MCP or app-server, a fake `t3` CLI, a
  loopback receiver);
- **real, isolated**: real binaries and real Claude Code or Codex
  processes, in disposable sessions, isolated homes and loopback
  receivers;
- **real, production**: the parent's bot, its routine and its receipts;
- **parent-reported**: evidence the parent holds (on the Grok Bot box or
  in the bot's records), recorded here by path and id only. It is not
  re-verified from this worktree. No webhook URL, token or secret is
  recorded.

An exit code, or an HTTP 200 from a loopback receiver, is never counted as
a bot's continuation. In the Codex runs, approvals in the disposable
threads are declined by the probe's owner client (standing in for the
person who owns the thread). t3-watch answers none.

Times are UTC on 2026-11-09. The build host is the maintainer's Mac mini:
macOS 27.0.1, arm64, rustc 1.99.1, zig 0.17.0, codex-cli 0.166.0, Claude
Code 2.1.319.

## X3b: what changed in 0.16.0

The app-server keeps no record of past requests; Codex's documented
`PermissionRequest` hook runs for every approval request, whatever is
connected. `t3-watch hook codex` is that hook, installed by the user as an
async hook (`--print-settings` prints the block; t3-watch never edits
Codex's settings) and trusted in `/hooks` or Settings > Hooks. It appends
the time, turn id and tool name of each request to
`<state>/native/codex/<session>.requests.jsonl`, never the command or its
input, prints nothing, always exits 0 and returns no decision. The
counting connection matches each record to a request it counted live; one
no count matches opened while nothing counted and is reported in
`recoveredRequests` and once as `requests_recovered` progress. It never
becomes a needs-input wake: its owner answered it. `hooks/list`
(read-only) sets `requestRecovery`. Watching stays read-only: no
`thread/resume`, no answer, no decision. Details and limits:
`docs/native-sessions.md`, "Requests missed while nothing counts".

## Commits since the 0.15.0 matrix (`715700d`, branch `feat/t3-watch-v1`, unpushed)

| Commit | What |
|---|---|
| `f2a14db` | Codex: recover approval requests missed while nothing counted, from the `PermissionRequest` hook log |
| `0cd9b9f` | Engine 18 (0.16.0), capability `native-codex-hook-log` |
| `1720230` | Docs: the hook log recovery, its remaining limits, engine 18 |
| `4632305` | The desktop check reports whether the Codex hook is installed |
| this commit | The 0.16.0 matrix |

## Artifacts

They live in the worktree's ignored `dist/0.16.0/`, with the logs in
`dist/0.16.0/evidence/`. All are built from an exact `git archive` of
`4632305`, and `--version` prints `t3-watch 0.16.0 (engine 18, rev
4632305883c87283dc646218654904a576cf01a6)`.

They are built with
`RUSTFLAGS="--remap-path-prefix=$HOME=/build-home --remap-path-prefix=<source dir>=/build/src"`,
so no binary embeds the builder's home. None of the binaries below contains
a `/Users/` or `/private/tmp` path. The test binaries hold only the
synthetic test strings `/Users/a` and `/Users/someone`, and no
`/private/tmp` path. A rebuild is not claimed to match byte for byte.

| File | SHA-256 | Executed |
|---|---|---|
| `t3-watch-4632305-aarch64-apple-darwin` (2,003,216 bytes) | `1da1a88f4319f290e5cb9f63de22cf120c45a6457778c11d474e966ede7aaaad` | Every real run below |
| `t3-watch-4632305-universal-apple-darwin` (arm64 + x86_64) | `225d02c9b2256ea42ae9a16a1b2c660a16a326867fa4bc680c4f7317fb041beb` | Not run; its arm64 slice is the build above, and x86_64 is not run here (no Rosetta) |
| `t3-watch-0.16.0-x86_64-unknown-linux-gnu.tar.gz` | `bcd184ec3270a5840a77fcfe72b48133c74887b870fc0f5e9dd9c00a56478c0a` (binary `d7a42f24…1438`, glibc 2.17 or older) | **Not yet run on Linux** (T7, the parent's box) |
| `t3-watch-0.16.0-aarch64-unknown-linux-gnu.tar.gz` | `fc2f6f2c18e8b978099a461880edbff52f563c45a0960a80c09f2bdbbf1a17dc` (binary `e9e2a2c6…fdd2`, glibc 2.17 or older) | Not run: no aarch64 Linux host |
| `t3-watch-tests-0.16.0-x86_64-unknown-linux-gnu.tar.gz` | `6d245bfcaef2a3a48be334b041033679d309ad8e2ff1007ad489c0aa6fdd729a` (12 test binaries, LTO off, plus the source) | **Not yet run on Linux** (T7) |
| `linux.sh`, `desktop-check.sh` | copies of `e2e/acceptance/linux.sh` (`1dbe6485…5293`, unchanged since 0.15.0) and `e2e/native/codex/desktop-check.sh` (`c95b18ad…25ec`) | |

## Build checks at `4632305`

| Check | Toolchain | Result | When | Log |
|---|---|---|---|---|
| `cargo test --locked` | rustc 1.99.1 | **PASS**: 291 passed, 0 failed | 05:43:28–05:45:19 | `evidence/suite-4632305.txt` |
| (the first run of the same) | rustc 1.99.1 | 1 failed: `adversarial::a_cancelled_local_poll_kills_its_t3_process`, "timed out waiting for the budget to drain" (3 s) | 05:39:39–05:39:52 | `evidence/suite-4632305-run1-failed.txt` |
| `cargo +1.85.1 test --locked` (MSRV) | rustc 1.85.1 | **PASS**: 291 passed, 0 failed | ends 05:42:01 | `evidence/msrv-4632305.txt` |
| `cargo fmt --check`; `cargo clippy --locked --all-targets -- -D warnings` | rustc 1.99.1 | **PASS**: clean | 05:39:52 | `evidence/lint-4632305.txt` |

The failed test is a timing flake outside this change: it waits 3 s for a
killed local `t3` poll to give its slot back, and its code is unchanged
since 0.15.0. It did not fail again in 30 reruns (20 alone, 10 with its
whole test binary), in the MSRV run, or in the suite rerun. It is
recorded, not fixed.

## Gates

The rows run on this host used the shipped arm64 binary (`1da1a88f`). The
parent-reported results are for 0.15.0 (rev `1e98e73`); this build changes
the engine, so they are pending for it.

| # | Gate | Kind | Status | Evidence |
|---|---|---|---|---|
| T1 | The correct bot woken by a real completion webhook, and continuing | real, production | **PENDING (parent) for 0.16.0**; PASS for 0.15.0 (parent-reported) | 0.15.0: watchId `1650566df28c0743`, notificationId `5c5f52008d347700b6f8ebeec62f4206`, `/workspace/t3-watch-gates-1e98e73-03cb7361cb9947b7/FINAL-evidence.json` and `T1-RECEIVED.json` |
| T2 | A real `needs_input` notification, and the parent's continuation | real, production | **PENDING (parent) for 0.16.0**; PASS for 0.15.0 (parent-reported) | 0.15.0: watchId `9aa64e5f7e064c90`, `T2.RESULT`, same evidence directory |
| T3 | Durable delivery through a controlled outage and a daemon restart | real, isolated | **PASS** | `e2e/durable.sh`. With the receiver down, the client exited 4 with the event queued. After a daemon stop and restart, the receiver came up and the event was delivered on attempt 5 with the same id (`af020927…`), 2.0 s later. `evidence/durable-4632305.txt` |
| T3b | Durable replay to the **real** receiver, and the bot continuing | real, production | **PENDING (parent) for 0.16.0**; PASS for 0.15.0 (parent-reported) | 0.15.0: replay id `0903158322f9d1610fd19123acd696dd`, attempt 5; `T3b-clean-real-receiver-receipt-09031583.json` |
| T4 | Crash recovery: SIGKILL of the daemon mid-delivery | real, isolated | **PASS** | Same run: delivered on attempt 4 with the same id (`b13c2e8b…`), 1.8 s after the receiver answered again. Delivery is at least once |
| T5 | A real host suspend and resume | real | **PENDING (person, not waived)** | Not done, and never simulated in its place: this host is shared and kept awake. `e2e/acceptance/suspend.sh` (step 1). A sleeping host sends nothing until it wakes, so never promise instant notification from sleeping hardware |
| T6 | Live local T3, read only | real | **PASS** | 05:49:52: a completed fixture thread, directly (461 ms, watchId `60c58c88d4755960`) and through the daemon (316 ms, `e3d044fbab181957`); `--until workstream` on local exits 2. `evidence/live-local-4632305.txt` |
| T7 | Native Linux run of the latest archives and tests | real | **PENDING (parent) for 0.16.0**; PASS for 0.15.0 (parent-reported) | `dist/0.16.0/linux.sh` with the archives above, on the Grok Bot box. 0.15.0: `linux-run.log` in `/workspace/t3-watch-candidate-1e98e73`, 286 tests and `e2e/durable.sh` A and B |
| N1 | Claude Code: synthetic hook events | synthetic | **PASS** | `tests/native.rs`, 11 tests |
| N3 | Claude Code: real interactive permission dialog | real, isolated | **PASS** | Disposable session `84a2345e-b949-4b98-8660-6c16e6c894f4`, 05:50:16. `needs_input` and its webhook arrived (tool `Bash`). The dialog was rejected with Escape and never approved, and the command did not run. Same-session reuse woke a pinned wait 1.5 s after the next prompt; a `--resume` reuse 1.0 s after the resume started. The hook log holds `at`, `event`, `type`, `tool` and `subagent` only. `evidence/claude-interactive-4632305.txt` |
| N4 | Claude Code: same-session reuse wakes the bot, which continues | real, production | **PENDING (parent) for 0.16.0**; PASS for 0.15.0 (parent-confirmed) | 0.15.0: correlation `cbbacb6e4f0d4d1d`, watchId `24f5cc6a72c561cd`, `RECEIVED-24f5cc6a72c561cd.json` |
| X1 | Codex: mock app-server | synthetic | **PASS** | `tests/codex.rs`, 22 tests |
| X2 | Codex: real app-server, disposable threads | real, isolated | **PASS** | `e2e/native/codex/run.sh`, 05:45:26–05:48:25, thread `01a1bed7-1045-7040-b3ba-75825bcd92ec`: <ul><li>needs-input and its webhook arrived 1,015 ms after the approval request;</li><li>in the watcher and control windows alike, only the app-server's own log database changed;</li><li>the owner's request stayed its own;</li><li>pinned wake 1.0 s after `turn/completed`;</li><li>unload at 60 s while polled, then a read after unload.</li></ul> `evidence/codex-run-sh-4632305.txt` |
| X3 | Codex: two approvals in one turn | real, isolated | **PASS** | Same run, at pauses of 3 s, 0.5 s and 0 s: <ul><li>one connected wait woke for request 2 under its own key (59 to 593 ms after it opened);</li><li>through the daemon, request 2 opened between waits and got its own key;</li><li>on the keeper drain path, a wait inside a round kept the request, and the next request reached the third wait;</li><li>a second `--no-daemon` wait without the hook counted the open request again, as documented.</li></ul> |
| X3b | Codex: a request opened and closed while nothing counts | real, isolated | **PASS** with the hook installed and trusted; limits below | Same run, `probe_missed.py`, with the printed hook block in the fixture's `CODEX_HOME` and the binary on a path with a space (quoted command): <ul><li>untrusted, the hook left no record for two requests;</li><li>with a wait counting throughout, nothing was recovered (`requestRecovery: "hook"`);</li><li>with nothing counting while two requests opened and closed, the next wait listed both in `recoveredRequests` and reported them once as `requests_recovered`; a later wait listed them without reporting them again;</li><li>with the daemon stopped after it counted request 1, a wait through a new daemon recovered request 2 only;</li><li>the log held `atMs`, `event`, `tool` and `turn` only.</li></ul> Mock: `tests/codex.rs::a_request_that_opens_and_closes_while_nothing_counts_is_recovered_from_the_hook_log` (webhook progress, with and without the daemon) and five more hook tests |
| X4 | Codex: default socket through the official shared daemon | real, isolated | **PASS** | 05:50:02. Completion, then a pinned run 2 500 ms after `turn/completed`. The daemon was stopped and no LaunchAgent changed. `evidence/codex-daemon-socket-4632305.txt` |
| X5 | Codex: the desktop thread on the MacBook, read only | real | **PENDING (parent) for 0.16.0**; PASS for 0.15.0 (parent-reported) | `dist/0.16.0/desktop-check.sh` against the research thread. 0.15.0 exited 0 at run ordinal 4 (`x5-desktop-check.out`; the thread's id is withheld from this file) |
| X6 | Codex: the bot reuses a thread and continues | real, production | **PENDING (parent) for 0.16.0**; PASS for 0.15.0 (parent-confirmed) | 0.15.0: correlation `cbbacb6e4f0d4d1d`, disposable thread `01a1be81-857d-7483-a375-daadd3d77217`, watchId `5a10216c5de0876f`, `RECEIVED-5a10216c5de0876f.json` |
| X7 | Codex: the hook in the user's own Codex (desktop app or CLI) | real | **PENDING (person)** | The user adds the `t3-watch hook codex --print-settings` block to `~/.codex/config.toml` and trusts it; then `T3_WATCH_HOME=<that state dir> sh desktop-check.sh <t3-watch> <thread id>` shows `requestRecovery: "hook"`. t3-watch never edits Codex's settings, and trusting a hook is the user's decision, so this was not done for them |

**X3b limits** (also in `docs/native-sessions.md`, "Limits"):

- recovery needs the hook installed and trusted by the user (X7); without
  it (`requestRecovery: "none"`) such a request is still not reported;
- user-input requests (`waitingOnUserInput`) have no Codex hook event and
  are not recovered;
- approvals Codex does not run `PermissionRequest` for are not recovered;
  only shell-command escalations were run here, while Codex's
  documentation also names `apply_patch`, MCP tools and managed-network
  approvals;
- a hook that is trusted but records nothing (a moved binary) loses
  requests that open and close unseen; a replaced open request then still
  gets the extra wake, never a missed one;
- subagent threads (records under the parent's `session_id`) were not run;
- a request another hook allowed or denied is reported as recovered,
  though it never reached a person.

## Still open

1. **Review of this build.** No review has seen `f2a14db`..`4632305`.
2. **The parent's gates for 0.16.0:** T1, T2, T3b, N4 and X6 with this
   build; T7 with `sh dist/0.16.0/linux.sh dist/0.16.0 4632305` on the Grok
   Bot box; X5 with `dist/0.16.0/desktop-check.sh` on the MacBook.
3. **T5, a real host suspend (a person, on a machine that may sleep).**
   Run `sh e2e/acceptance/suspend.sh <t3-watch> <thread URL> [webhook URL]`
   with the thread running elsewhere. Sleep the machine and wake it a few
   minutes later. Record the printed `suspendedMs`, `elapsedAwakeMs` and
   `elapsedWallMs`, and the system's sleep and wake lines, against the
   webhook's arrival. Not waived, and never simulated in its place.
4. **X7, the hook in the user's own Codex (the user).** Install and trust
   it, then the desktop check above with `T3_WATCH_HOME` set.

## History: the native Linux run of 0.14.0 (gate T7: FAIL, fixed in 0.15.0)

Run on the Grok Bot Linux box: Debian, glibc 2.41, kernel 6.12.113,
x86_64, 2026-11-09T00:44:45Z. The archive and the tests archive checked
out (SHA-256 OK), and `--version` reported rev `bf58d758`. Its logs were
read from the box's evidence bundle.

- **Passed:** adversarial 18, consistency 11, hardening 33, integration
  24, native 11, outbox 7, workstream 19, and `e2e/durable.sh` (outage
  replay; SIGKILL with a hanging receiver).
- **Failed:**

| Failure on Linux | Root cause | Kind | Fix |
|---|---|---|---|
| compat: `a_hung_engine_5_daemon_with_a_full_backlog_is_found_by_its_lock` hung | On Linux a **blocking** connect to a Unix socket whose backlog is full waits until a slot frees; macOS refuses at once. The test filled the backlog with blocking connects. | test harness | `663c9ae`: backlogs are filled with non-blocking connects (`HungListener`) |
| (same root cause) | The product made the same blocking call. `daemon::connect`, used by every `wait`, `status` and `stop`, would wait forever on a hung daemon with a full backlog, and so would a starting daemon in `take_lock` | **product** | `663c9ae`: `util::connect_unix` retries non-blocking connects every 20 ms and gives up after 5 s with an error naming the daemon (exit 2); `take_lock` uses the non-blocking probe. New test: `a_client_meeting_a_hung_daemon_returns_instead_of_hanging` |
| codex: `a_dropped_connection_is_reopened_and_a_turn_finished_meanwhile_is_seen` (Broken pipe, never recovered) | The mock pings before each answer and closes after the third. The client's pong met the closed peer (EPIPE at once on Linux), and that write error ended the call although the answer was already buffered. macOS accepted the write while the peer was only shut down | **product** | `95f847a`: a pong that cannot be written is ignored and buffered frames are still read. The mock now closes every descriptor at once, which reproduces the failure on macOS too (old code: 2 of 3 runs time out; new code: 3 of 3 pass) |
| delivery ×2: `python3: can't open file '/private/tmp/t3wsrc/e2e/timeline.py'` | `CARGO_MANIFEST_DIR` is baked in at build time, as the macOS path | test harness | `b6a487f`: the script is built into the test (`include_str!`) and written next to the trace |
| suspend: `an_answer_buffered_across_a_suspend_never_completes_the_wait` (frozen, 1 s) | Linux ends a `SO_RCVTIMEO` read stopped by SIGSTOP with EINTR on SIGCONT, so the client drops the registration as slept through (`daemon.stale`) before reading the buffered answer. macOS restarts the read and finds the answer too late (`daemon.late`). Both refuse it, and the exit, outcome and webhook assertions held on Linux | test over-specified | `b6a487f`: the frozen case accepts either. New test `a_final_answer_read_after_the_deadline_is_not_used` reaches `daemon.late` without signals on every OS |
| unit: `watchdog_spawn_failure_kills_and_reaps_the_child` (Text file busy) | ETXTBSY: Linux refuses to run a file any process has open for writing, and a child forked by another test thread holds a just-written script open until its own `exec`. T3 updating its binary in place can do the same to a real watch | test race, reachable in production | `a135c50`: running `t3 thread show` is retried on ETXTBSY for up to 200 ms. New test: `a_t3_binary_still_open_for_writing_runs_once_it_is_closed` |

`1e98e73` makes this engine 17 (0.15.0): the daemon-side fixes change what
an engine-16 daemon would do for a new client.

**0.15.0 on the same box: PASS** (parent-reported). `linux-run.log` in
`/workspace/t3-watch-candidate-1e98e73` covers the 286 tests and
`e2e/durable.sh` scenarios A and B.

The Linux-specific branches of the new tests (the retried and refused
connect, ETXTBSY) can only run on Linux. On macOS they take the path macOS
takes.

## Publish-ready history

`feat/t3-watch-v1` is left as it is. A separate local branch,
`publish/t3-watch-scrubbed`, holds the same history with private details
removed from every commit message and every commit's files: the MacBook's
hostname, an account name, the research project's path, the build host's
name, a real thread id and environment id, a home path, and a person's
name in one doc. That includes the messages of `4dca907` and `17f71fb`.
The `LICENSE-MIT` copyright line and the commit authors are kept. Its
tip's tree is identical to `feat/t3-watch-v1`'s. It is not pushed.

## Scope notes: what ran on this host and what ran elsewhere

- `/concil`: no command, skill or plugin by that name exists. The
  `council` skill's default panel includes the review provider that must
  not be retried, so neither was run.
- **The runtime on this host** (names only, no contents read): the T3
  server runs always on. There is no t3-watch install, saved pairing,
  webhook receiver or bot routine. So T1, T2, T3b and X6 were not run on
  this host. The parent ran them for 0.15.0 against its own receiver and
  bot routine, where the webhook URL and secret are; T7 and X5 ran on the
  Grok Bot box and the MacBook. For 0.16.0 they wait for the parent.
- **The user's Codex** (`~/.codex`) was not read, used or changed: every
  hook check ran in a fixture `CODEX_HOME`, where the hook was trusted the
  way `/hooks` records it. Nothing was installed in the user's Codex.

## Side effects of these runs

- **Claude Code.** It recorded folder-trust answers for the disposable
  `/private/tmp/t3wcc*/proj` folders in `~/.claude.json` and the test
  prompts in `~/.claude/history.jsonl`; neither file was edited by hand.
  The sessions' `~/.claude/projects` and `~/.claude/session-env` entries
  were removed.
- **Codex.** Every app-server, daemon and mock started here was stopped,
  and their fixtures and sockets were removed. `~/.codex` was not used,
  and no real thread was read or touched. The mocks ran on port 18821,
  because the parent's X6 fixture still holds 18801; it was left alone.
