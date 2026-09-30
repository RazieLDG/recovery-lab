"""Shared harness for real Toxiproxy integration tests and mock contract tests."""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import time
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[1]
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def request_json(url: str, method: str = "GET", body=None, timeout: float = 2):
    encoded = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(url, data=encoded, method=method,
                                     headers={"Content-Type": "application/json"})
    with OPENER.open(request, timeout=timeout) as response:
        raw = response.read()
        return json.loads(raw) if raw else None


def free_port() -> int:
    # Reserve only long enough to select it; callers retry setup if another
    # process wins the unavoidable bind race when launching an external binary.
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def recovery_binary() -> Path | None:
    override = os.environ.get("RECOVERY_LAB_BIN")
    if override:
        resolved = shutil.which(override) or override
        path = Path(resolved).resolve()
        if not path.is_file() or not os.access(path, os.X_OK):
            raise RuntimeError(f"RECOVERY_LAB_BIN is not executable: {override}")
        return path
    for profile in ("debug", "release"):
        path = ROOT / "target" / profile / "recovery-lab"
        if path.is_file() and os.access(path, os.X_OK):
            return path
    return None


def toxiproxy_binary() -> str | None:
    override = os.environ.get("TOXIPROXY_BIN")
    if override:
        resolved = shutil.which(override) or override
        if not Path(resolved).is_file() or not os.access(resolved, os.X_OK):
            raise RuntimeError(f"TOXIPROXY_BIN is not executable: {override}")
        return str(Path(resolved).resolve())
    return shutil.which("toxiproxy-server")


def scenario(api_url: str, health_url: str, *, kind: str = "outage", name: str = "fixture-recovery") -> dict:
    fault = {"kind": kind, "duration_ms": 500}
    if kind == "latency":
        fault["latency_ms"] = 400
    return {"name": name, "toxiproxy_url": api_url, "proxy": "reference",
            "health_url": health_url, "fault": fault, "request_timeout_ms": 150,
            "recovery_timeout_ms": 1500, "poll_interval_ms": 25,
            "preflight_timeout_ms": 1500}


def launch_cli(binary: Path, directory: Path, config: dict) -> tuple[subprocess.Popen, Path, Path]:
    config_path = directory / "scenario.json"
    json_path, junit_path = directory / "report.json", directory / "report.xml"
    config_path.write_text(json.dumps(config), encoding="utf-8")
    process = subprocess.Popen([str(binary), "run", str(config_path), "--json", str(json_path),
                                "--junit", str(junit_path)], cwd=ROOT, text=True,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    return process, json_path, junit_path


def finish_cli(process: subprocess.Popen, timeout: float = 12) -> tuple[int, str]:
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        process.kill()
        stdout, stderr = process.communicate()
        raise AssertionError(f"CLI hung beyond {timeout}s\n{stdout}\n{stderr}")
    return process.returncode, stdout + stderr


def check_reports(testcase, json_path: Path, junit_path: Path, *, failed: bool):
    testcase.assertTrue(json_path.is_file(), "JSON report missing")
    testcase.assertTrue(junit_path.is_file(), "JUnit report missing")
    report = json.loads(json_path.read_text(encoding="utf-8"))
    testcase.assertIsInstance(report, dict)
    testcase.assertEqual(report["schema_version"], 1)
    testcase.assertIn(report["outcome"], ("passed", "failed", "error", "cancelled"))
    testcase.assertIn(report["cleanup"], ("not_needed", "succeeded", "failed"))
    testcase.assertIn(report["exit_code"], (0, 1, 2, 3, 130))
    testcase.assertEqual(report["exit_code"] != 0, failed)
    testcase.assertIsInstance(report["elapsed_ms"], int)
    testcase.assertIsInstance(report["events"], list)
    times = [event["elapsed_ms"] for event in report["events"]]
    testcase.assertEqual(times, sorted(times), "event timeline must be monotonic")
    for event in report["events"]:
        testcase.assertIsInstance(event["phase"], str)
        testcase.assertIsInstance(event["detail"], str)
    xml = ET.parse(junit_path).getroot()
    testcase.assertIn(xml.tag, ("testsuite", "testsuites"))
    testcase.assertTrue(xml.findall(".//testcase"), "JUnit report must contain a testcase")
    failure_nodes = xml.findall(".//failure") + xml.findall(".//error")
    testcase.assertEqual(bool(failure_nodes), failed, "JUnit result must agree with process status")
    return report


class ToxiproxyProcess:
    def __init__(self, binary: str, log_path: Path):
        self.port = free_port()
        self.url = f"http://127.0.0.1:{self.port}"
        self.binary, self.log_path = binary, log_path
        self.process = None
        self.log = None

    def __enter__(self):
        self.log = self.log_path.open("w+")
        self.process = subprocess.Popen([self.binary, "-host", "127.0.0.1", "-port", str(self.port)],
                                        stdout=self.log, stderr=subprocess.STDOUT)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                break
            try:
                request_json(self.url + "/proxies")
                return self
            except (urllib.error.URLError, TimeoutError, OSError):
                time.sleep(0.025)
        self.__exit__(None, None, None)
        raise RuntimeError("Toxiproxy startup failed: " + self.log_path.read_text())

    def __exit__(self, *_args):
        if self.process is not None and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=2)
        if self.log is not None:
            self.log.close()


def wait_for_fault(api_url: str, timeout: float = 4):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        proxy = request_json(api_url + "/proxies/reference")
        if not proxy["enabled"] or proxy.get("toxics"):
            return proxy
        time.sleep(0.02)
    raise AssertionError("fault was never observed in the proxy API")


def interrupt(process: subprocess.Popen) -> None:
    if os.name == "posix":
        process.send_signal(signal.SIGINT)
    else:
        raise RuntimeError("cancellation test requires POSIX signal semantics")
