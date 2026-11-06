---
name: t3-code-watch
description: Wait for T3 Code threads to finish without spending tokens. Use after launching or messaging a T3 thread (t3_thread_launch, t3_thread_send, delegate_task) when you would otherwise poll t3_thread_read or keep a subagent open. Starts `t3-watch wait` as a background command, ends the turn, and resumes when the command exits with one JSON line.
---

# Watching T3 Code threads with t3-watch

`t3-watch wait` blocks in the operating system until the threads you name
leave the active statuses (`preparing`, `queued`, `starting`, `running`). It
then prints one JSON object and exits. Your harness wakes you when the
background command exits, so you spend no tokens while the thread works.

## When to use it

- You started a long T3 thread and have nothing else to do until it finishes.
- You sent a follow-up turn with `t3_thread_send` and need its result.
- You are fanning out several threads and want to resume when all of them (or
  the first one) settle.

Skip it when the thread is already settled or when you have other useful work
to do in the meantime. Check back later instead.

## Steps

1. Launch or message the thread and note its `threadId` (or its full T3
   thread URL) and the run ordinal of the turn you care about. A run id looks
   like `run:thread:<id>:ordinal:2`, so that turn's ordinal is 2.
2. Start the watcher as a background command and end your turn right away.
   Do not poll it.

   ```sh
   t3-watch wait <threadId-or-url> --expect-min-run-ordinal 2 --timeout 2h
   ```

3. When the command exits, read its stdout and act on `event`.

## Choosing the target

| You have | Pass |
| --- | --- |
| A thread on this machine | the bare thread id (local transport, no setup) |
| A relay URL `https://prod-....t3coderelay.com/<envId>/<threadId>` | the URL as is; it picks the saved machine and environment |
| A `t3-thread://v2/<envId>/<threadId>` link | the link, plus `--machine <name>` if it is not on the default machine |
| Several threads | all of them; add `--mode any` to resume on the first one |

Remote machines need a one-time read-only pairing (`t3-watch auth login
<relay-origin>`, or `oauth-start` and `oauth-finish` on a headless box). The
token only has `orchestration:read`, so the watcher can never send messages
or run anything.

## Always pass `--expect-min-run-ordinal` after a follow-up

Right after `t3_thread_send`, the thread may still report the previous turn as
`completed` for a moment. Without the ordinal check the watcher would exit at
once on that stale state. Pass the ordinal of the run you are waiting for.

## Reading the result

```json
{"event":"settled","mode":"all","elapsedMs":81234,"via":"daemon",
 "threads":[{"machine":"local","threadId":"mcp:launch:...","status":"completed",
             "settled":true,"latestRunId":"run:thread:...:ordinal:2","runOrdinal":2,
             "title":"...","link":"..."}]}
```

| Exit | `event` | What to do |
| --- | --- | --- |
| 0 | `settled` | Read the thread (`t3_thread_read`) and continue. `waiting` means it needs input from you; `failed`, `interrupted` and `cancelled` need attention. |
| 3 | `timeout` | Nothing finished in time. Re-run the same command if you still want to wait. `lastError` shows network trouble, if any. |
| 2 | `error` | `error.kind` says why: `auth` (pair again), `not_found` (wrong id), `usage` or `config`. Do not retry blindly. |

## Harness recipes

Claude Code: run it with the Bash tool and `run_in_background: true`, then end
the turn. You get a notification when it exits.

Cursor, Codex and other agents with background terminals: start it as a
background or detached command and stop. Read the output when the terminal
reports completion.

Grok Bot or any shell-driven bot: run it as a background shell job and hook
its completion:

```sh
t3-watch wait "$THREAD" --expect-min-run-ordinal "$ORD" > /tmp/watch.json; notify-bot /tmp/watch.json
```

Shell scripts: `t3-watch wait a b c && echo "all done"`. Exit codes compose
with `&&`, `||` and `set -e`.

## Cost model

Every `t3-watch wait` on a machine shares one background daemon. Twenty agents
watching the same thread cost one upstream long-poll (remote) or one `t3
thread show` per interval (local), not twenty. The daemon exits by itself 60
seconds after its last waiter leaves. Use `t3-watch status` to see what it is
watching.
