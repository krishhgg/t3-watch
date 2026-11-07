#!/usr/bin/env python3
"""Audit fixture for e2e/live.sh, not a production receiver.

A controlled webhook receiver on 127.0.0.1 only. It verifies the
X-T3-Watch-Signature HMAC with the secret in SECRET_FILE and the expected
Authorization header, appends one JSON line per request to LOG_FILE (accepted
or not), and answers 200 only when both check out; otherwise 401, so a
misconfigured sender fails here as it would against a real receiver. Writes
its port to PORT_FILE. Never prints secrets.

What a pass here shows: t3-watch sent a correctly signed, authorized event to
this fixture. It is not evidence that a production receiver (a bot's webhook
routine) received the event or acted on it; that is checked separately at the
receiver.

Usage: receiver.py SECRET_FILE EXPECTED_AUTH LOG_FILE PORT_FILE [PORT]
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
            "idempotencyOk": self.headers.get("Idempotency-Key") == event_id,
            "attempt": self.headers.get("X-T3-Watch-Attempt"),
            "outcome": payload.get("outcome"),
            "bodyEvent": payload.get("event"),
            "threadStatus": next(iter(t.get("status") for t in payload.get("threads") or [] if isinstance(t, dict)), None),
            "bytes": len(body),
        }
        accepted = entry["signatureOk"] and entry["authorizationOk"] and entry["idempotencyOk"]
        entry["accepted"] = accepted
        with open(LOG, "a") as f:
            f.write(json.dumps(entry) + "\n")
        reply = b"ok" if accepted else b"rejected"
        self.send_response(200 if accepted else 401)
        self.send_header("Content-Length", str(len(reply)))
        self.end_headers()
        self.wfile.write(reply)

    def log_message(self, *args):
        pass


server = http.server.HTTPServer(("127.0.0.1", int(sys.argv[5]) if len(sys.argv) > 5 else 0), Handler)
with open(PORT_FILE, "w") as f:
    f.write(str(server.server_address[1]))
server.serve_forever()
