#!/usr/bin/env python3
"""A controlled webhook receiver for e2e/live.sh. Listens on 127.0.0.1 only,
verifies the X-T3-Watch-Signature HMAC with the secret in SECRET_FILE, checks
an expected Authorization header, and appends one JSON line per request to
LOG_FILE. Writes its port to PORT_FILE. Never prints secrets.

Usage: receiver.py SECRET_FILE EXPECTED_AUTH LOG_FILE PORT_FILE
"""

import hashlib
import hmac
import http.server
import json
import sys

SECRET = open(sys.argv[1], "rb").read().strip()
EXPECTED_AUTH = sys.argv[2]
LOG = sys.argv[3]
PORT_FILE = sys.argv[4]


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        ts = self.headers.get("X-T3-Watch-Timestamp", "")
        event_id = self.headers.get("X-T3-Watch-Id", "")
        expected = "sha256=" + hmac.new(SECRET, f"{ts}.{event_id}.".encode() + body, hashlib.sha256).hexdigest()
        try:
            payload = json.loads(body)
        except ValueError:
            payload = {}
        entry = {
            "path": self.path,
            "event": self.headers.get("X-T3-Watch-Event"),
            "id": event_id,
            "signatureOk": hmac.compare_digest(expected, self.headers.get("X-T3-Watch-Signature", "")),
            "authorizationOk": self.headers.get("Authorization") == EXPECTED_AUTH,
            "bodyEvent": payload.get("event"),
            "threadStatus": (payload.get("threads") or [{}])[0].get("status"),
            "bytes": len(body),
        }
        with open(LOG, "a") as f:
            f.write(json.dumps(entry) + "\n")
        self.send_response(200)
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, *args):
        pass


server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
with open(PORT_FILE, "w") as f:
    f.write(str(server.server_address[1]))
server.serve_forever()
