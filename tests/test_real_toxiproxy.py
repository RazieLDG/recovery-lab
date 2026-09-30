"""End-to-end tests using the REAL Toxiproxy binary and HTTP reference app.

Set TOXIPROXY_BIN to an already installed toxiproxy-server executable. These tests
never install or download it. A missing binary is an explicit unittest skip.
"""
from contextlib import ExitStack
import os
import signal
from pathlib import Path
import tempfile
import unittest

from fixtures.reference_app import BackendHandler, ReferenceApp, RunningServer, wait_healthy
from tests.support import (ToxiproxyProcess, check_reports, finish_cli, free_port, interrupt,
                           launch_cli, recovery_binary, request_json, scenario,
                           toxiproxy_binary, wait_for_fault)


class RealToxiproxyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.binary = recovery_binary()
        cls.toxiproxy = toxiproxy_binary()
        if cls.binary is None:
            raise unittest.SkipTest("CLI binary unavailable; run cargo build or set RECOVERY_LAB_BIN")
        if cls.toxiproxy is None:
            raise unittest.SkipTest("REAL Toxiproxy unavailable; set TOXIPROXY_BIN (not downloaded automatically)")

    def setUp(self):
        self.stack = ExitStack()
        self.addCleanup(self.stack.close)
        self.directory = Path(self.stack.enter_context(tempfile.TemporaryDirectory(prefix="recovery-lab-real-")))
        self.backend = self.stack.enter_context(RunningServer(BackendHandler))
        self.proxy = self.stack.enter_context(ToxiproxyProcess(self.toxiproxy, self.directory / "toxiproxy.log"))
        self.proxy_port = free_port()
        request_json(self.proxy.url + "/proxies", "POST", {
            "name": "reference", "listen": f"127.0.0.1:{self.proxy_port}",
            "upstream": self.backend.url.removeprefix("http://"), "enabled": True})
        self.before = request_json(self.proxy.url + "/proxies/reference")

    def app(self, mode="retry"):
        app = self.stack.enter_context(ReferenceApp(f"http://127.0.0.1:{self.proxy_port}/work", mode=mode))
        wait_healthy(app.url + "/healthz")
        return app

    def assert_restored(self):
        self.assertEqual(request_json(self.proxy.url + "/proxies/reference"), self.before)

    def exercise(self, kind, mode="retry"):
        app = self.app(mode)
        config = scenario(self.proxy.url, app.url + "/healthz", kind=kind)
        process, report_path, junit_path = launch_cli(self.binary, self.directory, config)
        code, output = finish_cli(process)
        expected = 1 if mode == "latch" else 0
        self.assertEqual(code, expected, output)
        self.assert_restored()
        report = check_reports(self, report_path, junit_path, failed=bool(expected))
        self.assertEqual(report["cleanup"], "succeeded")
        self.assertGreater(app.state()["failures"], 0, "application must actually see a dependency failure")
        if mode == "latch":
            self.assertTrue(app.state()["latched"])
        else:
            wait_healthy(app.url + "/healthz")

    def test_real_outage_recovers(self):
        self.exercise("outage")

    def test_real_latency_recovers(self):
        self.exercise("latency")

    def test_real_reset_peer_recovers(self):
        self.exercise("reset_peer")

    def test_real_nonrecoverable_app_fails(self):
        self.exercise("outage", "latch")

    @unittest.skipUnless(os.name == "posix", "signal cancellation is a POSIX integration test")
    def test_real_sigint_restores_connectivity(self):
        self.exercise_cancellation("outage", signal.SIGINT)

    @unittest.skipUnless(os.name == "posix", "signal cancellation is a POSIX integration test")
    def test_real_sigterm_removes_latency_toxic(self):
        self.exercise_cancellation("latency", signal.SIGTERM)

    def exercise_cancellation(self, kind, signum):
        app = self.app()
        config = scenario(self.proxy.url, app.url + "/healthz", kind=kind)
        config["fault"]["duration_ms"] = 10000
        process, report_path, junit_path = launch_cli(self.binary, self.directory, config)
        try:
            wait_for_fault(self.proxy.url)
            process.send_signal(signum)
            code, output = finish_cli(process)
            self.assertEqual(code, 130, output)
            self.assert_restored()
            report = check_reports(self, report_path, junit_path, failed=True)
            self.assertEqual(report["cleanup"], "succeeded")
            wait_healthy(app.url + "/healthz")
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate()


if __name__ == "__main__":
    unittest.main()
