"""Gate X3b on a real `codex app-server`: approval requests that open and
close while no t3-watch connection counts are recovered from the hook log
that `t3-watch hook codex` keeps as Codex's PermissionRequest hook.

The hook is installed in the fixture's own CODEX_HOME only, the way a user
installs it: the block `t3-watch hook codex --print-settings` prints goes
into config.toml, and the hook is trusted by its key and hash as
`hooks/list` lists them (what /hooks records when the user trusts it). The
binary is run from a copy whose path has a space, so the printed command
is quoted for the shell Codex runs it with.
Client A owns each disposable thread and declines every request; t3-watch
answers none. The mock provider asks for two escalated commands per turn
(MOCK_CALLS=2).

  untrusted: before it is trusted, the hook does not run.
  live:   a wait counts throughout; nothing is recovered.
  missed: no wait and no daemon while both requests open and close; the
          next wait recovers both, announced once as progress.
  daemon: the daemon counted request 1, then was stopped; request 2 opens
          and closes; the next wait through a new daemon recovers request 2
          only.

usage: probe_missed.py <fixture dir> <t3-watch binary>
"""
import json, os, shutil, subprocess, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probe_client as pc

X = sys.argv[1]
os.makedirs(X + "/bin dir", exist_ok=True)
BIN = shutil.copy2(sys.argv[2], X + "/bin dir/t3-watch")
SOCK, WH = X + "/cs.sock", X + "/wh"
ENV = dict(os.environ, T3_WATCH_HOME=WH, T3_WATCH_CODEX_SOCKET=SOCK)
def report(k, v): print(json.dumps({k: v}), flush=True)

# ---- install and trust the hook in the fixture's CODEX_HOME ----------------
block = subprocess.run([BIN, "hook", "codex", "--print-settings"], env=ENV, capture_output=True, text=True, check=True).stdout
with open(X + "/home/config.toml", "a") as f:
    f.write("\n" + block)
A = pc.Client(SOCK, "A")
listed = [h for e in A.call("hooks/list", {"cwds": [X + "/proj"]})["result"]["data"] for h in e["hooks"]
          if h["eventName"] == "permissionRequest" and "hook codex" in h["command"]]
assert len(listed) == 1, listed
report("hook listed", {"trustStatus": listed[0]["trustStatus"], "quoted": listed[0]["command"].startswith("'")})

def start_turn():
    tid = A.call("thread/start", {"cwd": X + "/proj", "approvalPolicy": "on-request", "sandbox": "read-only"})["result"]["thread"]["id"]
    A.call("turn/start", {"threadId": tid, "input": [{"type": "text", "text": "two checks"}]})
    return tid, "codex://threads/" + tid

def request(tid, after=None):
    return A.until(lambda m: "method" in m and "id" in m and "requestApproval" in m["method"]
                   and m["params"]["threadId"] == tid and (after is None or m["id"] != after["id"]), 30)

def decline(r):
    A.w.send({"jsonrpc": "2.0", "id": r["id"], "result": {"decision": "decline"}})

def records(tid):
    try:
        return sum(1 for _ in open(WH + "/native/codex/%s.requests.jsonl" % tid))
    except FileNotFoundError:
        return 0

# ---- untrusted: Codex does not run it ---------------------------------------
tid, target = start_turn()
r1 = request(tid); decline(r1)
r2 = request(tid, r1); decline(r2)
A.until(lambda m: m.get("method") == "turn/completed" and m["params"].get("threadId") == tid, 30)
time.sleep(1.0)
report("untrusted: hook records for two requests", records(tid))

with open(X + "/home/config.toml", "a") as f:
    f.write('\n[hooks.state."%s"]\ntrusted_hash = "%s"\n' % (listed[0]["key"], listed[0]["currentHash"]))
h = [h for e in A.call("hooks/list", {"cwds": [X + "/proj"]})["result"]["data"] for h in e["hooks"]
     if h["eventName"] == "permissionRequest"][0]
report("hook installed", {"trustStatus": h["trustStatus"], "async": h.get("async"), "enabled": h["enabled"]})

def wait(*args, daemon=False):
    cmd = [BIN, "wait", *([] if daemon else ["--no-daemon"]), *args]
    p = subprocess.run(cmd, env=ENV, capture_output=True, text=True, timeout=90)
    v = json.loads(p.stdout or "{}")
    t = (v.get("threads") or [{}])[0]
    progress = [json.loads(l.split("t3-watch: progress ", 1)[1]) for l in p.stderr.splitlines() if l.startswith("t3-watch: progress ")]
    return {"exit": p.returncode, "outcome": v.get("outcome"), "via": v.get("via"),
            "pendingKey": t.get("pendingKey"), "ids": t.get("pendingRequestIds"),
            "recovered": [r["id"].rsplit("-", 2)[-2] + "-" + r["id"].rsplit("-", 1)[-1] for r in t.get("recoveredRequests") or []],
            "requestRecovery": t.get("requestRecovery"),
            "progress": [(e["event"], [r["id"].rsplit("-", 1)[-1] for r in e.get("recovered", [])]) for e in progress]}

# ---- live: a wait counts through both requests ------------------------------
tid, target = start_turn()
r1 = request(tid)
w1 = wait("--timeout", "5s", target)
report("live: request 1", w1)
bg = subprocess.Popen([BIN, "wait", "--no-daemon", "--timeout", "40s", "--acknowledge", w1["pendingKey"], target],
                      env=ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
time.sleep(1.5)
decline(r1)
r2 = request(tid, r1)
out, err = bg.communicate(timeout=60)
v = json.loads(out or "{}"); t = (v.get("threads") or [{}])[0]
report("live: request 2, same wait", {"exit": bg.returncode, "ids": t.get("pendingRequestIds"),
       "recovered": len(t.get("recoveredRequests") or []), "progressLines": err.count("t3-watch: progress ")})
decline(r2)
A.until(lambda m: m.get("method") == "turn/completed" and m["params"].get("threadId") == tid, 30)
report("live: turn end", dict(wait("--timeout", "10s", target), hookRecords=records(tid)))

# ---- missed: nothing counts while both requests open and close --------------
tid, target = start_turn()
r1 = request(tid); time.sleep(0.5); decline(r1)
r2 = request(tid, r1); time.sleep(0.5); decline(r2)
A.until(lambda m: m.get("method") == "turn/completed" and m["params"].get("threadId") == tid, 30)
time.sleep(0.5)
report("missed: records while nothing counted", records(tid))
report("missed: the next wait", wait("--timeout", "10s", target))
report("missed: a later wait", wait("--timeout", "10s", target))

# ---- daemon: counted request 1, stopped, request 2 missed -------------------
tid, target = start_turn()
r1 = request(tid)
d1 = wait("--timeout", "5s", target, daemon=True)
report("daemon: request 1", d1)
st = json.loads(subprocess.run([BIN, "status"], env=ENV, capture_output=True, text=True).stdout or "{}")
subprocess.run([BIN, "stop"], env=ENV, capture_output=True)
report("daemon: stopped", {"codexKeptBefore": st.get("codexKept")})
decline(r1)
r2 = request(tid, r1); time.sleep(0.5); decline(r2)
A.until(lambda m: m.get("method") == "turn/completed" and m["params"].get("threadId") == tid, 30)
time.sleep(0.5)
report("daemon: the next wait, new daemon", dict(wait("--timeout", "10s", target, daemon=True), hookRecords=records(tid)))
subprocess.run([BIN, "stop"], env=ENV, capture_output=True)

# ---- what the hook log holds -------------------------------------------------
keys = set()
for name in os.listdir(WH + "/native/codex"):
    if name.endswith(".requests.jsonl"):
        for line in open(WH + "/native/codex/" + name):
            keys |= set(json.loads(line))
report("hook log fields", sorted(keys))
report("A saw serverRequest/resolved", sum(1 for m in A.seen if m.get("method") == "serverRequest/resolved"))
A.close()
