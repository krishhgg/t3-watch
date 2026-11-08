"""Minimal WebSocket client over a Unix socket (stdlib only), for talking
to `codex app-server --listen unix://...` in tests."""
import base64, hashlib, json, os, socket, struct

class WS:
    def __init__(self, path, timeout=10):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.settimeout(timeout)
        self.s.connect(path)
        key = base64.b64encode(os.urandom(16)).decode()
        self.s.sendall(("GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                        "Sec-WebSocket-Key: %s\r\nSec-WebSocket-Version: 13\r\n\r\n" % key).encode())
        head = b""
        while b"\r\n\r\n" not in head:
            c = self.s.recv(1)
            if not c: raise IOError("closed during handshake")
            head += c
        status = head.split(b"\r\n")[0]
        want = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
        if b" 101 " not in status or want.encode() not in head:
            raise IOError("bad handshake: %r" % head[:200])
        self.buf = b""
    def send(self, obj):
        data = json.dumps(obj).encode()
        mask = os.urandom(4)
        n = len(data)
        hdr = bytes([0x81]) + (bytes([0x80 | n]) if n < 126 else bytes([0x80 | 126]) + struct.pack(">H", n) if n < 65536 else bytes([0x80 | 127]) + struct.pack(">Q", n))
        self.s.sendall(hdr + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))
    def _need(self, n):
        while len(self.buf) < n:
            c = self.s.recv(65536)
            if not c: raise IOError("closed")
            self.buf += c
        out, self.buf = self.buf[:n], self.buf[n:]
        return out
    def recv(self):
        msg = b""
        while True:
            b0, b1 = self._need(2)
            op, n = b0 & 0x0F, b1 & 0x7F
            if n == 126: n = struct.unpack(">H", self._need(2))[0]
            elif n == 127: n = struct.unpack(">Q", self._need(8))[0]
            mask = self._need(4) if b1 & 0x80 else None
            payload = self._need(n)
            if mask: payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
            if op == 0x9:  # ping
                self.s.sendall(bytes([0x8A, 0x80 | len(payload)]) + b"\0\0\0\0" + payload); continue
            if op == 0x8: raise IOError("server closed")
            if op in (0x1, 0x0):
                msg += payload
                if b0 & 0x80: return json.loads(msg)
