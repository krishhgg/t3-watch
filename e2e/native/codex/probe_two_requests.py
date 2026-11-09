"""Two approval requests in one turn on a real `codex app-server` (mock
provider with MOCK_CALLS=2; MOCK_DELAY sets the model's pause before the
second). A wait that acknowledged the first runs while the owner (client A)
declines it; it must wake for the second under its own pendingKey. The
status notifications every connection gets show the second request even
with no pause. Then the same through the daemon, with the second request
opening between two waits, and once more with drain rounds stretched so
that a wait starts inside one. Client A, the owner of these disposable
threads, declines every request; t3-watch answers none.

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

# The keeper's drain path: a separate daemon whose drain rounds read each
# kept connection for 3 s, back to back (T3_WATCH_TEST_CODEX_DRAIN_MS), so
# the second wait starts inside a round. It must take the kept connection
# (and its counter), and request 2, opened while no wait runs, must reach
# the third wait under its own key.
ENV3 = dict(ENV, T3_WATCH_HOME=X + "/w3", T3_WATCH_TEST_CODEX_DRAIN_MS="3000")
def watch3(*args):
    p = subprocess.run([BIN, "wait", *args], env=ENV3, capture_output=True, text=True, timeout=60)
    v = json.loads(p.stdout or "{}")
    t = (v.get("threads") or [{}])[0]
    return p.returncode, t.get("pendingKey"), t.get("pendingRequestIds")
tid3 = A.call("thread/start", {"cwd": X + "/proj", "approvalPolicy": "on-request", "sandbox": "read-only"})["result"]["thread"]["id"]
target3 = "codex://threads/" + tid3
A.call("turn/start", {"threadId": tid3, "input": [{"type": "text", "text": "two checks"}]})
p1 = A.until(lambda m: "method" in m and "id" in m and "requestApproval" in m["method"] and m["params"]["threadId"] == tid3, 30)
code, k1, ids = watch3("--timeout", "5s", target3)
report("keeper: request 1", {"exit": code, "ids": ids})
code, k, ids = watch3("--timeout", "8s", "--acknowledge", k1, target3)
st = json.loads(subprocess.run([BIN, "status"], env=ENV3, capture_output=True, text=True).stdout or "{}")
report("keeper: a wait during a drain round, same request", {"exit": code, "ids": ids, "codexKept": st.get("codexKept")})
A.w.send({"jsonrpc": "2.0", "id": p1["id"], "result": {"decision": "decline"}})
p2 = A.until(lambda m: "method" in m and "id" in m and "requestApproval" in m["method"] and m["params"]["threadId"] == tid3 and m["id"] != p1["id"], 30)
time.sleep(4.0)
code, k2, ids = watch3("--timeout", "10s", "--acknowledge", k1, target3)
report("keeper: request 2, opened between waits", {"exit": code, "ids": ids, "distinct": k2 is not None and k2 != k1})
A.w.send({"jsonrpc": "2.0", "id": p2["id"], "result": {"decision": "decline"}})
A.until(lambda m: m.get("method") == "turn/completed" and m["params"].get("threadId") == tid3, 30)
subprocess.run([BIN, "stop"], env=ENV3, capture_output=True)
A.close()
