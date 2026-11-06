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

## Delegated work

If the thread delegates to other threads and you need their results too, add
`--until-delegated-settled`. It only works over MCP (a paired machine or a
thread URL); for a local thread id it exits 2 at once, so pair the machine
instead of dropping the flag.

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
| 3 | `timeout` | Nothing finished in time. Re-run the same command if you still want to wait. `lastError` shows network trouble, and `diagnostic` a status this t3-watch does not recognize. |
| 2 | `error` | `error.kind` says why: `auth` (pair again), `not_found` (wrong id), `usage`, `config` or `limit`. Do not retry blindly. |
| 4 | `settled` | The threads settled, but the webhook or `--on-settle` program failed; `notification` says which. Continue as for 0. |

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

Bots with no background terminal (a chat bot behind a webhook, a cron job):
let t3-watch call them. Keep the URL and secret in the environment, not in
argv:

```sh
BOT_URL=https://bot.example/hooks/t3 BOT_SECRET=... \
  t3-watch wait "$THREAD" --notify-on all \
    --webhook-url-env BOT_URL --webhook-secret-env BOT_SECRET &
```

The bot receives the same JSON in a POST, signed with
`X-T3-Watch-Signature: sha256=HMAC(secret, "<X-T3-Watch-Timestamp>.<X-T3-Watch-Id>.<body>")`.
`--on-settle <program> --on-settle-arg <arg>` runs a local program with the
JSON on stdin instead. `--notify-on all` also notifies on timeout and error,
including a broken config found before the wait starts, so an expired token
cannot strand an unattended bot. Files given to `--webhook-url-file` or
`--webhook-secret-file` must be yours and `chmod 600`.

## Cost model

Every `t3-watch wait` on a machine shares one background daemon. Twenty agents
watching the same thread cost one upstream long-poll (remote) or one `t3
thread show` per interval (local), not twenty. The daemon exits by itself 60
seconds after its last waiter leaves. Use `t3-watch status` to see what it is
watching.
