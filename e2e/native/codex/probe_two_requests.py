"""Two approval requests in one turn on a real `codex app-server` (mock
provider with MOCK_CALLS=2; MOCK_DELAY sets the model's pause before the
second). A wait that acknowledged the first runs while the owner (client A)
declines it; it must wake for the second under its own pendingKey. With no
pause the flag never goes off between polls and the second is missed until
the turn ends (docs/native-sessions.md, "Limits"). t3-watch answers neither.

usage: probe_two_requests.py <fixture dir> <t3-watch binary>
"""
import json, os, subprocess, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probe_client as pc

X, BIN = sys.argv[1], sys.argv[2]
SOCK = X + "/cs.sock"
ENV = dict(os.environ, T3_WATCH_HOME=X + "/w2", T3_WATCH_CODEX_SOCKET=SOCK)
def report(k, v): print(json.dumps({k: v}), flush=True)
def watch(*args):
    p = subprocess.run([BIN, "wait", "--no-daemon", *args], env=ENV, capture_output=True, text=True, timeout=60)
    v = json.loads(p.stdout or "{}")
    t = (v.get("threads") or [{}])[0]
    return p.returncode, t.get("pendingKey"), t.get("pendingRequestIds"), t.get("runOrdinal")

A = pc.Client(SOCK, "A")
tid = A.call("thread/start", {"cwd": X + "/proj", "approvalPolicy": "on-request", "sandbox": "read-only"})["result"]["thread"]["id"]
target = "codex://threads/" + tid
report("thread", tid)
A.call("turn/start", {"threadId": tid, "input": [{"type": "text", "text": "two checks"}]})
r1 = A.until(lambda m: "method" in m and "id" in m and "requestApproval" in m["method"], 30)
code, k1, ids1, _ = watch("--timeout", "5s", target)
report("request 1", {"exit": code, "pendingKey": k1, "ids": ids1})
code, k, ids, _ = watch("--timeout", "3s", "--acknowledge", k1, target)
report("request 1 acknowledged", {"exit": code, "pendingKey": k, "ids": ids})
# A bot's watcher waits past request 1 while the owner answers it.
bg = subprocess.Popen([BIN, "wait", "--no-daemon", "--timeout", "40s", "--acknowledge", k1, target],
                      env=ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
time.sleep(1.5)
A.w.send({"jsonrpc": "2.0", "id": r1["id"], "result": {"decision": "decline"}})
r2 = A.until(lambda m: "method" in m and "id" in m and "requestApproval" in m["method"] and m["id"] != r1["id"], 30)
report("A got request 2", r2 is not None)
out, _ = bg.communicate(timeout=60); t_exit = time.time()
v = json.loads(out or "{}"); t = (v.get("threads") or [{}])[0]
k2 = t.get("pendingKey")
report("request 2, watcher running since request 1", {"exit": bg.returncode, "outcome": v.get("outcome"),
       "pendingKey": k2, "ids": t.get("pendingRequestIds"), "distinct": k2 is not None and k2 != k1,
       "afterRequestMs": r2 and int((t_exit - r2["_at"]) * 1000)})
A.w.send({"jsonrpc": "2.0", "id": r2["id"], "result": {"decision": "decline"}})
done = A.until(lambda m: m.get("method") == "turn/completed", 30)
code, k, ids, n = watch("--timeout", "10s", target)
report("turn end", {"turn": done and done["params"]["turn"]["status"], "exit": code, "runOrdinal": n})
report("A saw serverRequest/resolved for both", sum(1 for m in A.seen if m.get("method") == "serverRequest/resolved"))
A.close()
