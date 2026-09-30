"""Minimal Toxiproxy HTTP contract double, NOT a real fault-injection engine.

This lets CLI lifecycle/report/cleanup tests run without an installed Toxiproxy.
Its HTTP-only forwarding and timing are deliberately not claimed to reproduce
Toxiproxy TCP semantics. See test_real_toxiproxy.py for real-proxy coverage.
"""
from __future__ import annotations

import copy
import json
import socket
import threading
import time
import urllib.error
from urllib.parse import unquote, urlsplit

from fixtures.reference_app import QuietHandler, RunningServer
from tests.support import OPENER


class MockToxiproxy:
    def __init__(self, backend_url: str):
        self.lock = threading.Lock()
        self.events = []
        self.fail_delete = False
        self.fail_mutation_once = False
        self.proxy = {"name": "reference", "listen": "127.0.0.1:0",
                      "upstream": urlsplit(backend_url).netloc, "enabled": True, "toxics": []}
        owner = self

        class ProxyHandler(QuietHandler):
            def do_GET(self):
                proxy = owner.snapshot()
                if not proxy["enabled"] or any(t["type"] == "reset_peer" for t in proxy["toxics"]):
                    self.connection.shutdown(socket.SHUT_RDWR)
                    self.connection.close()
                    return
                latency = sum(t.get("attributes", {}).get("latency", 0)
                              for t in proxy["toxics"] if t["type"] == "latency")
                if latency:
                    time.sleep(latency / 1000)
                try:
                    with OPENER.open(backend_url + self.path, timeout=1) as response:
                        payload = response.read()
                        self.send_response(response.status)
                        self.send_header("Content-Type", "application/json")
                        self.send_header("Content-Length", str(len(payload)))
                        self.end_headers()
                        self.wfile.write(payload)
                except (BrokenPipeError, ConnectionResetError):
                    pass
                except (urllib.error.URLError, TimeoutError, OSError):
                    self.send_json(502, {"error": "backend unavailable"})

        class APIHandler(QuietHandler):
            def body(self):
                size = int(self.headers.get("Content-Length", "0"))
                return json.loads(self.rfile.read(size)) if size else {}

            def do_GET(self):
                path = unquote(urlsplit(self.path).path)
                if path == "/proxies":
                    self.send_json(200, {"reference": owner.snapshot()})
                elif path == "/proxies/reference":
                    self.send_json(200, owner.snapshot())
                elif path == "/proxies/reference/toxics":
                    self.send_json(200, owner.snapshot()["toxics"])
                elif path == "/version":
                    self.send_json(200, {"version": "mock-contract-only"})
                else:
                    self.send_json(404, {"error": "not found"})

            def do_POST(self):
                path = unquote(urlsplit(self.path).path)
                body = self.body()
                if path == "/proxies/reference":
                    with owner.lock:
                        owner.events.append(("update", copy.deepcopy(body)))
                        owner.proxy.update(body)
                        result = copy.deepcopy(owner.proxy)
                    self.send_json(200, result)
                elif path == "/proxies/reference/toxics":
                    with owner.lock:
                        if any(t["name"] == body["name"] for t in owner.proxy["toxics"]):
                            self.send_json(409, {"error": "toxic already exists"})
                            return
                        toxic = {"stream": "downstream", "toxicity": 1.0, **body}
                        owner.proxy["toxics"].append(toxic)
                        owner.events.append(("add", copy.deepcopy(toxic)))
                        uncertain = owner.fail_mutation_once
                        owner.fail_mutation_once = False
                    if uncertain:
                        # Simulate server applying an action before response loss.
                        self.connection.shutdown(socket.SHUT_RDWR)
                        self.connection.close()
                    else:
                        self.send_json(200, toxic)
                else:
                    self.send_json(404, {"error": "not found"})

            def do_DELETE(self):
                prefix = "/proxies/reference/toxics/"
                path = unquote(urlsplit(self.path).path)
                if not path.startswith(prefix):
                    self.send_json(404, {"error": "not found"})
                    return
                name = path[len(prefix):]
                with owner.lock:
                    owner.events.append(("delete", name))
                    if owner.fail_delete:
                        self.send_json(500, {"error": "forced cleanup failure"})
                        return
                    before = len(owner.proxy["toxics"])
                    owner.proxy["toxics"] = [t for t in owner.proxy["toxics"] if t["name"] != name]
                    removed = len(owner.proxy["toxics"]) != before
                if removed:
                    self.send_response(204)
                    self.end_headers()
                else:
                    self.send_json(404, {"error": "not found"})

        self.forwarder = RunningServer(ProxyHandler)
        self.api = RunningServer(APIHandler)
        self.proxy["listen"] = urlsplit(self.forwarder.url).netloc

    @property
    def url(self):
        return self.api.url

    @property
    def proxy_url(self):
        return self.forwarder.url

    def snapshot(self):
        with self.lock:
            return copy.deepcopy(self.proxy)

    def __enter__(self):
        self.forwarder.start()
        self.api.start()
        return self

    def __exit__(self, *_args):
        self.api.close()
        self.forwarder.close()
