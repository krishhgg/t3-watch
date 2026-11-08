"""A loopback relay between t3-watch and a real webhook receiver, for the
durable-replay gate: stopping this process is a controlled outage of the
watcher's path to the receiver only. No firewall rule, no network change,
nothing else on the host is affected.

t3-watch posts to http://127.0.0.1:<port>/hook. The relay forwards each
request unchanged (body, Content-Type, every X-T3-Watch-* header,
Idempotency-Key, Authorization) to the receiver named by the environment
variable you pass, and answers with the receiver's status. The signature
t3-watch computes covers the body, id and timestamp, which pass through
untouched, so the receiver verifies it as usual.

usage: T3W_RELAY_TARGET=<receiver URL> python3 -I e2e/relay.py <port> <log>

The target URL may carry a secret path: it is read from the environment,
never printed or logged. The log holds one JSON line per request: arrival
time, X-T3-Watch-Id, -Event, -Attempt, and the receiver's status (or the
error class), nothing else.
"""
import http.server, json, os, sys, time, urllib.error, urllib.request

PORT, LOG = int(sys.argv[1]), sys.argv[2]
TARGET = os.environ.get("T3W_RELAY_TARGET", "")
if not TARGET.startswith(("https://", "http://")):
    sys.exit("set T3W_RELAY_TARGET to the receiver URL")
PASS = ("content-type", "authorization", "idempotency-key")

class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a): pass
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        headers = {k: v for k, v in self.headers.items()
                   if k.lower() in PASS or k.lower().startswith("x-t3-watch-")}
        entry = {"t": round(time.time(), 3), "id": self.headers.get("X-T3-Watch-Id"),
                 "event": self.headers.get("X-T3-Watch-Event"), "attempt": self.headers.get("X-T3-Watch-Attempt")}
        status = 502
        try:
            req = urllib.request.Request(TARGET, data=body, headers=headers, method="POST")
            with urllib.request.urlopen(req, timeout=25) as r:
                status = r.status
        except urllib.error.HTTPError as e:
            status = e.code
        except Exception as e:
            entry["error"] = type(e).__name__
        entry["status"] = status
        with open(LOG, "a") as f:
            f.write(json.dumps(entry) + "\n")
        self.send_response(status)
        self.send_header("Content-Length", "0")
        self.end_headers()

http.server.ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
