#!/usr/bin/env python3
"""Fixed-origin, cross-process profile fixture; never records storage values in errors."""
import argparse
import json
import os
from pathlib import Path
import socket
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlencode, urlsplit

LIMIT = 16384
PAGE = (Path(__file__).resolve().parent.parent / "assets" / "profile-test.html").read_bytes()


def atomic_json(path, data):
    path = Path(path)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(data), encoding="utf-8")
    os.replace(tmp, path)


class Server(ThreadingHTTPServer):
    daemon_threads = False
    block_on_close = True
    allow_reuse_address = True

    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.origin = "http://127.0.0.1:%d" % self.server_port
        self.lock = threading.Lock()
        self.tokens = {}
        self.errors = []
        self.stopping = False

    def error(self, token, category):
        # Only the token and category, never cookies, localStorage or body.
        safe = token if isinstance(token, str) and len(token) <= 256 else (None if token is None else "<invalid>")
        with self.lock:
            if len(self.errors) < 64:
                self.errors.append({"token": safe, "category": category})
            elif self.errors[-1]["category"] != "additional-errors-omitted":
                self.errors.append({"token": None, "category": "additional-errors-omitted"})

    def get_request(self):
        conn, addr = super().get_request()
        conn.settimeout(1.0)
        return conn, addr

    def handle_error(self, request, client_address):
        # Known transport cancellation is normal; unexpected handler failures
        # remain sticky without leaking bodies, headers or tracebacks.
        error_type = sys.exc_info()[0]
        if error_type is None or not issubclass(error_type, OSError):
            self.error(None, "internal-handler-error")


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def handle_one_request(self):
        # Limit aggregate header bytes, not just individual header lines.
        original = self.rfile
        self.rfile = LimitedHeaders(original)
        try:
            super().handle_one_request()
        except ValueError:
            self.server.error(None, "headers-too-large")
            self.close_connection = True
        except OSError:
            self.close_connection = True
        finally:
            self.rfile = original

    def respond(self, status, obj=None, content_type="application/json"):
        body = b"" if obj is None else (obj if isinstance(obj, bytes) else json.dumps(obj).encode())
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True
        self.wfile.write(body)

    def send_error(self, code, message=None, explain=None):
        # BaseHTTPRequestHandler rejects unknown methods and malformed headers
        # through this path. Keep those failures sticky without reflecting input.
        category = "http-error-%d" % int(code)
        self.server.error(None, category)
        self.respond(int(code), {"error": category})

    def fail(self, token, category):
        self.server.error(token, category)
        self.respond(400, {"error": category})

    def body(self):
        length = self.headers.get("Content-Length")
        if self.headers.get("Transfer-Encoding") or length is None or not length.isdigit() or int(length) > LIMIT:
            raise ValueError("body-length")
        raw = self.rfile.read(int(length))
        if len(raw) != int(length):
            raise ValueError("body-truncated")
        try:
            value = json.loads(raw)
        except (ValueError, UnicodeError):
            raise ValueError("body-json") from None
        if not isinstance(value, dict):
            raise ValueError("body-type")
        return value

    def do_GET(self):
        parsed = urlsplit(self.path)
        query = parse_qs(parsed.query, keep_blank_values=True)
        if parsed.path == "/favicon.ico":
            return self.respond(204)
        if parsed.path == "/health":
            with self.server.lock:
                errors = list(self.server.errors)
            return self.respond(200, {"errors": errors})
        if parsed.path in ("/observation", "/profile-test"):
            tokens = query.get("token", [])
            token = tokens[0] if len(tokens) == 1 else None
            with self.server.lock:
                entry = self.server.tokens.get(token)
                if entry and parsed.path == "/profile-test":
                    valid = (not entry["served"] and query.get("op") == [entry["op"]]
                             and query.get("value") == [entry["value"]])
                    if valid:
                        entry["served"] = True
                elif entry:
                    valid = True
                    result = entry["result"]
                else:
                    valid = False
            if not valid:
                return self.fail(token, "unknown-or-invalid-token-request")
            if parsed.path == "/profile-test":
                return self.respond(200, PAGE, "text/html; charset=utf-8")
            if result is None:
                return self.respond(200, {"ready": False})
            return self.respond(200, {"ready": True, **result})
        self.fail(None, "unknown-path")

    def do_POST(self):
        path = urlsplit(self.path).path
        try:
            obj = self.body()
        except (ValueError, OSError) as exc:
            return self.fail(None, str(exc) if isinstance(exc, ValueError) else "body-read")
        if path == "/stop":
            self.respond(200, {"stopping": True})
            with self.server.lock:
                if not self.server.stopping:
                    self.server.stopping = True
                    threading.Thread(target=self.server.shutdown, daemon=True).start()
            return
        token = obj.get("token")
        if (not isinstance(token, str) or not token or len(token) > 256
                or any(not (c.isascii() and (c.isalnum() or c in "-_.:")) for c in token)):
            return self.fail(None, "invalid-token")
        if path == "/register":
            op, value = obj.get("op"), obj.get("value")
            if (set(obj) != {"token", "op", "value"} or op not in ("read", "write")
                    or not isinstance(value, str) or len(value.encode("utf-8")) > 1024):
                return self.fail(token, "invalid-registration")
            with self.server.lock:
                duplicate = token in self.server.tokens
                full = len(self.server.tokens) >= 256
                if not duplicate and not full:
                    self.server.tokens[token] = {"op": op, "value": value, "served": False, "result": None}
            if duplicate or full:
                return self.fail(token, "duplicate-registration" if duplicate else "token-limit")
            url = self.server.origin + "/profile-test?" + urlencode({"token": token, "op": op, "value": value})
            return self.respond(200, {"url": url})
        if path == "/result":
            with self.server.lock:
                entry = self.server.tokens.get(token)
                if not entry:
                    error = "unknown-result"
                elif not entry["served"] or entry["result"] is not None:
                    error = "duplicate-or-unserved-result"
                elif "error" in obj:
                    error = "page-error" if set(obj) == {"token", "error"} and isinstance(obj["error"], str) else "invalid-result"
                elif set(obj) != {"token", "cookie", "local_storage"} or any(key not in obj or (obj[key] is not None and not isinstance(obj[key], str)) for key in ("cookie", "local_storage")):
                    error = "invalid-result"
                else:
                    error = None
                    entry["result"] = {key: obj[key] for key in ("token", "cookie", "local_storage")}
            if error:
                return self.fail(token, error)
            return self.respond(200, {"ok": True})
        self.fail(token, "unknown-path")


class LimitedHeaders:
    """Count request line and headers, then pass body reads through unchanged."""
    def __init__(self, wrapped):
        self.wrapped = wrapped
        self.remaining = LIMIT
        self.headers_done = False

    def readline(self, size=-1):
        if self.headers_done:
            return self.wrapped.readline(size)
        line = self.wrapped.readline(min(size if size >= 0 else LIMIT + 1, self.remaining + 1))
        self.remaining -= len(line)
        if self.remaining < 0:
            raise ValueError("headers-too-large")
        if line in (b"\r\n", b"\n"):
            self.headers_done = True
        return line

    def read(self, size=-1):
        return self.wrapped.read(size)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--ready-file", type=Path, required=True)
    parser.add_argument("--final-report", type=Path, required=True)
    args = parser.parse_args()
    if not args.ready_file.is_absolute() or not args.final_report.is_absolute():
        parser.error("report paths must be absolute")
    server = Server()
    try:
        atomic_json(args.ready_file, {"origin": server.origin, "pid": os.getpid()})
        server.serve_forever(poll_interval=0.1)
    finally:
        server.server_close()  # joins request threads before final error snapshot
        with server.lock:
            report = {"errors": list(server.errors), "registered": len(server.tokens)}
        atomic_json(args.final_report, report)


if __name__ == "__main__":
    main()
