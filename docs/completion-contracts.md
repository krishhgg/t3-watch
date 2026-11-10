# Completion contracts

A T3 turn and the work it starts are different things. On 2026-11-06 a
research thread's run 2 finished its own turn after launching four reviewer
threads; their completions came back as parent runs 3 to 5, and the
synthesis was run 6. A watcher waiting for "run 2 or later, finished" exited
at run 2, as it was asked to. t3-watch now offers two explicit contracts:

| `--until` | Settles when | Proof | Transports |
| --- | --- | --- | --- |
| `turn` (default) | The latest run of the thread (at least `--expect-min-run-ordinal`) has a terminal status and no run is active | `t3_thread_read` (MCP) or `t3 thread show` (local) | both |
| `workstream` | The run, its delegated and background tasks, and the parent runs that deliver their completions have all settled | T3's own `t3_thread_wait` with `untilDelegatedWorkSettled`, whose `resultRun` names the run with the final answer | MCP only |

Turn mode is unchanged for ordinary single turns. Over MCP it also asks T3,
without waiting (`t3_thread_wait` with `timeoutMs: 1`), whether the finished
turn left delegated work running, and reports `delegatedWorkPending: true`
with a diagnostic when it did. That is a hint; it never changes the result.
If the probe's answer names a newer run of the thread than the header did,
in `runId` or in `resultRun`, the observation is discarded and the header
read again: a turn is never reported from a header that T3 has already
contradicted. The probe's aggregate status alone is not used for that,
because a completed parent with children still running is exactly what the
hint reports.

Workstream mode settles only when all of these hold, checked on each cycle:

1. `t3_thread_wait` returned `timedOut: false` with a settled status, for
   this thread (`threadId` matches when present).
2. It named `resultRun`, and that run is a run of this thread
   (`run:thread:<this thread>:ordinal:N`) whose status, when given, is
   settled. A run of another thread (a child's) never counts.
3. The header read afterwards shows a settled status, no `activeRunId`, and
   either `resultRun` itself as the latest run, or the same latest run a read
   had already seen before this wait was sent (a wait without `runId` covers
   the latest run and its delegated work, so T3 may name an earlier run as
   the one holding the answer). A `resultRun` newer than the header's latest
   run, or a header that disagrees with the proof about whether the same run
   succeeded, is re-checked, never settled from either side.
4. Nothing else read in the same cycle contradicts the header. Every later
   response (the delegated-work probe, the activity timeline, each
   `--final-marker` listing and message page, and a final header read after
   the marker check) is compared with it: a different latest run, a run that
   became active, a changed status, or input that appeared voids the
   observation. The cycle then reports the fresh header marked for
   re-checking (it can still report input, never completion or failure) and
   the next cycle starts over with a new wait and a new proof.

The outcome of a proven workstream is the status of `resultRun`, the run
T3 names as holding the final answer, as T3 reported it in the proof: a
`failed`, `interrupted` or `cancelled` result run is a failure (exit 6) even
if a later run completed, and a failed latest run after a completed result
run is a failure too. Success needs both to have succeeded.

If a newer run appears (a late child completion continuing the parent),
t3-watch waits again on it. If T3 finishes the wait without `resultRun`,
nothing can prove the workstream, and the result is an error
(`error.kind = "unsupported"`, exit 2), never a completion.

What this cannot prove: that nobody will start a new run later. A user
message or a delegated task created after T3 reported the workstream settled
starts a new workstream. And the proof is only as good as T3's own
bookkeeping of delegated work.

`--final-marker <TEXT>` is an explicit final-result contract for both modes
and both transports: the final assistant message of the deciding run (the
latest run, or `resultRun` in workstream mode) must contain TEXT. Put the
marker in the instructions ("end your final synthesis with
`SYNTHESIS-COMPLETE`"); until it appears, a completed status keeps waiting,
with `finalMarkerSeen: false` and a diagnostic. A failed run ends the wait
without the marker. Over MCP the message is found with a one-character
listing of the last 100 timeline items and then read alone, page by page.
T3 marks a delegated child's final result as delivered when it is read, so
`--final-marker` refuses child threads (exit 2, `unsupported`); watch the
parent instead.
