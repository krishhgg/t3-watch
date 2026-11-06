#!/usr/bin/env python3
"""Measures t3-watch against the polling Python watcher on this machine.

Every number comes from a real run:
  - binary size from the file system,
  - startup from wall-clock timing of N sequential runs (includes this
    script's fork/exec cost, so /usr/bin/true is measured as the floor),
  - RSS from `ps` while the watchers are blocked,
  - CPU from `/usr/bin/time -l`, which includes each process's waited-for
    `t3` children,
  - `t3 thread show` invocations counted by a wrapper around the real CLI.

Usage:
  python3 bench/run.py --thread <id of a thread that stays running> [--window 60]
"""

import argparse
import glob
import json
import os
import platform
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
PY_WATCHER = os.path.join(HERE, "baseline_python_watcher.py")
DEVNULL = subprocess.DEVNULL


def run(cmd, **kw):
    return subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, **kw)


def time_runs(cmd, n, env=None):
    samples = []
    for _ in range(n):
        t = time.perf_counter()
        subprocess.run(cmd, stdout=DEVNULL, stderr=DEVNULL, env=env)
        samples.append((time.perf_counter() - t) * 1000)
    samples.sort()
    return {
        "runs": n,
        "medianMs": round(statistics.median(samples), 2),
        "p90Ms": round(samples[max(0, int(n * 0.9) - 1)], 2),
        "minMs": round(samples[0], 2),
    }


TIME_RE = re.compile(r"([\d.]+) real\s+([\d.]+) user\s+([\d.]+) sys")


def parse_time_l(stderr):
    m = TIME_RE.search(stderr or "")
    rss = re.search(r"(\d+)\s+maximum resident set size", stderr or "")
    peak = re.search(r"(\d+)\s+peak memory footprint", stderr or "")
    return {
        "real": float(m.group(1)) if m else None,
        "user": float(m.group(2)) if m else None,
        "sys": float(m.group(3)) if m else None,
        "maxRssBytes": int(rss.group(1)) if rss else None,
        "peakFootprintBytes": int(peak.group(1)) if peak else None,
    }


def memory_of(cmd, env=None, runs=20):
    """Median max RSS and median macOS peak memory footprint over `runs` runs.
    Single samples of tiny processes vary by about 1 MB between runs."""
    samples = [parse_time_l(run(["/usr/bin/time", "-l"] + cmd, env=env).stderr) for _ in range(runs)]
    med = lambda k: int(statistics.median(x[k] for x in samples if x[k] is not None))
    return {"runs": runs, "maxRssBytesMedian": med("maxRssBytes"), "peakFootprintBytesMedian": med("peakFootprintBytes")}


def sleeping_rss_kb(cmd):
    """RSS (ps) of a process blocked in sleep: the floor for a blocked waiter."""
    p = subprocess.Popen(cmd, stdout=DEVNULL, stderr=DEVNULL)
    time.sleep(1.0)
    kb = rss_kb([p.pid]).get(p.pid)
    p.kill()
    p.wait()
    return kb


def child_pid(pid, timeout=5.0):
    """`/usr/bin/time` forks the measured command; return that child's pid."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        out = run(["pgrep", "-P", str(pid)]).stdout.split()
        if out:
            return int(out[0])
        time.sleep(0.01)
    raise RuntimeError("no child for pid %d" % pid)


def rss_kb(pids):
    out = run(["ps", "-o", "pid=,rss=", "-p", ",".join(map(str, pids))]).stdout
    return {int(a): int(b) for a, b in (line.split() for line in out.strip().splitlines())}


def make_wrapper(base, calls_file, real_t3, real_base):
    """A fake T3 base dir whose newest `t3` logs each call, then runs the real
    CLI against the real data dir. Both watchers discover it the same way."""
    vdir = os.path.join(base, "runtime", "versions", "999.0.0-bench")
    os.makedirs(vdir, exist_ok=True)
    path = os.path.join(vdir, "t3")
    with open(path, "w") as f:
        f.write(
            "#!/bin/sh\n"
            "echo call >> '%s'\n"
            'for a; do last="$a"; done\n'
            "exec '%s' thread show --base-dir '%s' --messages 0 --json \"$last\"\n" % (calls_file, real_t3, real_base)
        )
    os.chmod(path, 0o755)
    return path


def count_lines(path):
    try:
        with open(path) as f:
            return sum(1 for _ in f)
    except FileNotFoundError:
        return 0


def thread_status(real_t3, real_base, thread):
    p = run([real_t3, "thread", "show", "--base-dir", real_base, "--messages", "0", "--json", thread])
    try:
        return json.loads(p.stdout)["thread"]["status"]
    except Exception:
        return None


def sample_rss(pids, window, started):
    """RSS samples at 25%, 50% and 75% of the window; keeps the largest total."""
    best = None
    for frac in (0.25, 0.5, 0.75):
        delay = started + window * frac - time.time()
        if delay > 0:
            time.sleep(delay)
        s = rss_kb(pids)
        if best is None or sum(s.values()) > sum(best.values()):
            best = s
    return best


def scenario_rust(binary, thread, n, window, interval, real_t3, real_base):
    home = tempfile.mkdtemp(prefix="t3wb-", dir="/tmp")
    try:
        base = os.path.join(home, "base")
        calls = os.path.join(home, "calls")
        make_wrapper(base, calls, real_t3, real_base)
        env = dict(os.environ, T3_WATCH_HOME=home, T3_WATCH_IDLE_EXIT="1s")
        # Foreground daemon under /usr/bin/time so its CPU includes its t3 children.
        daemon = subprocess.Popen(["/usr/bin/time", "-l", binary, "daemon"], env=env, stdout=DEVNULL, stderr=subprocess.PIPE, text=True)
        sock = os.path.join(home, "daemon.sock")
        while not os.path.exists(sock):
            time.sleep(0.005)
        daemon_pid = child_pid(daemon.pid)
        cmd = [binary, "wait", "--transport", "local", "--base-dir", base, "--interval", "%gs" % interval, "--timeout", "%gs" % window, thread]
        started = time.time()
        clients = [subprocess.Popen(["/usr/bin/time", "-l"] + cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) for _ in range(n)]
        client_pids = [child_pid(c.pid) for c in clients]
        while True:
            st = json.loads(run([binary, "status"], env=env).stdout or "{}")
            if st.get("activeWaits") == n:
                break
            time.sleep(0.01)
        rss = sample_rss(client_pids + [daemon_pid], window, started)
        results = [c.communicate() for c in clients]
        codes = [c.returncode for c in clients]
        # Exit 3 means the client waited out the whole window. Anything else
        # means the thread settled mid-run and the numbers are not comparable.
        if any(code != 3 for code in codes):
            raise RuntimeError("t3-watch clients exited with %s; the thread must stay active" % sorted(set(codes)))
        _, daemon_err = daemon.communicate(timeout=30)
        client_times = [parse_time_l(err) for _, err in results]
        daemon_time = parse_time_l(daemon_err)
        client_rss = [rss[p] for p in client_pids if p in rss]
        return {
            "clients": n,
            "exitCodes": sorted(set(codes)),
            "clientRssKb": {"median": statistics.median(client_rss), "max": max(client_rss), "sum": sum(client_rss)},
            "daemonRssKb": rss.get(daemon_pid),
            "totalRssKb": sum(rss.values()),
            "cpuSeconds": round(sum(t["user"] + t["sys"] for t in client_times) + daemon_time["user"] + daemon_time["sys"], 2),
            "cpuSecondsClientsOnly": round(sum(t["user"] + t["sys"] for t in client_times), 3),
            "t3Calls": count_lines(calls),
        }
    finally:
        shutil.rmtree(home, ignore_errors=True)


def scenario_python(thread, n, window, interval, real_t3, real_base):
    home = tempfile.mkdtemp(prefix="t3pb-", dir="/tmp")
    try:
        base = os.path.join(home, "base")
        calls = os.path.join(home, "calls")
        make_wrapper(base, calls, real_t3, real_base)
        cmd = [sys.executable, PY_WATCHER, "--base-dir", base, "--interval", str(interval), "--max-wait-seconds", str(int(window)), thread]
        started = time.time()
        procs = [subprocess.Popen(["/usr/bin/time", "-l"] + cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) for _ in range(n)]
        pids = [child_pid(p.pid) for p in procs]
        rss = sample_rss(pids, window, started)
        results = [p.communicate() for p in procs]
        if not all("t3_local_watch_timeout" in out for out, _ in results):
            raise RuntimeError("Python watchers saw the thread settle; the thread must stay active")
        times = [parse_time_l(err) for _, err in results]
        per = [rss[p] for p in pids if p in rss]
        return {
            "watchers": n,
            "exitCodes": sorted(set(p.returncode for p in procs)),
            "watcherRssKb": {"median": statistics.median(per), "max": max(per), "sum": sum(per)},
            "totalRssKb": sum(per),
            "cpuSeconds": round(sum(t["user"] + t["sys"] for t in times), 2),
            "t3Calls": count_lines(calls),
        }
    finally:
        shutil.rmtree(home, ignore_errors=True)


def startup(binary, real_t3, real_base, thread_settled):
    home = tempfile.mkdtemp(prefix="t3ws-", dir="/tmp")
    try:
        env = dict(os.environ, T3_WATCH_HOME=home, T3_WATCH_IDLE_EXIT="10s")
        stub_dir = os.path.join(home, "stub")
        os.makedirs(stub_dir)
        stub = os.path.join(stub_dir, "t3")
        with open(stub, "w") as f:
            f.write('#!/bin/sh\nfor a; do last="$a"; done\n'
                    'printf \'{"thread":{"threadId":"%s","status":"completed","latestRunId":"run:thread:x:ordinal:1"}}\\n\' "$last"\n')
        os.chmod(stub, 0o755)
        out = {
            "trueFloor": time_runs(["/usr/bin/true"], 300),
            "t3watchVersion": time_runs([binary, "--version"], 300),
            "pythonBare": time_runs([sys.executable, "-c", "pass"], 100),
            "pythonWatcherHelp": time_runs([sys.executable, PY_WATCHER, "--help"], 100),
        }
        # Warm the daemon, then time full wait round trips on a settled thread.
        run([binary, "wait", "--transport", "local", "--t3-bin", stub, "warm"], env=env)
        out["waitSettledStubT3ViaDaemon"] = time_runs([binary, "wait", "--transport", "local", "--t3-bin", stub, "x"], 100, env=env)
        out["waitSettledStubT3NoDaemon"] = time_runs([binary, "wait", "--no-daemon", "--transport", "local", "--t3-bin", stub, "x"], 100, env=env)
        if thread_settled:
            out["waitSettledRealT3ViaDaemon"] = time_runs([binary, "wait", "--transport", "local", "--base-dir", real_base, thread_settled], 10, env=env)
            out["realT3ThreadShow"] = time_runs([real_t3, "thread", "show", "--base-dir", real_base, "--messages", "0", "--json", thread_settled], 10)
        out["memory"] = {
            "trueFloor": memory_of(["/usr/bin/true"]),
            "t3watchVersion": memory_of([binary, "--version"]),
            "t3watchWaitSettledViaDaemon": memory_of([binary, "wait", "--transport", "local", "--t3-bin", stub, "x"], env=env),
            "pythonBare": memory_of([sys.executable, "-c", "pass"]),
            "sleepingRssKb": {
                "binSleepFloor": sleeping_rss_kb(["/bin/sleep", "30"]),
                "pythonSleeping": sleeping_rss_kb([sys.executable, "-c", "import time; time.sleep(30)"]),
            },
        }
        run([binary, "stop"], env=env)
        return out
    finally:
        shutil.rmtree(home, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--thread", required=True, help="a local thread that stays running for the whole benchmark")
    ap.add_argument("--settled-thread", help="a local thread that has already settled (for wake latency)")
    ap.add_argument("--binary", default=os.path.join(ROOT, "target", "release", "t3-watch"))
    ap.add_argument("--base-dir", default=os.path.expanduser("~/.t3"))
    ap.add_argument("--window", type=float, default=60.0)
    ap.add_argument("--interval", type=float, default=8.0, help="poll interval for both watchers (Python default: 8s)")
    ap.add_argument("--clients", default="1,20")
    ap.add_argument("--out", default=None)
    a = ap.parse_args()

    versions = sorted(glob.glob(os.path.join(a.base_dir, "runtime", "versions", "*", "t3")))
    real_t3 = versions[-1] if versions else shutil.which("t3")
    counts = [int(c) for c in a.clients.split(",")]
    status_before = thread_status(real_t3, a.base_dir, a.thread)
    if status_before not in ("preparing", "queued", "starting", "running"):
        sys.exit("--thread must be active for the whole run (status now: %s)" % status_before)

    res = {
        "host": platform.node(),
        "machine": run(["sysctl", "-n", "machdep.cpu.brand_string"]).stdout.strip() or platform.machine(),
        "os": ("macOS " + platform.mac_ver()[0]) if platform.mac_ver()[0] else "%s %s" % (platform.system(), platform.release()),
        "rustc": run(["rustc", "--version"]).stdout.strip(),
        "pythonVersion": sys.version.split()[0],
        "t3": os.path.basename(os.path.dirname(real_t3)) if versions else real_t3,
        "date": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "window": a.window,
        "interval": a.interval,
        "binaryBytes": os.path.getsize(a.binary),
    }
    print("startup and RSS...", file=sys.stderr)
    res["startup"] = startup(a.binary, real_t3, a.base_dir, a.settled_thread)
    res["rust"], res["python"] = {}, {}
    for n in counts:
        print("rust, %d client(s), %gs window..." % (n, a.window), file=sys.stderr)
        res["rust"][str(n)] = scenario_rust(a.binary, a.thread, n, a.window, a.interval, real_t3, a.base_dir)
        print("python, %d watcher(s), %gs window..." % (n, a.window), file=sys.stderr)
        res["python"][str(n)] = scenario_python(a.thread, n, a.window, a.interval, real_t3, a.base_dir)
    res["threadStatusAfter"] = thread_status(real_t3, a.base_dir, a.thread)
    res["valid"] = res["threadStatusAfter"] in ("preparing", "queued", "starting", "running")

    text = json.dumps(res, indent=2)
    print(text)
    if a.out:
        with open(a.out, "w") as f:
            f.write(text + "\n")


if __name__ == "__main__":
    main()
