#!/usr/bin/env python3
"""Delivery timeline of one t3-watch wait, from its trace, for acceptance.

Reads `<state>/trace.jsonl` (written by t3-watch; ids, statuses and times
only) and prints, for one wait, every step from T3's run completion to the
webhook's 2xx, with the time since completion and since the previous step.
Read-only; standard library only. Run it with `python3 -I`.

The receiver side is not in the trace. Pass what the receiving bot's own
records show, and the timeline extends to them:

  --receipt TS        when the bot's webhook routine received the event
  --continuation TS   when the correct bot continued (its next message or
                      action naming the thread), without any user ping
  --bot NAME          which bot continued (recorded as given)

The last line is a JSON summary to paste into the acceptance record. This
tool proves nothing by itself about a bot: the receipt and continuation
times must come from the bot's own records, per event, for completion,
needs-input and durable replay separately.

The acceptance event is one notification: the one named by --id, else the
notification of the wait's final outcome (the `wait.outcome` line's event
and eventSeq). Only its delivery counts toward `completionTo2xxS`; progress
events (run_failed, outages) are listed apart and never stand in for it,
even when they are the latest notification prepared. If the final outcome
had no notification (say a timeout under `--notify-on settled`), the summary
says it is missing (`acceptanceEvent.missing`); missing or not delivered,
`acceptanceDelivered` is false and `completionTo2xxS` null.

Usage: timeline.py TRACE [--watch WATCH_ID] [--id EVENT_ID] [--receipt TS]
                         [--continuation TS] [--bot NAME]
"""

import argparse
import datetime
import json
import sys


def ts(s):
    if not s:
        return None
    return datetime.datetime.fromisoformat(s.replace("Z", "+00:00"))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("trace")
    ap.add_argument("--watch")
    ap.add_argument("--id", help="the acceptance notification id (default: the final outcome's notification)")
    ap.add_argument("--receipt")
    ap.add_argument("--continuation")
    ap.add_argument("--bot")
    a = ap.parse_args()

    lines = []
    with open(a.trace) as f:
        for raw in f:
            try:
                lines.append(json.loads(raw))
            except ValueError:
                continue
    starts = [l for l in lines if l.get("kind") == "wait.start"]
    if not starts:
        sys.exit("no wait in this trace")
    watch = a.watch or starts[-1].get("watchId")
    mine = [l for l in lines if l.get("watchId") == watch]
    if not mine:
        sys.exit(f"no lines for watch {watch}")
    prepared = [l for l in lines if l.get("kind") == "notify.prepared" and l.get("watchId") == watch]
    ids = {l["id"] for l in prepared}
    outcome = next((l for l in mine if l.get("kind") == "wait.outcome"), {})
    missing = None
    if a.id:
        target = next((l for l in prepared if l["id"] == a.id), None)
        if not target:
            sys.exit(f"notification {a.id} is not an event of watch {watch}")
    elif not outcome:
        target = None
        missing = "the wait has no final outcome in this trace"
    else:
        # The final outcome's own notification: same event, and the same
        # eventSeq when the trace records it (older traces do not).
        final = [
            l
            for l in prepared
            if l.get("event") == outcome.get("event")
            and (outcome.get("eventSeq") is None or l.get("eventSeq") == outcome.get("eventSeq"))
        ]
        target = max(final, key=lambda l: (l.get("eventSeq") or 0), default=None)
        if not target:
            missing = (
                f"no notification was prepared for the final outcome ({outcome.get('event')}); "
                "check --notify-on"
            )
    threads = {t.get("threadId") for t in outcome.get("threads") or []}
    start_at = ts(mine[0]["at"])
    end_at = ts(next((l["at"] for l in mine if l.get("kind") == "wait.exit"), mine[-1]["at"]))
    steps = [
        l
        for l in lines
        if l.get("watchId") == watch
        or l.get("id") in ids
        or (
            l.get("kind") in ("watch.settled", "watch.proof")
            and l.get("threadId") in threads
            and start_at <= ts(l["at"]) <= end_at
        )
    ]
    steps.sort(key=lambda l: l["at"])

    completed = None
    for t in outcome.get("threads") or []:
        completed = ts(t.get("runCompletedAt")) or completed
    anchor = completed or start_at
    marks = []
    if completed:
        marks.append((completed, "run.completedAt (T3)", {}))
    for l in steps:
        detail = {k: v for k, v in l.items() if k not in ("at", "kind", "pid", "engine", "rev")}
        marks.append((ts(l["at"]), l["kind"], detail))
    for label, value in (("receipt (bot)", a.receipt), ("continuation (bot)", a.continuation)):
        if value:
            marks.append((ts(value), label, {"bot": a.bot} if a.bot else {}))
    marks.sort(key=lambda m: m[0])

    print(f"watch {watch}  (times relative to {'run completion' if completed else 'wait start'})")
    prev = None
    for at, kind, detail in marks:
        since = (at - anchor).total_seconds()
        step = (at - prev).total_seconds() if prev else 0.0
        brief = {k: detail[k] for k in ("id", "event", "status", "attempt", "error", "nextAttemptAt",
                                        "notBefore", "code", "outcome", "runId", "daemon", "bot")
                 if k in detail and detail[k] is not None}
        print(f"  {at.isoformat()}  +{since:9.3f}s  (+{step:8.3f}s)  {kind:22s} {json.dumps(brief)[:160]}")
        prev = at

    def first(*kinds):
        return next((at for at, k, _ in marks if k in kinds), None)

    def is_2xx(d):
        return isinstance(d.get("status"), int) and 200 <= d["status"] < 300

    def delivered_at(event_id):
        return next(
            (
                at
                for at, k, d in marks
                if d.get("id") == event_id
                and ((k in ("notify.attempt", "outbox.attempt") and is_2xx(d)) or k == "outbox.delivered")
            ),
            None,
        )

    def disposition(event_id):
        for at, k, d in reversed(marks):
            if d.get("id") == event_id and k in ("outbox.expired", "outbox.dropped", "outbox.delivered"):
                return k.split(".")[1]
        return None

    target_id = target["id"] if target else None
    delivered = delivered_at(target_id) if target_id else None
    progress = [
        {
            "id": l["id"],
            "event": l.get("event"),
            "eventSeq": l.get("eventSeq"),
            "deliveredAt": (delivered_at(l["id"]).isoformat() if delivered_at(l["id"]) else None),
        }
        for l in prepared
        if l["id"] != target_id
    ]
    proof = first("watch.proof", "watch.settled")

    def secs(x, y):
        return round((y - x).total_seconds(), 3) if x and y else None

    summary = {
        "watchId": watch,
        "eventIds": sorted(ids),
        "acceptanceEvent": {
            "id": target_id,
            "event": target.get("event") if target else outcome.get("event"),
            "eventSeq": target.get("eventSeq") if target else outcome.get("eventSeq"),
            "missing": missing,
        },
        "acceptanceDelivered": delivered is not None,
        "acceptanceDisposition": disposition(target_id) if target_id else None,
        "progressEvents": progress,
        "exitCode": next((l.get("code") for l in mine if l.get("kind") == "wait.exit"), None),
        "runCompletedAt": completed.isoformat() if completed else None,
        "completionToProofS": secs(completed, proof),
        "proofToEndS": secs(proof, ts(outcome.get("endedAt"))),
        "completionTo2xxS": secs(completed, delivered),
        "twoXXToReceiptS": secs(delivered, ts(a.receipt)),
        "receiptToContinuationS": secs(ts(a.receipt), ts(a.continuation)),
        "completionToContinuationS": secs(completed, ts(a.continuation)),
        "bot": a.bot,
        "verified": "no: receipt and continuation must come from the bot's own records",
    }
    print(json.dumps(summary))


if __name__ == "__main__":
    main()
