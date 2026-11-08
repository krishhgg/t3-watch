"""Two approval requests in one turn on a real `codex app-server` (mock
provider with MOCK_CALLS=2; MOCK_DELAY sets the model's pause before the
second). A wait that acknowledged the first runs while the owner (client A)
declines it; it must wake for the second under its own pendingKey. The
status notifications every connection gets show the second request even
with no pause. Then the same through the daemon, with the second request
opening between two waits. t3-watch answers neither.

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

# The bot pattern through the daemon: the wait that reported request 1 has
# ended when the owner answers it and request 2 opens; the bot then waits
# again, acknowledging request 1.
def watchd(*args):
    p = subprocess.run([BIN, "wait", *args], env=ENV, capture_output=True, text=True, timeout=60)
    v = json.loads(p.stdout or "{}")
    t = (v.get("threads") or [{}])[0]
    return p.returncode, t.get("pendingKey"), t.get("pendingRequestIds"), v.get("via")
tid2 = A.call("thread/start", {"cwd": X + "/proj", "approvalPolicy": "on-request", "sandbox": "read-only"})["result"]["thread"]["id"]
target2 = "codex://threads/" + tid2
A.call("turn/start", {"threadId": tid2, "input": [{"type": "text", "text": "two checks"}]})
q1 = A.until(lambda m: "method" in m and "id" in m and "requestApproval" in m["method"] and m["params"]["threadId"] == tid2, 30)
code, j1, ids, via = watchd("--timeout", "5s", target2)
report("daemon: request 1", {"exit": code, "pendingKey": j1, "ids": ids, "via": via})
A.w.send({"jsonrpc": "2.0", "id": q1["id"], "result": {"decision": "decline"}})
q2 = A.until(lambda m: "method" in m and "id" in m and "requestApproval" in m["method"] and m["params"]["threadId"] == tid2 and m["id"] != q1["id"], 30)
time.sleep(0.5)
code, j2, ids, via = watchd("--timeout", "10s", "--acknowledge", j1, target2)
report("daemon: request 2, opened between the waits", {"exit": code, "pendingKey": j2, "ids": ids, "via": via,
       "distinct": j2 is not None and j2 != j1})
A.w.send({"jsonrpc": "2.0", "id": q2["id"], "result": {"decision": "decline"}})
A.until(lambda m: m.get("method") == "turn/completed" and m["params"].get("threadId") == tid2, 30)
A.close()
