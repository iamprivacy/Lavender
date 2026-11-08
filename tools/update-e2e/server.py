#!/usr/bin/env python3
"""Local stand-in for the GitHub releases API and downloads. The scenario
served at /api/latest is read from $WORK/mode.txt on every request."""

import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

E = os.environ.get("WORK", "/private/tmp/lavender-update-e2e")
REL = os.path.join(E, "releases")


class Handler(BaseHTTPRequestHandler):
    def send_bytes(self, body, ctype):
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        with open(os.path.join(E, "mode.txt")) as f:
            mode = f.read().strip()
        with open(os.path.join(E, "requests.log"), "a") as log:
            log.write(f"{mode} {self.path}\n")
        if self.path == "/api/latest":
            if mode == "none":
                self.send_error(404)
                return
            with open(os.path.join(REL, mode + ".api.json"), "rb") as f:
                self.send_bytes(f.read(), "application/json")
            return
        if self.path.startswith("/dl/"):
            parts = self.path[len("/dl/"):].split("/")
            if len(parts) == 2 and ".." not in parts:
                path = os.path.join(REL, parts[0], parts[1])
                if os.path.isfile(path):
                    with open(path, "rb") as f:
                        self.send_bytes(f.read(), "application/octet-stream")
                    return
        self.send_error(404)

    def log_message(self, *args):
        pass


ThreadingHTTPServer(("127.0.0.1", 8765), Handler).serve_forever()
