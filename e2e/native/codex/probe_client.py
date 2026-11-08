"""A JSON-RPC client over ws.py for probe scripts: the owner of a disposable
thread. Records every message with its arrival time."""
import queue, threading, time
from ws import WS

class Client:
    def __init__(self, sock, name):
        self.q, self.n, self.seen = queue.Queue(), 0, []
        self.w = WS(sock, timeout=None)
        threading.Thread(target=self._read, daemon=True).start()
        r = self.call("initialize", {"clientInfo": {"name": "probe-" + name, "version": "0"}, "capabilities": {"experimentalApi": True}})
        assert "result" in r, r
        self.w.send({"jsonrpc": "2.0", "method": "initialized"})
    def _read(self):
        try:
            while True:
                m = self.w.recv(); m["_at"] = time.time(); self.seen.append(m); self.q.put(m)
        except Exception:
            pass
    def call(self, method, params, secs=30):
        self.n += 1; i = self.n
        self.w.send({"jsonrpc": "2.0", "id": i, "method": method, "params": params})
        end = time.time() + secs
        while time.time() < end:
            try: m = self.q.get(timeout=0.2)
            except queue.Empty: continue
            if m.get("id") == i and "method" not in m: return m
        return {"error": "timeout"}
    def until(self, pred, secs):
        end = time.time() + secs
        for m in list(self.seen):
            if pred(m): return m
        while time.time() < end:
            try: m = self.q.get(timeout=0.2)
            except queue.Empty: continue
            if pred(m): return m
        return None
    def close(self):
        try: self.w.s.close()
        except Exception: pass
