# Native sessions: Codex and Claude Code (experimental)

t3-watch was built to watch T3 threads. This document scopes two more kinds
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
| Claude Code (`claude-code:<session_id>`) | Planned. First increment: hook ingestion and waiting, tested with synthetic hook input. |
| Codex (`codex://threads/<id>`) | Not implemented. The target is refused with an explicit "not supported yet" error. The protocol facts are verified below; the open questions are listed. |

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
MacBook, which the parent controls (see "MacBook integration needs").

## Claude Code adapter

### How it works

Claude Code runs configured hooks on documented events and passes each one
a JSON object on stdin (`session_id`, `hook_event_name`, `transcript_path`,
`cwd`, and event fields such as `notification_type`). An **async** command
hook (`"async": true`) runs in the background and cannot block or change
the session.

`t3-watch hook claude-code` is that hook. It reads the JSON and appends one
line to `<state>/native/claude-code/<session_id>.jsonl` (0600, fsynced). The
line holds only the time, the event name and its type (`notification_type`,
`source` or `reason`), never a prompt, message, tool input or path. The
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
| `Notification` with `permission_prompt`, `elicitation_dialog`, `elicitation_url_dialog` or `agent_needs_input`; `PermissionRequest` | Input is needed (a pending request) |
| `PostToolUse`, `PostToolUseFailure`, `PermissionDenied`, `ElicitationResult` | The pending request was answered |
| `Notification` with `idle_prompt` | Idle and waiting for a prompt: settled if no turn was seen yet |
| `SessionEnd` | A running turn ends as `cancelled` |
| `SessionStart`, anything else | No change |

Install the hook yourself; t3-watch never edits Claude Code settings.
`t3-watch hook claude-code --print-settings` prints the `hooks` block to
merge into `~/.claude/settings.json` or a project's `.claude/settings.json`.

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

1. Synthetic hook input: running, completed, failed, needs-input,
   answered, cancelled; the hook never blocks, never prints, never stores
   prompt text; webhook and `--durable` delivery.
2. A real Claude Code process in an isolated temporary project, with the
   hook loaded only from that test's settings, completes a trivial prompt.
   t3-watch then reports `completed` for its `session_id`, and no user
   settings or hooks are touched.
3. The same for a permission prompt (needs-input), answered by a human in
   an interactive session, not by t3-watch.
4. A bot reusing an existing session is woken through its webhook, with
   receipt and continuation timestamps and no user ping.

## Codex adapter

### Verified (codex-cli 0.166.0 on the Mac mini, read-only)

The official app-server documentation and the protocol schema this CLI
generates (`codex app-server generate-json-schema`) agree on the following.

- Messages are JSON-RPC over stdio (JSONL), plus WebSocket over `ws://`
  (experimental) or a Unix socket (`unix://`). Each connection sends
  `initialize` then `initialized`.
- `codex app-server daemon` manages a shared local app-server, and
  `codex app-server proxy [--sock PATH]` connects stdio to its control
  socket. This is the CLI's own way for a second client to reach a running
  app-server.
- Read-only methods: `thread/read` (stored metadata and runtime `status`;
  it does not load the thread or emit `thread/started`), `thread/list`,
  `thread/loaded/list`, `thread/turns/list`, `thread/items/list`.
- No subscribe method exists. A connection receives a thread's events
  after `thread/start` or `thread/resume`; `thread/unsubscribe` ends that.
  A thread unloads after its last subscriber leaves and 60 s pass, with
  `thread/status/changed` (`notLoaded`) and `thread/closed`.
- Watch notifications: `turn/started`, `turn/completed` (status
  `completed` or `interrupted`), `thread/status/changed` (`notLoaded`,
  `idle`, `systemError`, `active` with `activeFlags` such as
  `waitingOnApproval`), `item/started`, `item/completed`.
- Requests from the server that mean "needs input":
  `item/commandExecution/requestApproval`,
  `item/fileChange/requestApproval`, `item/permissions/requestApproval`,
  `item/tool/requestUserInput`, `mcpServer/elicitation/request`. The
  notification `serverRequest/resolved` (`threadId`, `requestId`) says one
  was answered or cleared.

### Planned design

`codex://threads/<id>` targets are watched by a t3-watch process on the
machine that runs the thread:

1. Connect through `codex app-server proxy` and initialize as
   `t3-watch`, opting out of delta notifications.
2. Read the thread (`thread/read`, `thread/turns/list`) for its status and
   latest turn.
3. Follow live events only through a mechanism verified not to change the
   thread (see the open questions).
4. Report `turn/completed` as completed or interrupted, and any pending
   server request or `waitingOnApproval` as needs-input. Never answer a
   request.
5. On every reconnect, re-read and compare the latest turn id and status
   with the last one seen, so that a turn finished while disconnected is
   still reported.

### Open questions, which gate any implementation

1. **Ids.** Does the UUID in `codex://threads/<uuid>` equal the
   app-server's `threadId`? The documentation's examples show `thr_…` ids.
2. **Second client.** Does the desktop app on the MacBook run its threads
   in the shared daemon that `proxy` reaches, or in its own private
   app-server? If it is private, no second client can see live events,
   and only `thread/read` polling (no model involved) remains.
3. **Attaching without changes.** Does `thread/resume` with no overrides
   on a thread that is already loaded only subscribe? No rollout write, no
   settings change, no change of owner, and no `updatedAt` change (the
   documentation says it does not update `updatedAt`). To be verified with
   a synthetic thread in an isolated `CODEX_HOME`, with a local mock model
   provider so that no OpenAI request is made.
4. **Approval routing.** Are approval and user-input requests sent to every
   subscribed connection, or only to the owner? An unanswered request on
   t3-watch's connection must never block or cancel the owner's.
5. **Unload.** Does t3-watch's subscription keep a thread loaded that would
   otherwise unload? It must not change how the app behaves.
6. **Gaps.** What do `thread/turns/list` cursors guarantee across a
   reconnect?

### MacBook integration needs (parent-controlled)

- On `MacBook-Pro-724` as `krishgarg`, read-only:
  - `codex --version` and `codex app-server daemon version`;
  - whether the app-server control socket exists;
  - over `proxy`: `initialize`, then `thread/read` of
    `01a1b891-e945-7f70-b755-db5ece6dcaff` and `thread/loaded/list`.
  
  None of these sends a prompt or touches the project.
- A t3-watch build for that Mac's architecture, installed for that user.
- The bot's webhook URL and secret available there, with the receiver
  reachable from that machine.
- Approval to keep a watcher running there while the thread is in use.
