#!/usr/bin/env python3
"""
Deterministic local T3 thread watcher on macOS.
Polls local `t3 thread show --messages 0 --json` cheaply with zero AI usage
and exits 0 once watched thread(s) leave active statuses, waking Grok Bot via
background-Shell completion.
"""
import argparse, glob, json, os, subprocess, sys, time

ACTIVE = {"preparing", "queued", "starting", "running"}

def find_t3_bin(base_dir: str) -> str:
    candidates = sorted(glob.glob(os.path.join(base_dir, "runtime", "versions", "*", "t3")))
    if candidates:
        return candidates[-1]
    shim = os.path.join(base_dir, "bin", "t3")
    if os.path.exists(shim):
        return shim
    return "t3"

def check_one(t3_bin: str, base_dir: str, tid: str) -> dict:
    proc = subprocess.run(
        [t3_bin, "thread", "show", "--base-dir", base_dir, "--messages", "0", "--json", tid],
        capture_output=True, text=True, timeout=15
    )
    if proc.returncode != 0:
        raise RuntimeError(proc.stderr.strip() or f"t3 exited {proc.returncode}")
    data = json.loads(proc.stdout)
    t = data.get("thread", data)
    return {
        "threadId": t.get("threadId") or tid,
        "title": t.get("title"),
        "status": t.get("status"),
        "latestRunId": t.get("latestRunId"),
        "activeRunId": t.get("activeRunId"),
        "pendingRequestCount": t.get("pendingRequestCount"),
        "itemCount": t.get("itemCount"),
        "runCount": t.get("runCount"),
    }

def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("threads", nargs="+")
    ap.add_argument("--base-dir", default=os.path.expanduser("~/.t3"))
    ap.add_argument("--interval", type=float, default=8.0)
    ap.add_argument("--mode", choices=["all", "any"], default="all")
    ap.add_argument("--expect-min-run-ordinal", type=int, default=0,
                    help="Wait until latestRunId ordinal >= N and status not active")
    ap.add_argument("--max-wait-seconds", type=int, default=14400)
    args = ap.parse_args()

    t3_bin = find_t3_bin(args.base_dir)
    started = time.time()
    deadline = started + args.max_wait_seconds

    while True:
        settled = []
        still_active = []
        for tid in args.threads:
            try:
                info = check_one(t3_bin, args.base_dir, tid)
                ord_ok = True
                if args.expect_min_run_ordinal > 0:
                    r_id = info.get("latestRunId") or ""
                    if ":ordinal:" in r_id:
                        try:
                            ord_num = int(r_id.rsplit(":ordinal:", 1)[1])
                            ord_ok = ord_num >= args.expect_min_run_ordinal
                        except ValueError:
                            pass
                if info.get("status") not in ACTIVE and ord_ok:
                    settled.append(info)
                else:
                    still_active.append(info)
            except Exception as e:
                still_active.append({"threadId": tid, "status": "transient_error", "error": str(e)[:200]})

        if (args.mode == "any" and settled) or (args.mode == "all" and not still_active):
            print(json.dumps({
                "event": "t3_local_watch_settled",
                "elapsedSeconds": round(time.time() - started, 1),
                "threads": settled + still_active,
            }, indent=2))
            return 0

        if time.time() >= deadline:
            print(json.dumps({
                "event": "t3_local_watch_timeout",
                "elapsedSeconds": round(time.time() - started, 1),
                "threads": settled + still_active,
            }, indent=2))
            return 0

        time.sleep(args.interval)

if __name__ == "__main__":
    sys.exit(main())
