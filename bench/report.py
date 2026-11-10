#!/usr/bin/env python3
"""Renders a bench/run.py result file as the Markdown tables in docs/benchmarks.md."""

import json
import sys


def mb_kb(kb):
    return "%.1f MiB" % (kb / 1024)


def mb_b(b):
    return "%.2f MiB" % (b / 1024 / 1024)


def main():
    r = json.load(open(sys.argv[1]))
    s = r["startup"]
    m = s["memory"]
    rust, py = r["rust"], r["python"]
    out = []
    w = out.append
    w("Measured on `%s` (%s, %s), %s, Python %s, T3 `%s`, on %s." % (
        r["host"], r["machine"], r["os"], r["rustc"], r["pythonVersion"], r["t3"], r["date"][:10]))
    w("Raw results: [`%s`](%s). Reproduce with `bench/run.py`." % (sys.argv[2], sys.argv[2]))
    w("")
    w("### Size and startup")
    w("")
    w("| | Result |")
    w("| --- | --- |")
    w("| Release binary (`opt-level = \"z\"`, LTO, stripped) | %s bytes (%s) |" % (format(r["binaryBytes"], ","), mb_b(r["binaryBytes"])))
    w("| `t3-watch --version`, median of %d runs | %.2f ms (floor: `/usr/bin/true` %.2f ms) |" % (
        s["t3watchVersion"]["runs"], s["t3watchVersion"]["medianMs"], s["trueFloor"]["medianMs"]))
    w("| `python3 -c pass`, median of %d runs | %.2f ms |" % (s["pythonBare"]["runs"], s["pythonBare"]["medianMs"]))
    w("| Python watcher `--help`, median of %d runs | %.2f ms |" % (s["pythonWatcherHelp"]["runs"], s["pythonWatcherHelp"]["medianMs"]))
    w("| `wait` on a settled thread via the warm daemon, stub `t3` (`sh`), median of %d | %.2f ms |" % (
        s["waitSettledStubT3ViaDaemon"]["runs"], s["waitSettledStubT3ViaDaemon"]["medianMs"]))
    w("| Same with `--no-daemon` | %.2f ms |" % s["waitSettledStubT3NoDaemon"]["medianMs"])
    if "waitSettledRealT3ViaDaemon" in s:
        w("| `wait` on a settled thread via the daemon, real `t3`, median of %d | %.1f ms (`t3 thread show` alone: %.1f ms) |" % (
            s["waitSettledRealT3ViaDaemon"]["runs"], s["waitSettledRealT3ViaDaemon"]["medianMs"], s["realT3ThreadShow"]["medianMs"]))
    w("")
    w("Startup times are wall clock from a Python `subprocess.run` loop, so they include about 1 ms of fork and exec cost; the `/usr/bin/true` row is that floor.")
    w("")
    w("### Memory of a single process")
    w("")
    w("| Process | Max RSS (median of 20) | Peak footprint (median of 20) |")
    w("| --- | --- | --- |")
    for label, key in [("`/usr/bin/true` (floor)", "trueFloor"), ("`t3-watch --version`", "t3watchVersion"),
                       ("`t3-watch wait` client, settled thread", "t3watchWaitSettledViaDaemon"), ("`python3 -c pass`", "pythonBare")]:
        w("| %s | %s | %s |" % (label, mb_b(m[key]["maxRssBytesMedian"]), mb_b(m[key]["peakFootprintBytesMedian"])))
    w("")
    w("A process blocked in `sleep` has %s RSS for `/bin/sleep` and %s for `python3 -c 'time.sleep(30)'`." % (
        mb_kb(m["sleepingRssKb"]["binSleepFloor"]), mb_kb(m["sleepingRssKb"]["pythonSleeping"])))
    w("")
    w("### Watching one running thread for %d s on the real local T3 server" % r["window"])
    w("")
    w("Both tools poll the local `t3` CLI every %g s (the Python watcher's default) and time out after the window. CPU is user plus system time from `/usr/bin/time -l`, including the `t3` processes each tool started. RSS is the largest of three `ps` samples taken while the watchers were blocked. `t3` calls were counted by a wrapper around the real CLI." % r["interval"])
    w("")
    w("| Watchers | Tool | `t3 thread show` calls | CPU time | Total RSS | RSS per waiting process |")
    w("| --- | --- | --- | --- | --- | --- |")
    for n in sorted(rust, key=int):
        ru, pv = rust[n], py[n]
        w("| %s | t3-watch (clients + daemon) | %d | %.2f s | %s | client %s, daemon %s |" % (
            n, ru["t3Calls"], ru["cpuSeconds"], mb_kb(ru["totalRssKb"]), mb_kb(ru["clientRssKb"]["median"]), mb_kb(ru["daemonRssKb"])))
        w("| %s | Python watcher | %d | %.2f s | %s | %s |" % (
            n, pv["t3Calls"], pv["cpuSeconds"], mb_kb(pv["totalRssKb"]), mb_kb(pv["watcherRssKb"]["median"])))
    w("")
    biggest = max(rust, key=int)
    ru = rust[biggest]
    w("Each of the %s t3-watch clients reported %.2f s of CPU for the whole window, which is below the 10 ms resolution of `/usr/bin/time`. All of t3-watch's CPU time is the daemon and the `t3` processes it ran. The watched thread was still `%s` at the end, and every watcher timed out rather than settling, so both tools did the same job." % (
        biggest, ru["cpuSecondsClientsOnly"], r["threadStatusAfter"]))
    print("\n".join(out))


if __name__ == "__main__":
    main()
