#!/usr/bin/env python3
"""Record real Toxiproxy recovery, failed-recovery, and cleanup evidence locally."""
from contextlib import ExitStack
from datetime import datetime, timezone
import argparse
import json
import os
from pathlib import Path
import signal
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from fixtures.reference_app import BackendHandler, ReferenceApp, RunningServer, wait_healthy
from tests.support import (ToxiproxyProcess, finish_cli, free_port, launch_cli,
                           recovery_binary, request_json, scenario, toxiproxy_binary,
                           wait_for_fault)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="new directory for JSON, JUnit, and logs")
    parser.add_argument("--binary")
    parser.add_argument("--toxiproxy")
    args = parser.parse_args()
    if args.binary:
        os.environ["RECOVERY_LAB_BIN"] = args.binary
    if args.toxiproxy:
        os.environ["TOXIPROXY_BIN"] = args.toxiproxy
    binary, proxy_binary = recovery_binary(), toxiproxy_binary()
    if not binary or not proxy_binary:
        parser.error("requires an already built CLI and installed Toxiproxy; use --binary and --toxiproxy")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    cases = [("outage-recovered", "outage", "retry", None, 0),
             ("latency-recovered", "latency", "retry", None, 0),
             ("reset-peer-recovered", "reset_peer", "retry", None, 0),
             ("latched-app-failed", "outage", "latch", None, 1)]
    if os.name == "posix":
        cases.extend([("sigint-restored", "outage", "retry", signal.SIGINT, 130),
                      ("sigterm-restored", "latency", "retry", signal.SIGTERM, 130)])
    summary = {"recorded_at_utc": datetime.now(timezone.utc).isoformat(), "engine": "real-toxiproxy",
               "binary": str(binary), "toxiproxy_binary": proxy_binary, "cases": []}
    for name, kind, mode, signum, expected in cases:
        directory = output / name
        directory.mkdir()
        with ExitStack() as stack:
            backend = stack.enter_context(RunningServer(BackendHandler))
            proxy = stack.enter_context(ToxiproxyProcess(proxy_binary, directory / "toxiproxy.log"))
            port = free_port()
            request_json(proxy.url + "/proxies", "POST", {
                "name": "reference", "listen": f"127.0.0.1:{port}",
                "upstream": backend.url.removeprefix("http://"), "enabled": True})
            before = request_json(proxy.url + "/proxies/reference")
            app = stack.enter_context(ReferenceApp(f"http://127.0.0.1:{port}/work", mode=mode))
            wait_healthy(app.url + "/healthz")
            config = scenario(proxy.url, app.url + "/healthz", kind=kind, name=name)
            if signum is not None:
                config["fault"]["duration_ms"] = 10000
            process, _, _ = launch_cli(binary, directory, config)
            try:
                if signum is not None:
                    wait_for_fault(proxy.url)
                    process.send_signal(signum)
                code, console = finish_cli(process)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.communicate()
            after = request_json(proxy.url + "/proxies/reference")
            evidence = {"before": before, "after": after, "restored": before == after,
                        "application": app.state()}
            (directory / "proxy-state.json").write_text(json.dumps(evidence, indent=2) + "\n")
            (directory / "console.log").write_text(console)
            passed = code == expected and before == after
            summary["cases"].append({"name": name, "expected_exit": expected, "actual_exit": code,
                                     "proxy_restored": before == after, "verified": passed,
                                     "report": f"{name}/report.json", "junit": f"{name}/report.xml"})
            print(f"{name}: exit {code} (expected {expected}), restored={before == after}", flush=True)
    (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    return 0 if all(case["verified"] for case in summary["cases"]) else 1


if __name__ == "__main__":
    raise SystemExit(main())
