"""A webhook receiver whose behavior a file controls, for durable-delivery
gates. Mode file contents: "ok" (200), "hang" (record, then hold the
request 600 s without answering). Records each request's arrival time,
X-T3-Watch-Id, -Attempt and -Event as one JSON line; never the body.

usage: recvctl.py <port> <log> <mode file>
"""
import http.server, json, sys, time

PORT, LOG, MODE = int(sys.argv[1]), sys.argv[2], sys.argv[3]

class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length") or 0))
        mode = open(MODE).read().strip()
        with open(LOG, "a") as f:
            f.write(json.dumps({"t": round(time.time(), 3), "mode": mode,
                                "id": self.headers.get("X-T3-Watch-Id"),
                                "attempt": self.headers.get("X-T3-Watch-Attempt"),
                                "event": self.headers.get("X-T3-Watch-Event")}) + "\n")
        if mode == "hang":
            time.sleep(600)
            return
        self.send_response(200); self.send_header("Content-Length", "0"); self.end_headers()

http.server.ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
