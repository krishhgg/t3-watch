"""Mock Responses API for isolated Codex tests: no model, no network.

First request of a turn: if a shell-like tool is offered and MOCK_TOOL_CALL
is set, ask to run that command; otherwise (or after a tool result) answer
with a short message. Logs each request's path, tool names and whether it
carried a tool result (never the prompt text) to the log file.
"""
import http.server, json, os, sys, time

PORT, LOG = int(sys.argv[1]), sys.argv[2]
CMD = os.environ.get("MOCK_TOOL_CALL")
DELAY = float(os.environ.get("MOCK_DELAY", "0"))

def sse(w, obj):
    w.write(("event: %s\ndata: %s\n\n" % (obj["type"], json.dumps(obj))).encode())
    w.flush()

class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a):
        pass
    def do_GET(self):
        body = json.dumps({"object": "list", "data": [{"id": "mock-model", "object": "model"}]}).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def do_POST(self):
        req = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")
        tools = [t.get("name") or t.get("type") for t in req.get("tools", [])]
        if os.environ.get("MOCK_DUMP_TOOLS"):
            with open(os.environ["MOCK_DUMP_TOOLS"], "w") as f:
                json.dump({t.get("name"): (t.get("parameters") or {}).get("properties", {}) for t in req.get("tools", []) if t.get("name") in ("exec_command", "request_user_input")}, f)
        results = sum(1 for i in req.get("input", []) if isinstance(i, dict) and i.get("type") in ("function_call_output", "custom_tool_call_output"))
        has_result = results >= int(os.environ.get("MOCK_CALLS", "1"))
        with open(LOG, "a") as f:
            f.write(json.dumps({"t": round(time.time(), 3), "path": self.path, "tools": tools, "toolResult": has_result}) + "\n")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        rid = "resp_%d" % int(time.time() * 1000)
        sse(self.wfile, {"type": "response.created", "response": {"id": rid, "object": "response", "status": "in_progress", "model": "mock-model", "output": []}})
        if DELAY:
            time.sleep(DELAY)
        shell = next((t for t in tools if t in ("shell", "exec_command", "shell_command", "local_shell")), None)
        extra = json.loads(os.environ.get("MOCK_TOOL_ARGS", "{}"))
        use = os.environ.get("MOCK_TOOL", shell)
        if CMD and use and not has_result:
            args = dict({"cmd": CMD} if use == "exec_command" else {}, **extra)
            shell = use
            item = {"id": "fc_1", "type": "function_call", "status": "completed", "name": shell, "call_id": "call_%s_%d" % (rid, results), "arguments": json.dumps(args)}
        else:
            item = {"id": "msg_1", "type": "message", "status": "completed", "role": "assistant", "content": [{"type": "output_text", "text": "OK", "annotations": []}]}
        sse(self.wfile, {"type": "response.output_item.added", "output_index": 0, "item": dict(item, status="in_progress")})
        sse(self.wfile, {"type": "response.output_item.done", "output_index": 0, "item": item})
        usage = {"input_tokens": 1, "input_tokens_details": {"cached_tokens": 0}, "output_tokens": 1, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 2}
        sse(self.wfile, {"type": "response.completed", "response": {"id": rid, "object": "response", "status": "completed", "model": "mock-model", "output": [item], "usage": usage}})

http.server.ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
