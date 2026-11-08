"""A disposable interactive Claude Code session on a pseudo-terminal,
watched by t3-watch through its async hook (docs/native-sessions.md,
Claude Code acceptance checks 3 and 4).

1. A wait is registered first (daemon, webhook to a local receiver).
2. Claude Code starts in <work dir>/proj with only t3-watch's hooks
   (--setting-sources local --settings ... --strict-mcp-config), model
   haiku, tool Bash, permission mode manual, and a prompt asking for one
   Bash command outside the project, so a real permission dialog appears.
   The folder-trust dialog for the new project is answered "yes".
3. t3-watch must report needs_input (exit 5, webhook). The operator (this
   script) then rejects the dialog with Escape. It never approves anything;
   the command must not run.
4. Same-session reuse: a wait pinned to the next run, acknowledging the
   relayed request, then a second prompt typed into the live session.
5. --resume reuse: a print-mode session, then a pinned wait and a
   `claude -p --resume` of it.

usage: e2e/native/claude_interactive.py <work dir under /tmp> <t3-watch binary> <session uuid>

Prints one JSON line per finding, never screen text. It spends a few small
haiku requests on the user's Claude Code login. Afterwards, remove the work
dir, ~/.claude/projects/-private-tmp-<work dir>-proj and
~/.claude/session-env/<session uuid> (and the print session's, the same uuid
with the last bit flipped). Claude Code also records the trust answer and
the prompts in its own files (~/.claude.json, ~/.claude/history.jsonl).
"""
import fcntl, http.server, json, os, pty, select, struct, subprocess, sys, termios, threading, time

W, BIN, SID = sys.argv[1], sys.argv[2], sys.argv[3]
PROJ, WHOME = W + "/proj", W + "/w"
os.makedirs(PROJ, exist_ok=True)
def report(k, v): print(json.dumps({k: v}), flush=True)
def now(): return time.time()

hooks = []
class R(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")
        hooks.append({"at": now(), "event": self.headers.get("X-T3-Watch-Event"),
                      "threads": [t.get("threadId") for t in body.get("threads", [])],
                      "runOrdinal": [t.get("runOrdinal") for t in body.get("threads", [])]})
        self.send_response(200); self.send_header("Content-Length", "0"); self.end_headers()
rx = http.server.ThreadingHTTPServer(("127.0.0.1", 0), R)
threading.Thread(target=rx.serve_forever, daemon=True).start()
HOOK = "http://127.0.0.1:%d/hook" % rx.server_address[1]

ENV = dict(os.environ, T3_WATCH_HOME=WHOME, T3_WATCH_IDLE_EXIT="5s", TERM="xterm-256color")
for k in ("T3_WATCH_SOCKET", "T3_WATCH_NO_DAEMON"): ENV.pop(k, None)
settings = subprocess.run([BIN, "hook", "claude-code", "--print-settings"], env=ENV, capture_output=True, text=True).stdout
open(W + "/hooks.json", "w").write(settings)
TARGET = "claude-code:" + SID
LOG = WHOME + "/native/claude-code/%s.jsonl" % SID
def events():
    try: return [json.loads(l) for l in open(LOG)]
    except FileNotFoundError: return []
def watch(*args):
    return subprocess.Popen([BIN, "wait", *args], env=ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
def done(p, secs):
    out, err = p.communicate(timeout=secs)
    try: v = json.loads(out)
    except Exception: v = {"raw": out[:200]}
    t = (v.get("threads") or [{}])[0]
    return {"exit": p.returncode, "outcome": v.get("outcome"), "status": t.get("status"),
            "runOrdinal": t.get("runOrdinal"), "pendingKey": t.get("pendingKey"), "via": v.get("via")}

COMMON = ["--setting-sources", "local", "--settings", W + "/hooks.json", "--strict-mcp-config", "--model", "haiku"]

# The wait is registered before the session exists (daemon, webhook).
w = watch("--timeout", "180s", "--webhook-url", HOOK, TARGET)
time.sleep(1.0)

pid, fd = pty.fork()
if pid == 0:
    os.chdir(PROJ)
    os.execvpe("claude", ["claude", *COMMON, "--tools", "Bash", "--permission-mode", "manual", "--session-id", SID,
                          "Use the Bash tool to run exactly this command and nothing else: touch " + W + "/outside-probe"], ENV)

fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
screen = b""
def pump(secs):
    global screen
    end = now() + secs
    while now() < end:
        r, _, _ = select.select([fd], [], [], 0.1)
        if r:
            try: screen = (screen + os.read(fd, 65536))[-20000:]
            except OSError: return
def send(b):
    try: os.write(fd, b)
    except OSError: pass

t0 = now()
trusted = False
needs = None
while now() - t0 < 120 and needs is None:
    pump(0.5)
    low = screen.lower()
    if not trusted and b"trust" in low and b"folder" in low:
        # The dialog's default is "No, exit": move to "Yes" first.
        time.sleep(0.5); send(b"\x1b[B"); time.sleep(0.3); send(b"\r"); trusted = True; screen = b""
        report("folder trust dialog", "answered for the disposable project")
    if w.poll() is not None:
        needs = done(w, 5); t_needs = now()
ev = events()
report("events before the answer", [e["event"] + ("/" + e["type"] if e.get("type") else "") for e in ev])
report("needs input", needs)
hook = next((h for h in hooks if h["event"] == "needs_input"), None)
pr = next((e for e in ev if e["event"] == "PermissionRequest"), None)
report("needs_input webhook", hook and {**hook, "tool": pr and pr.get("tool")})

# The operator denies: Escape on the dialog. Never approves.
time.sleep(1.0)
send(b"\x1b"); t_deny = now()
report("operator", "denied with Escape")
end = now() + 30
while now() < end and not any(e["event"] == "Stop" for e in events()):
    pump(0.5)
ev = events()
report("events after the denial", [e["event"] + ("/" + e["type"] if e.get("type") else "") for e in ev])
report("the command did not run", not os.path.exists(W + "/outside-probe"))
r = done(watch("--no-daemon", "--timeout", "5s", TARGET), 30)
report("wait after the denial", r)

# Reuse 1: a second prompt in the same live session. The bot reads the
# current runOrdinal, waits pinned to the next one, then submits.
cur = r["runOrdinal"] or 0
n = len(hooks)
# The request it already relayed is acknowledged: Escape fires no hook, so
# that request stays open until the next prompt.
ack = ["--acknowledge", r["pendingKey"]] if r.get("pendingKey") else []
w = watch("--timeout", "120s", "--expect-min-run-ordinal", str(cur + 1), *ack, "--webhook-url", HOOK, TARGET)
time.sleep(1.5)
t_submit = now()
send(b"Reply with the single word OK."); time.sleep(0.5); send(b"\r")
r = done(w, 120); t_woke = now()
report("same session, next prompt", {"pinnedTo": cur + 1, "acknowledged": bool(ack), **r, "wokeAfterSubmitMs": int((t_woke - t_submit) * 1000)})
hook = next((h for h in hooks[n:] if h["event"] == "settled"), None)
report("settled webhook", hook and {**hook, "afterSubmitMs": int((hook["at"] - t_submit) * 1000)})
report("events", [e["event"] + ("/" + e["type"] if e.get("type") else "") for e in events()])
send(b"/exit"); time.sleep(0.3); send(b"\r"); pump(4)
try: os.waitpid(pid, 0)
except ChildProcessError: pass
report("events after exit", [e["event"] + ("/" + e["type"] if e.get("type") else "") for e in events()][-2:])

# Reuse 2: a print-mode session, then --resume of it.
P = SID[:-12] + "%012x" % (int(SID[-12:], 16) ^ 1)
PT = "claude-code:" + P
PLOG = WHOME + "/native/claude-code/%s.jsonl" % P
p1 = subprocess.run(["claude", "-p", *COMMON, "--tools", "", "--session-id", P, "Reply with the single word OK."],
                    cwd=PROJ, env=ENV, capture_output=True, text=True, timeout=120)
cur = done(watch("--no-daemon", "--timeout", "5s", PT), 20)["runOrdinal"] or 0
report("print session", {"claudeExit": p1.returncode, "runOrdinal": cur})
n = len(hooks)
w = watch("--timeout", "120s", "--expect-min-run-ordinal", str(cur + 1), "--webhook-url", HOOK, PT)
time.sleep(1.5)
t_resume = now()
p2 = subprocess.run(["claude", "-p", *COMMON, "--tools", "", "--resume", P, "Reply with the single word OK."],
                    cwd=PROJ, env=ENV, capture_output=True, text=True, timeout=120)
t_printed = now()
try:
    r = done(w, 30)
except subprocess.TimeoutExpired:
    w.kill(); r = {"exit": None, "outcome": "no wake within 30 s after the resumed print exited"}
report("resumed print session", {"claudeExit": p2.returncode, "pinnedTo": cur + 1, **r,
                                 "resumeError": p2.returncode != 0 and len(p2.stderr) > 0})
try: pev = [json.loads(l) for l in open(PLOG)]
except FileNotFoundError: pev = []
report("print session events", [e["event"] + ("/" + e["type"] if e.get("type") else "") for e in pev])
hook = next((h for h in hooks[n:] if h["event"] == "settled"), None)
report("resume webhook", hook and {**hook, "afterResumeStartMs": int((hook["at"] - t_resume) * 1000)})
report("hooks", [{k: h[k] for k in ("event", "threads", "runOrdinal")} for h in hooks])
