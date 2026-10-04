#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Loopback JSON-RPC fixture. It accepts only the test key, never a real key."""
import base64
import hashlib
import json
import socket
import struct
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

scenario = sys.argv[1]
state = {"polls": 0, "starts": 0}
lock = threading.Lock()

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *args):
        pass

    def read_exact(self, n):
        data = b""
        while len(data) < n:
            part = self.rfile.read(n - len(data))
            if not part:
                raise EOFError()
            data += part
        return data

    def send_frame(self, data):
        size = len(data)
        header = bytes([0x81, size]) if size < 126 else bytes([0x81, 126]) + struct.pack("!H", size) if size <= 65535 else bytes([0x81, 127]) + struct.pack("!Q", size)
        self.wfile.write(header + data)
        self.wfile.flush()

    def reply(self, request, result=None, error=None):
        value = {"jsonrpc": "2.0", "id": request["id"]}
        value["error" if error else "result"] = error if error else result
        self.send_frame(json.dumps(value).encode())

    def do_GET(self):
        if self.path != "/api/current":
            self.send_error(404)
            return
        key = self.headers["Sec-WebSocket-Key"]
        accept = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
        self.send_response(101)
        self.send_header("Upgrade", "websocket")
        self.send_header("Connection", "Upgrade")
        self.send_header("Sec-WebSocket-Accept", accept)
        self.end_headers()
        self.close_connection = True
        try:
            authenticated = False
            while True:
                first, second = self.read_exact(2)
                if first & 15 == 8:
                    return
                size = second & 127
                if size == 126:
                    size = struct.unpack("!H", self.read_exact(2))[0]
                elif size == 127:
                    size = struct.unpack("!Q", self.read_exact(8))[0]
                if size > 65536 or not second & 128:
                    return
                mask = self.read_exact(4)
                payload = bytes(b ^ mask[i % 4] for i, b in enumerate(self.read_exact(size)))
                request = json.loads(payload)
                method, params = request["method"], request["params"]
                if method == "auth.login_with_api_key":
                    authenticated = params == ["test-key"]
                    self.reply(request, authenticated)
                elif not authenticated:
                    self.reply(request, error={"code": -32001, "data": {"reason": "Not authenticated", "errname": "EACCES"}})
                elif method == "app.query":
                    self.send_frame(b'{"jsonrpc":"2.0","method":"collection_update","params":[]}')
                    if scenario == "malformed":
                        self.send_frame(b'not json')
                    elif scenario == "oversize":
                        self.send_frame(b'x' * (4 * 1024 * 1024 + 1))
                    else:
                        self.reply(request, [{"name": "demo", "metadata": {"title": "Demo"}, "upgrade_available": True, "human_version": "1", "latest_version": "2"}])
                elif method in ("app.upgrade", "app.pull_images", "catalog.sync"):
                    expected = [] if method == "catalog.sync" else ["demo", {"app_version": "latest"}] if method == "app.upgrade" else ["demo", {"redeploy": True}]
                    if params != expected:
                        self.reply(request, error={"code": -32602, "message": "Wrong positional arguments"})
                    else:
                        with lock:
                            state["starts"] += 1
                        self.reply(request, 42)
                elif method == "core.get_jobs":
                    assert params == [[["id", "=", 42]]]
                    with lock:
                        state["polls"] += 1
                        polls, starts = state["polls"], state["starts"]
                    if scenario == "drop" and polls == 1:
                        self.connection.shutdown(socket.SHUT_RDWR)
                        return
                    if scenario == "failed":
                        self.reply(request, [{"state": "FAILED", "error": "pull failed\ntrace"}])
                    elif starts != 1:
                        self.reply(request, [{"state": "FAILED", "error": "mutation replayed"}])
                    else:
                        self.reply(request, [{"state": "SUCCESS"}])
                else:
                    self.reply(request, error={"code": -32601})
        except (EOFError, OSError):
            pass

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
print(server.server_port, flush=True)
server.serve_forever()
