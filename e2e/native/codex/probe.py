"""t3-watch against a real `codex app-server` (isolated CODEX_HOME, mock
model provider: no OpenAI request, no spend). Client A owns a disposable
thread and gets its approval request; t3-watch watches it read-only. Prints
one JSON line per finding. Run it through run.sh, which sets up the fixture.

usage: probe.py <fixture dir> <t3-watch binary>
"""
import hashlib, http.server, json, os, signal, subprocess, sys, threading, time

X, BIN = sys.argv[1], sys.argv[2]
HOME, SOCK, WHOME = X + "/home", X + "/cs.sock", X + "/w"
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probe_client

def report(k, v): print(json.dumps({k: v}), flush=True)
def now(): return time.time()

# ---- webhook receiver: arrival time, event header, a few body fields -------
hooks = []
class R(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")
        hooks.append({"at": now(), "event": self.headers.get("X-T3-Watch-Event"),
                      "thread": [t.get("threadId") for t in body.get("threads", [])],
                      "outcome": body.get("outcome"), "keys": sorted(body)[:12]})
        self.send_response(200); self.send_header("Content-Length", "0"); self.end_headers()
rx = http.server.ThreadingHTTPServer(("127.0.0.1", 0), R)
threading.Thread(target=rx.serve_forever, daemon=True).start()
HOOK = "http://127.0.0.1:%d/hook" % rx.server_address[1]

def Client(name):
    return probe_client.Client(SOCK, name)

def files():
    out = {}
    for root, _, names in os.walk(HOME):
        if "/.tmp" in root: continue
        for n in names:
            p = os.path.join(root, n)
            try: out[os.path.relpath(p, HOME)] = hashlib.sha256(open(p, "rb").read()).hexdigest()[:16]
            except Exception: pass
    return out
def changed(a, b): return sorted(k for k in set(a) | set(b) if a.get(k) != b.get(k))

ENV = dict(os.environ, T3_WATCH_HOME=WHOME, T3_WATCH_CODEX_SOCKET=SOCK, T3_WATCH_IDLE_EXIT="5s")
for k in ("T3_WATCH_SOCKET", "T3_WATCH_NO_DAEMON"): ENV.pop(k, None)
def watch(*args):
    return subprocess.Popen([BIN, "wait", *args], env=ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
def done(p, secs):
    out, err = p.communicate(timeout=secs)
    try: v = json.loads(out)
    except Exception: v = {"raw": out[:300]}
    return p.returncode, v, err.strip()[:300]
def brief(v):
    t = (v.get("threads") or [{}])[0]
    return {"outcome": v.get("outcome"), "status": t.get("status"), "runOrdinal": t.get("runOrdinal"),
            "pendingKey": t.get("pendingKey"), "via": v.get("via"), "lastError": t.get("lastError")}

A = Client("A")
r = A.call("thread/start", {"cwd": X + "/proj", "approvalPolicy": "on-request", "sandbox": "read-only"})
tid = r["result"]["thread"]["id"]; target = "codex://threads/" + tid
report("thread", tid)

# 1. A fresh thread: no turn yet, the wait keeps waiting.
code, v, err = done(watch("--no-daemon", "--timeout", "3s", target), 20)
report("1 no turn yet", {"exit": code, **brief(v)})

# 2. needs input: registered before the turn starts, through the daemon,
#    with a webhook. A holds the approval request and never answers it
#    until the end of step 3.
w = watch("--timeout", "60s", "--webhook-url", HOOK, target)
time.sleep(2.0)
t_turn = now()
A.call("turn/start", {"threadId": tid, "input": [{"type": "text", "text": "run the check"}]})
req = A.until(lambda m: "method" in m and "id" in m and "requestApproval" in m["method"], 30)
t_req = req["_at"] if req else None
code, v, err = done(w, 60)
t_exit = now()
report("2 needs input", {"exit": code, **brief(v), "approvalRequest": req and req["method"],
       "exitAfterRequestMs": t_req and int((t_exit - t_req) * 1000), "err": err})
hook = next((h for h in hooks if h["event"] == "needs_input"), None)
report("2 webhook", hook and {**hook, "afterRequestMs": int((hook["at"] - t_req) * 1000)})
key = brief(v)["pendingKey"]

# 3. Non-mutation: the same 8 s with and without t3-watch polling, while
#    A's request stays open and nothing else runs.
time.sleep(1.0)
f0 = files(); time.sleep(8.0); f1 = files()
report("3 control window (no watcher): files changed", changed(f0, f1))
w = watch("--no-daemon", "--timeout", "8s", "--acknowledge", key, target)
code, v, err = done(w, 30)
f2 = files()
report("3 watcher window (acknowledged, polling): files changed", changed(f1, f2))
report("3 acknowledged wait", {"exit": code, **brief(v)})
report("3 A still owns its request (no serverRequest/resolved yet)",
       not any(m.get("method") == "serverRequest/resolved" for m in A.seen))

# 4. The owner declines; the turn completes.
A.w.send({"jsonrpc": "2.0", "id": req["id"], "result": {"decision": "decline"}})
c1 = A.until(lambda m: m.get("method") == "turn/completed", 30)
report("4 A turn/completed", c1 and c1["params"]["turn"]["status"])
code, v, err = done(watch("--no-daemon", "--timeout", "10s", target), 30)
report("4 wait after the turn", {"exit": code, **brief(v)})

# 5. A pinned wake for the next turn (a bot reusing the thread): the wait
#    is registered first, A starts turn 2, the webhook says settled.
n = len(hooks)
w = watch("--timeout", "60s", "--expect-min-run-ordinal", "2", "--webhook-url", HOOK, target)
time.sleep(2.0)
A.call("turn/start", {"threadId": tid, "input": [{"type": "text", "text": "again"}]})
c2 = A.until(lambda m: m.get("method") == "turn/completed" and m is not c1, 30)
code, v, err = done(w, 60)
t_exit = now()
report("5 pinned wait", {"exit": code, **brief(v), "turn": c2 and c2["params"]["turn"]["status"],
       "exitAfterTurnCompletedMs": c2 and int((t_exit - c2["_at"]) * 1000), "err": err})
hook = next((h for h in hooks[n:] if h["event"] == "settled"), None)
report("5 webhook", hook and {**hook, "afterTurnCompletedMs": int((hook["at"] - c2["_at"]) * 1000)})

# 6. Unload: A leaves. t3-watch keeps polling; the thread must still unload
#    (no subscription of t3-watch keeps it loaded).
A.call("thread/unsubscribe", {"threadId": tid}); A.close()
t_left = now()
w = watch("--no-daemon", "--timeout", "150s", "--expect-min-run-ordinal", "3", target)
D = Client("D")
unloaded = None
while now() - t_left < 120:
    loaded = (D.call("thread/loaded/list", {}).get("result") or {}).get("data") or []
    if tid not in loaded:
        unloaded = int(now() - t_left); break
    time.sleep(5)
st = (D.call("thread/read", {"threadId": tid}).get("result") or {}).get("thread", {}).get("status")
report("6 unloaded while t3-watch polled", {"afterSeconds": unloaded, "status": st})
w.send_signal(signal.SIGTERM)
code, v, err = done(w, 20)
report("6 watcher stopped", {"exit": code})

# 7. A turn finished while no client had the thread loaded is still read:
#    D reads it as notLoaded with both turns completed.
code, v, err = done(watch("--no-daemon", "--timeout", "5s", "--expect-min-run-ordinal", "2", target), 30)
report("7 read of an unloaded thread", {"exit": code, **brief(v)})
D.close()
report("hooks", [{k: h[k] for k in ("event", "thread", "outcome")} for h in hooks])
