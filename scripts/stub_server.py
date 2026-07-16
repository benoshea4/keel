#!/usr/bin/env python3
# Effects stub for scripts/smoke_effects.sh + smoke_secrets.sh (port = argv[1]).
#   GET  /body.txt  -> fixed body
#   GET  /slow      -> 200 after 3s (v2.1: the http-request timeout target)
#   GET  /count     -> {"posts": N, "auth": <last authorization header>}
#                      (posts stays 1 across an engine kill -9 = the replay
#                      proof; auth proves the WIRE carried the real secret
#                      while the journal holds a placeholder)
#   POST /echo      -> {"echo": <body>, "x-keel": <header>} and counts
#   anything else   -> 404
# Threading server ON PURPOSE: a /slow handler sleeping out its 3s must not
# block /count polls.
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

count = 0
last_auth = ""
last_key = ""


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
            self._send(200, json.dumps({"posts": count, "auth": last_auth, "key": last_key}))
        elif self.path == "/body.txt":
            self._send(200, "stub body")
        elif self.path == "/slow":
            time.sleep(3)
            try:
                self._send(200, json.dumps({"slow": True}))
            except BrokenPipeError:
                pass  # the engine gave up at its timeout — expected
        else:
            self._send(404, json.dumps({"err": "nope"}))

    def do_POST(self):
        global count, last_auth, last_key
        if self.path == "/echo":
            count += 1
            if self.headers.get("authorization"):
                last_auth = self.headers.get("authorization")
            if self.headers.get("keel-idempotency-key"):
                last_key = self.headers.get("keel-idempotency-key")
            n = int(self.headers.get("content-length", 0))
            body = self.rfile.read(n).decode()
            self._send(200, json.dumps({"echo": body, "x-keel": self.headers.get("x-keel", "")}))
        else:
            self._send(404, json.dumps({"err": "nope"}))


ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
