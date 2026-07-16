#!/usr/bin/env python3
# Effects stub for scripts/smoke_effects.sh (port = argv[1]).
#   GET  /body.txt  -> fixed body
#   GET  /count     -> {"posts": N}  (how many POSTs /echo has taken — the
#                      replay proof: stays 1 across an engine kill -9)
#   POST /echo      -> {"echo": <body>, "x-keel": <header>} and counts
#   anything else   -> 404
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

count = 0


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _send(self, code, body):
        data = body.encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path == "/count":
            self._send(200, json.dumps({"posts": count}))
        elif self.path == "/body.txt":
            self._send(200, "stub body")
        else:
            self._send(404, json.dumps({"err": "nope"}))

    def do_POST(self):
        global count
        if self.path == "/echo":
            count += 1
            n = int(self.headers.get("content-length", 0))
            body = self.rfile.read(n).decode()
            self._send(200, json.dumps({"echo": body, "x-keel": self.headers.get("x-keel", "")}))
        else:
            self._send(404, json.dumps({"err": "nope"}))


HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
