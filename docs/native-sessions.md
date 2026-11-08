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

`codex://threads/01a1b891-e945-7f70-b755-db5ece6dcaff` lives on
`MacBook-Pro-724` (user `krishgarg`), in the project
`/Users/krishgarg/Documents/socials/market-research-kit`. It is a read-only
target: no prompts are sent to it, its files are never modified, and it is
not used as a fixture. The Mac mini does not have that project, and nothing
is migrated or duplicated to it. Checking it at all needs access to the
MacBook, which the parent controls (see "Checking the desktop target on the MacBook").

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
2. **Passed** on the Mac mini with Claude Code 2.1.312. A real Claude Code
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
3. **Passed** on the Mac mini with Claude Code 2.1.319, in a disposable
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
methods are all it sends. It never sends `thread/resume`, `thread/start`,
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

**Requests are numbered.** A reader sees only the flag, and nothing it can
read changes between two requests of one turn: with codex-cli 0.166.0,
`thread/read`, the turn and the thread's items were identical while a
first and a second approval were pending, and a pending request's item is
not persisted. So `<n>` counts the times a t3-watch process saw the flag
come on in that turn. The count is kept in
`<state>/native/codex/<thread>.json` (under a lock), so every process that
watches the thread uses the same numbers, and a bot that acknowledged
request 1 is woken for request 2.

The run ordinal is the turn count. A bot that reuses a thread reads the
current `runOrdinal`, starts its turn, and waits with
`--expect-min-run-ordinal <that + 1>`. An unknown thread ends the wait
with an error (exit 2). An unreachable socket is retried until the
timeout, with the reason in `upstream.lastError`.

### Verified (codex-cli 0.166.0 on the Mac mini)

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
9. **Two requests in one turn.** A wait that acknowledged the first
   approval and kept running while the owner declined it woke for the
   second under its own key (`…-approval-2`) when the model took 3 s or
   0.5 s before asking again. When the second request followed at once (a
   mock model with no latency), the flag never went off between polls, so
   the second request shared the first one's key and the wait woke only at
   the end of the turn.

### Limits

- Needs-input and the live running status come from the app-server that
  runs the thread. A socket that reaches another app-server (for example
  the shared daemon while the desktop app runs the thread in its own
  process) shows only persisted turns. Completion is then seen once the
  rollout records it, and needs-input is not seen at all.
- Polling, not push: a wake can lag the event by up to one interval. Live
  events need `thread/resume`, which point 3 rules out.
- A request that opens within about 250 ms of the previous one's answer,
  or while no t3-watch process watches the thread, shares the previous
  one's number. A wait that acknowledged that one is then woken only when
  the turn ends; the owner still sees the request in their own client.
- A rollback (`thread/rollback`) lowers the turn count, so the ordinal
  goes down with it.
- `--until workstream` and `--final-marker` are T3 contracts and are
  refused for this target.

### Acceptance checks (not yet passed)

1. **Passed.** Mock app-server (`tests/codex.rs`): only the three read
   methods are sent; a server request is never answered; frames are
   masked; fragmented and 64-bit-length messages; reconnect; turn paging;
   pinned waits; one `pendingKey` per request, and successive requests in
   a turn counted across processes; unknown thread; no app-server;
   refused T3 contracts.
2. **Passed.** Real codex-cli 0.166.0 app-server with disposable threads,
   the probes above (fresh thread, needs-input with webhook, no changes,
   completion, pinned wake with webhook, unload, read after unload, two
   requests in one turn).
3. **Open.** The desktop app on the MacBook: the read-only checks below,
   then a watch of the research thread, with no prompt sent.
4. **Open.** A bot reusing a Codex thread is woken through its webhook,
   with receipt and continuation timestamps.

### Checking the desktop target on the MacBook

The target stays read-only (see "The concrete Codex target"). Checking it
needs, under the parent's control:

- On that MacBook, as `krishgarg`: `codex --version`; whether
  `~/.codex/app-server-control/app-server-control.sock` exists while the
  desktop app has the thread open; then
  `t3-watch wait --no-daemon --timeout 5s codex://threads/01a1b891-e945-7f70-b755-db5ece6dcaff`,
  which sends `initialize`, `thread/read` and `thread/turns/list` only. If
  no socket exists, the desktop app runs its threads in a private
  app-server. Starting `codex app-server daemon` would then be a change
  on that machine that needs the user's approval, and it would see only
  persisted turns (see "Limits").
- A t3-watch build for that Mac's architecture, installed for that user.
- The bot's webhook URL and secret available there, with the receiver
  reachable from that machine.
- Approval to keep a watcher running there while the thread is in use.
