# Native sessions: Codex and Claude Code (experimental)

t3-watch was built to watch T3 threads. This document covers two more kinds
of target: existing native **Codex** threads (desktop app or CLI) and
existing native **Claude Code** sessions. The goal is the same as for T3:
a bot that manages or reuses one of these sessions gets a deterministic
wake (webhook, durable outbox, correct `X-T3-Watch-*` routing headers) when
a turn completes or the session needs input, and no AI model has to poll
anything.

## Separate from the T3 release gates

The T3 release gates (README, "Acceptance"; `docs/live-integration.md`) are
unchanged and apply to T3 targets only. The native adapters are
experimental. Nothing in this document is part of those gates, and neither
adapter is "supported" until its acceptance checks below have passed.

| Adapter | Status |
|---|---|
| Claude Code (`claude-code:<session_id>`) | First increment implemented (hook ingestion and waiting). Acceptance checks 1 to 3 passed; 4 passed with a local receiver, the bot's own continuation still open. Not yet accepted. |
| Codex (`codex://threads/<id>`) | Implemented (read-only polling of a Codex app-server). Acceptance checks 1 and 2 passed against a real codex-cli 0.166.0 app-server; 3 (the desktop app) and 4 open. Not yet accepted. |

## Rules for both adapters

- **Watch only.** Never send a prompt or start a turn, never steer,
  interrupt, resume with overrides, fork, archive, rename or delete
  anything, and never change a session's settings.
- **Approval ownership stays where it is.** t3-watch never answers an
  approval, permission, elicitation or user-input request. It reports that
  one is pending, so the owning human or client answers it.
- **Official interfaces only.** Claude Code: documented hooks (and, if ever
  needed, the documented Agent SDK and session flags). Codex: the
  documented app-server JSON-RPC protocol and the `codex` CLI. No
  undocumented APIs, no copied credentials, no reading an app's internal
  databases.
- **Synthetic, isolated fixtures for process tests.** Tests drive these
  interfaces with synthetic sessions in temporary homes. Real user
  sessions are read-only targets, never edit fixtures.
- **A remote shell alone is not a wake.** A shell on the session's machine
  can run t3-watch, but the bot is woken by t3-watch's webhook, with the
  same durable outbox, ids and signatures as for T3.

### The concrete Codex target

A real thread in the Codex desktop app on the maintainer's MacBook is the
concrete target. Its id, machine, account and project are not recorded
here. It is a read-only target: no prompt is sent to it, it is never
resumed, its files are never modified, and it is not used as a fixture.
Nothing is migrated or copied from it to another machine. Checking it at
all needs access to that MacBook (see "Checking the desktop target on the
MacBook").

## Claude Code adapter

### How it works

Claude Code runs configured hooks on documented events and passes each one
a JSON object on stdin (`session_id`, `hook_event_name`, `transcript_path`,
`cwd`, and event fields such as `notification_type`). An **async** command
hook (`"async": true`) runs in the background and cannot block or change
the session.

`t3-watch hook claude-code` is that hook. It reads the JSON and appends one
line to `<state>/native/claude-code/<session_id>.jsonl` (0600, fsynced). The
line holds only the time, the event name, its type (`notification_type`,
`source` or `reason`) and the tool's name (`Bash`, `mcp__server__tool`, so a
denial closes that tool's request), never a prompt, message, tool input or
path. The
hook always exits 0 and prints nothing, whatever the input. It never
returns a decision.

`t3-watch wait claude-code:<session_id>` watches that log through the same
engine as T3 targets. That means the same daemon sharing, the same `--timeout`
rules (suspended time counts), needs-input, `--notify-on`, webhooks,
`--durable` and the trace all apply. The log is a local file, so watching it
costs a file read every half second and no model calls.

| Hook event | Effect on the watched session |
|---|---|
| `UserPromptSubmit` | A new turn (run ordinal + 1) is running |
| `Stop` | The turn completed |
| `StopFailure` | The turn failed (API error) |
| `PermissionRequest`; `Notification` with `elicitation_dialog`, `elicitation_url_dialog` or `agent_needs_input` | Input is needed: one more open request, with its own id (so each gets its own `pendingKey`), until the turn ends or the next rows close it |
| `Notification` with `permission_prompt` | The dialog of an open `PermissionRequest`: no new request; a new one only if none is open |
| `PermissionDenied` | Closes one open permission request: that tool's oldest, else the oldest. Others stay open |
| `ElicitationResult` | Closes the oldest open elicitation. Others stay open |
| `Stop`, `StopFailure`, `SessionEnd` from a subagent (`agent_id` set) | No change: not the session's own |
| `Notification` with `idle_prompt` | No change. With no turn recorded yet, idle proves nothing about one, so it never settles a wait |
| `SessionEnd` | A running turn ends as `cancelled` |
| A permission dialog rejected with Escape | No hook runs (Claude Code 2.1.319: neither `PermissionDenied` nor `Stop`), so the request stays open until the next `UserPromptSubmit` or `SessionEnd` |
| `SessionStart`, anything else | No change |

Install the hook yourself; t3-watch never edits Claude Code settings.
`t3-watch hook claude-code --print-settings` prints the `hooks` block to
merge into `~/.claude/settings.json` or a project's `.claude/settings.json`.

Tool events (`PostToolUse`, `PostToolUseFailure`) do not clear a pending
request. Tool ids are deliberately not recorded, so a later tool event (a
parallel call, an async hook arriving late) cannot be told apart from the
one that was waiting. A wait can therefore report needs-input after a
person has already approved, until the turn ends. It exits 5 and wakes
the bot, which is the safe side. The installed hooks do not subscribe to
tool events at all. The hook command quotes the binary's path for the
shell, so a path with a space works.

**Pin the turn.** Async hooks can arrive late, so for a moment after a bot
submits a prompt the log may still show the previous turn as finished.
A bot reusing a session reads the current `runOrdinal` before it
submits, then waits with `--expect-min-run-ordinal <that + 1>`. An unpinned
wait in that gap exits 0 on the old turn. An `idle_prompt` with no turn
recorded never settles a wait, and the thread title then says "no turn
recorded yet". A pin does not hide an open request from an older turn: a
waiting request blocks the session, so it is reported first. A bot that
already relayed one (its `pendingKey` from the earlier `needs_input`)
passes `--acknowledge <pendingKey>` with the pin. That matters after an
Escape, which leaves the request open until the bot's next prompt.

### Limits of the first increment

- A session is only seen from the first hook event recorded after the hook
  is installed. A session that went idle before that reports nothing until
  its next event, so the wait keeps waiting.
- If the hook fails (for example on a full disk), that event is lost.
  There is no backfill from the transcript: its format is not a documented
  interface.
- Subagent stops are not treated as the session's turn ending.
- `--until workstream` and `--final-marker` are T3 contracts and are
  refused for this target.

### Acceptance checks (not yet passed)

1. **Passed.** Synthetic hook input: running, completed, failed,
   needs-input, answered, cancelled; the hook never blocks, never prints,
   never stores prompt text; webhook delivery, with and without the
   daemon (`tests/native.rs`).
2. **Passed** on the maintainer's Mac mini with Claude Code 2.1.312. A real Claude Code
   process ran in an isolated temporary project, in print mode, with
   `--setting-sources local --settings <the hooks only> --tools ""
   --strict-mcp-config --no-session-persistence --session-id <uuid>`, so
   neither the user's settings nor their hooks were loaded. It fired
   `SessionStart`, `UserPromptSubmit`, `Stop` and `SessionEnd` through
   the async hook. A `t3-watch wait` registered before the prompt (through
   the daemon, `--durable`) reported `completed` for run 1, and its
   webhook delivered `settled` for `claude-code:<uuid>` about 4.5 s after
   the prompt started. Side effect: Claude Code created an empty project
   entry under `~/.claude/projects` for the temporary path (removed
   afterwards). It may also list that path in `~/.claude.json`, which was
   not edited.
3. **Passed** on the maintainer's Mac mini with Claude Code 2.1.319, in a disposable
   interactive session on a pseudo-terminal (temporary project,
   `--setting-sources local --settings <the hooks only> --strict-mcp-config
   --model haiku --tools Bash --permission-mode manual`). The prompt asked
   for one Bash command outside the project, so a real permission dialog
   appeared. `PermissionRequest` (tool `Bash`) reached a wait registered
   before the session started (through the daemon): exit 5, `needs_input`
   webhook for run 1. The operator rejected the dialog with Escape; nobody
   approved it, and the command did not run. No hook ran for the
   rejection (see the table). In the default permission mode, the same
   command inside the project ran without any dialog, so `manual` mode
   was needed to get one. The folder-trust dialog for the temporary
   project was answered "yes", which Claude Code records in its own
   configuration.
4. **Passed with a local receiver; the bot's continuation is open.** In
   the same live session, a wait pinned to run 2 and acknowledging the
   relayed request was registered first, then a second prompt was typed:
   `settled` webhook for run 2, 1.5 s after the prompt. A print-mode
   session (`--session-id`, then `--resume` of it, pinned the same way)
   woke with `settled` for run 2, 4.1 s after the resume started (the
   model's reply included). The interactive sessions left no transcript
   under `~/.claude/projects` in these runs, so `--resume` of one found
   nothing; the print-mode sessions did leave one. What remains is the
   bot's own receipt and continuation, with timestamps and no user ping.

Checks 3 and 4 are reproduced by `e2e/native/claude_interactive.py`; see
`docs/acceptance-manifest.md` for the runs, logs and cleanup.

## Codex adapter

### How it works

`t3-watch wait codex://threads/<id>` reads the thread from a Codex
app-server through its documented JSON-RPC protocol. It uses the
WebSocket-over-Unix-socket transport, the one `codex app-server --listen
unix://PATH` and the shared daemon's control socket serve. `codex
app-server proxy` only relays stdio bytes to that same socket, so t3-watch
connects to the socket itself and starts no extra process. The socket is
`$T3_WATCH_CODEX_SOCKET`, else
`$CODEX_HOME/app-server-control/app-server-control.sock` (`CODEX_HOME`
defaults to `~/.codex`). The client resolves it and sends it with the wait,
so the daemon reads the socket the caller meant.

Once a second (four times a second while a request is open), on a
connection kept between polls (a dropped one is reopened on the next
poll), it sends:

- `thread/read {threadId}`: the stored thread and its runtime `status`;
- `thread/turns/list {threadId, limit: 50, sortDirection: "desc",
  itemsView: "notLoaded"}`: the latest turn and its status, without item
  content, so no message text is read. With more than 50 turns, the turns
  are counted once by paging, then kept up from the newest page.

The connection's `initialize` names the client `t3-watch`. Those three
methods are all it sends. It also keeps the `thread/status/changed`
notifications the app-server sends every initialized connection, for every
thread and without any subscription (checked with codex-cli 0.166.0: a
connection that only initialized, even one that sent no capabilities and
connected while the thread was already waiting, got every change of the
owner's thread, and no server request). It never sends `thread/resume`, `thread/start`,
`turn/*` or `thread/unsubscribe`, and it never answers a server request:
one that arrives is skipped. Watching costs two local socket calls per
poll and no model calls.

| App-server state | Effect on the watched thread |
|---|---|
| `active`, no flags | The latest turn is running |
| `active` with `waitingOnApproval` or `waitingOnUserInput` | Input is needed: one open request per flag (`codex-<turn>-approval-<n>`, `codex-<turn>-input-<n>`), each with its own `pendingKey` |
| `systemError` | Failed (`error`) |
| Not active, latest turn `completed` | Completed |
| Not active, latest turn `interrupted` or `failed` | Failed (`interrupted`, `failed`) |
| Not active, latest turn `inProgress` | Running (the turn runs on another app-server) |
| No turn yet (also "not materialized yet", before the first user message) | No state: the wait keeps waiting |
| `thread/turns/list` answers "list_turns is not supported yet" (seen while a thread materializes its first message) | Retried for 30 s with the reason in `upstream.lastError`, then the wait fails (exit 2). Any other refusal fails at once |

**Requests are numbered.** Nothing a reader can read tells two requests of
one turn apart: with codex-cli 0.166.0, `thread/read`, the turn and the
thread's items were identical while a first and a second approval were
pending, and a pending request's item is not persisted. The status
notifications do show each one: between two requests the flag goes off,
however briefly (5 ms in a test with no model latency). So `<n>` counts
the times the flag came on in that turn, from the notifications that
arrived since the last read, then the read itself. Only one connection
per thread counts: it holds a counter lock
(`<state>/native/codex/<thread>.counter`) and stores the count in
`<thread>.json`, and any other connection watching the thread only reads
it, so no notification is counted twice. A bot that acknowledged request
1 is woken for request 2 even when both reads in between said "waiting".

A connection that takes the counter over (the first wait, a new process,
or after the counting connection went away) cannot tell whether a flag
that is still on is the request already counted or one that replaced it
while nothing was counting. So it counts it once more: an extra wake for
the same request, never a missed one. In practice, without the daemon each
`--no-daemon` wait takes the counter over, so a request that is still
open gets a new key in every new process and `--acknowledge` does not
carry it from one such wait to the next. Bots that acknowledge requests
should use the daemon (the default).

**Between waits.** When a watch ends while its turn is still running (a
bot relayed a request and is about to wait again), the daemon keeps its
connection, counter and all, and goes on counting from its
notifications. It stops when the newest status says the turn is over, the
connection closes, or 15 minutes pass. A batch that ends one turn and
starts the next keeps it, and the new turn's requests are counted from 1.
The next wait on that thread takes the same connection: one that arrives
while the daemon is reading kept connections waits for that round (about
50 ms per connection) instead of opening another one. If a second
connection to the thread does get kept, the one holding the counter stays.
The daemon stays up meanwhile, and `t3-watch status` shows such
connections as `codexKept`.

The run ordinal is the turn count. A bot that reuses a thread reads the
current `runOrdinal`, starts its turn, and waits with
`--expect-min-run-ordinal <that + 1>`. An unknown thread ends the wait
with an error (exit 2). An unreachable socket is retried until the
timeout, with the reason in `upstream.lastError`.

### Verified (codex-cli 0.166.0 on the maintainer's Mac mini)

These checks used disposable threads on a real `codex app-server --listen
unix://…`, in an isolated `CODEX_HOME` whose model provider was a local mock
of the Responses API, so no OpenAI request was made. A second client
(client A) owned each thread and got its approval requests. They answer
the questions that gated the implementation:

1. **Ids.** App-server thread ids are UUIDv7 (`01a1ba84-154c-7be3-…`), the
   same shape as the desktop target's id. Whether an app-server on the
   MacBook accepts that id is still unchecked.
2. **Second client.** A second connection that only reads sees the
   owner's live status (`active` with `waitingOnApproval`, the turn
   `inProgress`) through `thread/read`, and receives no server request.
3. **Attaching.** `thread/resume` with no overrides on a loaded thread
   changed no file, but it made the second client a recipient of the
   owner's pending approval request (`item/commandExecution/requestApproval`)
   and of the thread's notifications. A watcher that resumed could answer
   for the owner, so t3-watch never resumes.
4. **Approval routing.** Without a resume, only the owner received the
   request. While t3-watch held a needs-input wait (acknowledged, still
   polling), the owner's request stayed open: no `serverRequest/resolved`
   until the owner declined it.
5. **Unload.** With the owner gone, the thread unloaded after 60 s while
   t3-watch polled it once a second. Reading an unloaded thread did not load
   it, took no writer lock and left its rollout file unchanged.
6. **Gaps.** Polling reads the persisted turns, so a turn that ended while
   t3-watch was disconnected or not running is reported by the next read.
   t3-watch read the unloaded thread as completed run 2.
7. **No changes.** Over the same 8 s with and without t3-watch polling, the
   only files that changed in `CODEX_HOME` were the app-server's own log
   database (`logs_2.sqlite-wal`, `-shm`), in both windows. No session,
   rollout, state or lock file changed.
8. **Latency.** The needs-input webhook arrived 62 ms after the owner got
   the approval request. A pinned wait's `settled` webhook arrived 1.0 s
   after `turn/completed`, one poll interval.
9. **Two requests in one turn.** Before engine 14 counted notifications, a
   wait that acknowledged the first approval woke for the second only when
   the model paused 0.5 s or more before asking again; with no pause it
   missed it until the turn ended. Counting notifications, see
   `docs/acceptance-manifest.md` for the runs with no pause, and with the
   second request opening between two waits through the daemon.

### Limits

- Needs-input and the live running status come from the app-server that
  runs the thread. A socket that reaches another app-server (for example
  the shared daemon while the desktop app runs the thread in its own
  process) shows only persisted turns. Completion is then seen once the
  rollout records it, and needs-input is not seen at all.
- Wakes come from polls: a wake can lag the event by up to one interval
  (1 s, 250 ms while a request is open). Notifications are read at each
  poll, so they make the count exact but do not make the wake sooner.
  Turn events (`turn/completed`) reach only subscribers, which needs
  `thread/resume`, ruled out by point 3.
- **Not covered: a request that opens and closes while nothing counts.**
  Only the counting connection counts; other connections only read the
  count. The app-server keeps no record of past requests, so one that is
  opened and answered while no counting connection is open (no wait
  running and no daemon keeping one: `--no-daemon` between waits, a
  stopped daemon, a dropped connection, more than 15 minutes after the
  last wait) leaves no trace. Its owner answered it, so no bot is left
  waiting on it, but it is not reported. A request still open when a
  connection takes the count over is reported, possibly as an extra wake
  for one already relayed (see "Requests are numbered"). Bots that relay
  requests should use the daemon (the default). `tests/codex.rs` states
  both as tests
  (`a_request_that_opens_and_closes_while_nothing_counts_is_never_seen`,
  `a_request_replaced_while_nothing_counts_is_counted_again`).
- A rollback (`thread/rollback`) lowers the turn count, so the ordinal
  goes down with it.
- `--until workstream` and `--final-marker` are T3 contracts and are
  refused for this target.

### Acceptance checks (not yet passed)

1. **Passed.** Mock app-server (`tests/codex.rs`): only the three read
   methods are sent; a server request is never answered; frames are
   masked; fragmented and 64-bit-length messages; reconnect; turn paging;
   pinned waits; one `pendingKey` per request; successive requests counted
   from notifications, including one that comes and goes between two reads,
   and between two waits through the daemon; a wait that starts during a
   drain round keeps the counting connection; a batch that ends one turn
   and starts the next; a request replaced while nothing counts (counted
   again) and one opened and closed while nothing counts (not seen); the
   materializing refusal retried for 30 s; unknown thread; no app-server;
   refused T3 contracts.
2. **Passed.** Real codex-cli 0.166.0 app-server with disposable threads,
   the probes above (fresh thread, needs-input with webhook, no changes,
   completion, pinned wake with webhook, unload, read after unload, two
   requests in one turn), reproduced by `e2e/native/codex/run.sh`. The
   default socket also worked through `codex app-server daemon start` in
   an isolated `CODEX_HOME` (see `docs/acceptance-manifest.md`).
3. **Open.** The desktop app on the MacBook: the read-only checks below,
   then a watch of the research thread, with no prompt sent.
4. **Open.** A bot reusing a Codex thread is woken through its webhook,
   with receipt and continuation timestamps.

### Checking the desktop target on the MacBook

The target stays read-only (see "The concrete Codex target"). Checking it
needs, under the parent's control:

- On that MacBook, as its user, with the desktop app open on the thread:
  `sh e2e/native/codex/desktop-check.sh <t3-watch> <thread id>`. It shows
  `codex --version`, whether
  `~/.codex/app-server-control/app-server-control.sock` exists, and runs
  `t3-watch wait --no-daemon --timeout 5s codex://threads/<thread id>`,
  which sends `initialize`, `thread/read` and `thread/turns/list` only. If
  no socket exists, the desktop app runs its threads in a private
  app-server. Starting `codex app-server daemon` would then be a change
  on that machine that needs the user's approval, and it would see only
  persisted turns (see "Limits").
- A t3-watch build for that Mac's architecture, installed for that user.
- The bot's webhook URL and secret available there, with the receiver
  reachable from that machine.
- Approval to keep a watcher running there while the thread is in use.
