#!/usr/bin/env python3
"""Tiny backend and a dependency-checking app for recovery experiments.

Run the backend directly and route the app's --backend URL through Toxiproxy.
The retry app becomes healthy after the dependency recovers. The latch app
intentionally stays unhealthy after its first failed dependency request.
No external packages or network services are required by these fixtures.
"""
from __future__ import annotations

import argparse
import json
import signal
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any


class QuietHandler(BaseHTTPRequestHandler):
    def log_message(self, *_args: Any) -> None:
        pass

    def send_json(self, status: int, value: Any) -> None:
        payload = json.dumps(value).encode("utf-8")
        try:
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        except (BrokenPipeError, ConnectionResetError):
            # A deliberately timed-out client is normal during a fault.
            pass


class BackendHandler(QuietHandler):
    def do_GET(self) -> None:
        if self.path in ("/work", "/healthz"):
            self.send_json(200, {"ok": True, "service": "backend"})
        else:
            self.send_json(404, {"error": "not found"})


class RunningServer:
    """Context-managed loopback server; port=0 selects a free ephemeral port."""

    def __init__(self, handler: type[BaseHTTPRequestHandler], host: str = "127.0.0.1", port: int = 0):
        self.server = ThreadingHTTPServer((host, port), handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    @property
    def url(self) -> str:
        host, port = self.server.server_address[:2]
        return f"http://{host}:{port}"

    def start(self) -> "RunningServer":
        self.thread.start()
        return self

    def close(self) -> None:
        if self.thread.is_alive():
            self.server.shutdown()
            self.thread.join(timeout=2)
        self.server.server_close()

    def __enter__(self) -> "RunningServer":
        return self.start()

    def __exit__(self, *_args: Any) -> None:
        self.close()


class ReferenceApp:
    def __init__(self, backend_url: str, *, mode: str = "retry", host: str = "127.0.0.1", port: int = 0,
                 request_timeout_ms: int = 100, poll_interval_ms: int = 25):
        if mode not in ("retry", "latch"):
            raise ValueError("mode must be retry or latch")
        if request_timeout_ms <= 0 or poll_interval_ms <= 0:
            raise ValueError("timeouts and polling intervals must be positive")
        self.backend_url = backend_url
        self.mode = mode
        self.request_timeout = request_timeout_ms / 1000
        self.poll_interval = poll_interval_ms / 1000
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self._healthy = False
        self._latched = False
        self._attempts = 0
        self._successes = 0
        self._failures = 0
        self._last_error: str | None = None
        # Ignore HTTP_PROXY/HTTPS_PROXY so local fixtures always stay local.
        self._opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        app = self

        class AppHandler(QuietHandler):
            def do_GET(self) -> None:
                state = app.state()
                if self.path in ("/healthz", "/health"):
                    self.send_json(200 if state["healthy"] else 503, state)
                elif self.path == "/state":
                    self.send_json(200, state)
                else:
                    self.send_json(404, {"error": "not found"})

        self.http = RunningServer(AppHandler, host, port)
        self.poller = threading.Thread(target=self._poll, name="reference-app-poller", daemon=True)

    @property
    def url(self) -> str:
        return self.http.url

    def state(self) -> dict[str, Any]:
        with self._lock:
            return {"healthy": self._healthy, "latched": self._latched, "mode": self.mode,
                    "attempts": self._attempts, "successes": self._successes,
                    "failures": self._failures, "last_error": self._last_error}

    def _poll(self) -> None:
        while not self._stop.is_set():
            ok, error = False, None
            try:
                with self._opener.open(self.backend_url, timeout=self.request_timeout) as response:
                    # Consume the body: a downstream latency toxic can delay it.
                    response.read()
                    ok = 200 <= response.status < 300
                    if not ok:
                        error = f"HTTP {response.status}"
            except (urllib.error.URLError, TimeoutError, OSError, ValueError) as exc:
                error = type(exc).__name__ + ": " + str(exc)
            with self._lock:
                self._attempts += 1
                self._successes += int(ok)
                self._failures += int(not ok)
                if not ok and self.mode == "latch":
                    self._latched = True
                self._healthy = ok and not self._latched
                self._last_error = error
            self._stop.wait(self.poll_interval)

    def start(self) -> "ReferenceApp":
        self.http.start()
        self.poller.start()
        return self

    def close(self) -> None:
        self._stop.set()
        self.poller.join(timeout=self.request_timeout + 1)
        self.http.close()

    def __enter__(self) -> "ReferenceApp":
        return self.start()

    def __exit__(self, *_args: Any) -> None:
        self.close()


def wait_healthy(url: str, timeout: float = 5) -> None:
    deadline = time.monotonic() + timeout
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    while time.monotonic() < deadline:
        try:
            with opener.open(url, timeout=0.25) as response:
                if response.status == 200:
                    return
        except (urllib.error.URLError, TimeoutError, OSError):
            pass
        time.sleep(0.025)
    raise TimeoutError(f"service did not become healthy: {url}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="service", required=True)
    backend = sub.add_parser("backend", help="dependency HTTP service")
    app = sub.add_parser("app", help="app polling a proxied dependency")
    for command, default_port in ((backend, 18081), (app, 18080)):
        command.add_argument("--host", default="127.0.0.1")
        command.add_argument("--port", type=int, default=default_port)
    app.add_argument("--backend", default="http://127.0.0.1:18082/work")
    app.add_argument("--mode", choices=("retry", "latch"), default="retry")
    app.add_argument("--request-timeout-ms", type=int, default=100)
    app.add_argument("--poll-interval-ms", type=int, default=25)
    args = parser.parse_args()
    stop = threading.Event()
    for signum in (signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, lambda *_: stop.set())
    if args.service == "backend":
        service = RunningServer(BackendHandler, args.host, args.port)
    else:
        service = ReferenceApp(args.backend, mode=args.mode, host=args.host, port=args.port,
                               request_timeout_ms=args.request_timeout_ms,
                               poll_interval_ms=args.poll_interval_ms)
    with service:
        print(json.dumps({"service": args.service, "url": service.url, "mode": getattr(args, "mode", None)}), flush=True)
        stop.wait()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
