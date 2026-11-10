# When a thread needs input

A thread blocked on a question or an approval never finishes by itself. The
wait ends at once with `event` and `outcome` `needs_input` and exit 5, in
either completion mode, when the thread's status is `waiting` or its
`pendingRequestCount` is above zero (an approval can block a run that still
says `running`). That is never reported as completion, and t3-watch never
answers or approves anything: it can only call `t3_thread_wait`,
`t3_thread_read` and `t3_pending_request` with `action` `list` or `read`.

Over MCP each pending request is described from `t3_pending_request list`
(which lists questions only, not approvals) and the end of the activity
timeline. For a delegated child thread the timeline is never read, because
it can hold the final results of the child's earlier runs and T3 marks a
child's final result delivered when it is read; questions there come from
`t3_pending_request read`, and approvals show only as
`pendingRequestCount`:

```json
{"kind":"question","itemId":"turn-item:runtime-request:...","requestId":"runtime-request:...","userOnly":false,
 "questions":[{"header":"Deploy","question":"Use the staging key for this run?","options":["Yes (Recommended)","No"],"multiSelect":false}]}
```

- `kind` is `question`, `approval` or `request`. Approvals have
  `userOnly: true`: a bot may answer questions, but approvals are for the
  user in T3.
- Question text is cut to 600 characters, option labels to 120, option
  descriptions are dropped, and anything that looks like a credential (key
  prefixes such as `sk-` or `ghp_`, JWTs, long mixed alphanumeric runs,
  `password=`/`token:` values, URL userinfo, the word after `Bearer`) is
  replaced with `[redacted]`.
- The thread gets a `pendingKey`. Once the caller has passed the request on,
  `--acknowledge <pendingKey>` keeps waiting past that exact set of
  requests; a different or additional request wakes it again.
- The local transport sees `waiting` and `pendingRequestCount` but has no
  way to read the request itself, so `requests` is empty there.
- A watch reads the thread header before its first long-poll, so a thread
  that is already blocked is reported at once, and then at least every 30 s
  (no long-poll lasts longer, whatever `--slice` says), so a request that
  appears while the run still says `running` is seen within about 30 s.
