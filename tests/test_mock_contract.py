"""CLI integration against a MOCK API; these do not prove real TCP fault behavior."""
from contextlib import ExitStack
import json
import os
from pathlib import Path
import tempfile
import time
import xml.etree.ElementTree as ET
import unittest

from fixtures.reference_app import BackendHandler, QuietHandler, ReferenceApp, RunningServer, wait_healthy
from tests.mock_toxiproxy import MockToxiproxy
from tests.support import check_reports, finish_cli, interrupt, launch_cli, recovery_binary, scenario, wait_for_fault


class MockContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.binary = recovery_binary()
        if cls.binary is None:
            raise unittest.SkipTest("CLI binary unavailable; run cargo build or set RECOVERY_LAB_BIN")

    def setUp(self):
        self.stack = ExitStack()
        self.addCleanup(self.stack.close)
        self.directory = Path(self.stack.enter_context(tempfile.TemporaryDirectory(prefix="recovery-lab-mock-")))
        self.run_number = 0
        self.backend = self.stack.enter_context(RunningServer(BackendHandler))
        self.proxy = self.stack.enter_context(MockToxiproxy(self.backend.url))

    def app(self, mode="retry"):
        app = self.stack.enter_context(ReferenceApp(self.proxy.proxy_url + "/work", mode=mode))
        wait_healthy(app.url + "/healthz")
        return app

    def run_config(self, config):
        self.run_number += 1
        run_dir = self.directory / str(self.run_number)
        run_dir.mkdir()
        process, report, junit = launch_cli(self.binary, run_dir, config)
        code, output = finish_cli(process)
        return code, output, report, junit

    def test_all_faults_recover_and_restore_proxy(self):
        app = self.app()
        for kind in ("outage", "latency", "reset_peer"):
            with self.subTest(kind=kind):
                before = self.proxy.snapshot()
                code, output, report, junit = self.run_config(scenario(self.proxy.url, app.url + "/healthz", kind=kind))
                self.assertEqual(code, 0, output)
                self.assertEqual(self.proxy.snapshot(), before)
                check_reports(self, report, junit, failed=False)

    def test_nonrecoverable_app_fails_but_proxy_is_restored(self):
        app = self.app("latch")
        before = self.proxy.snapshot()
        code, output, report, junit = self.run_config(scenario(self.proxy.url, app.url + "/healthz"))
        self.assertEqual(code, 1, output)
        self.assertEqual(self.proxy.snapshot(), before)
        self.assertTrue(app.state()["latched"])
        check_reports(self, report, junit, failed=True)

    def test_rejects_preexisting_toxic_without_changing_it(self):
        app = self.app()
        existing = {"name": "user-owned", "type": "latency", "stream": "downstream",
                    "toxicity": 1.0, "attributes": {"latency": 1, "jitter": 0}}
        with self.proxy.lock:
            self.proxy.proxy["toxics"].append(existing)
        before = self.proxy.snapshot()
        config = scenario(self.proxy.url, app.url + "/healthz", kind="latency")
        code, output, report, junit = self.run_config(config)
        self.assertEqual(code, 2, output)
        self.assertEqual(self.proxy.snapshot(), before)
        self.assertEqual(self.proxy.events, [])
        check_reports(self, report, junit, failed=True)

    def test_existing_report_destinations_rejected_without_mutation(self):
        app = self.app()
        for existing_name in ("report.json", "report.xml"):
            with self.subTest(existing=existing_name):
                run_dir = self.directory / existing_name.replace(".", "-")
                run_dir.mkdir()
                existing = run_dir / existing_name
                existing.write_text("previous evidence must survive", encoding="utf-8")
                config = scenario(self.proxy.url, app.url + "/healthz")
                process, _, _ = launch_cli(self.binary, run_dir, config)
                code, output = finish_cli(process)
                self.assertEqual(code, 2, output)
                self.assertEqual(existing.read_text(), "previous evidence must survive")
                self.assertEqual(self.proxy.events, [])

    def test_fault_window_too_short_rejected_without_mutation(self):
        app = self.app()
        for duration in (150, 174):
            with self.subTest(duration_ms=duration):
                config = scenario(self.proxy.url, app.url + "/healthz")
                config["fault"]["duration_ms"] = duration
                code, output, report, junit = self.run_config(config)
                self.assertEqual(code, 2, output)
                self.assertEqual(self.proxy.events, [])
                check_reports(self, report, junit, failed=True)

    def test_fault_window_exact_minimum_is_accepted(self):
        # The health endpoint intentionally bypasses the dependency, yielding
        # assertion failure 1 rather than configuration error 2.
        config = scenario(self.proxy.url, self.backend.url + "/healthz")
        config["fault"]["duration_ms"] = config["request_timeout_ms"] + config["poll_interval_ms"]
        code, output, report, junit = self.run_config(config)
        self.assertEqual(code, 1, output)
        self.assertTrue(self.proxy.events)
        check_reports(self, report, junit, failed=True)

    def test_xml_noncharacters_are_sanitized_in_junit(self):
        app = self.app()
        name = "xml <&\"'\ufffe\uffff"
        config = scenario(self.proxy.url, app.url + "/healthz", name=name)
        code, output, report, junit = self.run_config(config)
        self.assertEqual(code, 0, output)
        result = check_reports(self, report, junit, failed=False)
        self.assertEqual(result["scenario"], name)
        testcase = ET.parse(junit).getroot().find(".//testcase")
        self.assertEqual(testcase.attrib["name"], name.replace("\ufffe", "\ufffd").replace("\uffff", "\ufffd"))

    def test_late_healthy_response_does_not_pass_deadline(self):
        class SlowHealthyHandler(QuietHandler):
            def do_GET(self):
                time.sleep(0.12)
                self.send_json(200, {"ok": True})

        late = self.stack.enter_context(RunningServer(SlowHealthyHandler))
        config = scenario(self.proxy.url, late.url + "/healthz")
        config["preflight_timeout_ms"] = 60
        code, output, report, junit = self.run_config(config)
        self.assertEqual(code, 1, output)
        self.assertEqual(self.proxy.events, [])
        result = check_reports(self, report, junit, failed=True)
        self.assertEqual(result["cleanup"], "not_needed")

    def test_unknown_field_rejected_without_mutation(self):
        app = self.app()
        config = scenario(self.proxy.url, app.url + "/healthz")
        config["duraton_ms"] = 100
        code, output, _, _ = self.run_config(config)
        self.assertEqual(code, 2, output)
        self.assertEqual(self.proxy.events, [])

    def test_unknown_fault_field_rejected_without_mutation(self):
        app = self.app()
        config = scenario(self.proxy.url, app.url + "/healthz")
        config["fault"]["duraton_ms"] = 100
        code, output, _, _ = self.run_config(config)
        self.assertEqual(code, 2, output)
        self.assertEqual(self.proxy.events, [])

    def test_remote_targets_rejected_by_default(self):
        app = self.app()
        config = scenario("http://192.0.2.1:8474", app.url + "/healthz")
        code, output, _, _ = self.run_config(config)
        self.assertEqual(code, 2, output)
        self.assertEqual(self.proxy.events, [])

    def test_preexisting_disabled_proxy_is_rejected(self):
        app = self.app()
        with self.proxy.lock:
            self.proxy.proxy["enabled"] = False
        before = self.proxy.snapshot()
        code, output, report, junit = self.run_config(scenario(self.proxy.url, app.url + "/healthz"))
        self.assertEqual(code, 2, output)
        self.assertEqual(self.proxy.snapshot(), before)
        self.assertEqual(self.proxy.events, [])
        check_reports(self, report, junit, failed=True)

    def test_mutation_response_loss_still_cleans_up(self):
        app = self.app()
        before = self.proxy.snapshot()
        self.proxy.fail_mutation_once = True
        config = scenario(self.proxy.url, app.url + "/healthz", kind="latency")
        code, output, report, junit = self.run_config(config)
        self.assertEqual(code, 2, output)
        self.assertEqual(self.proxy.snapshot(), before)
        result = check_reports(self, report, junit, failed=True)
        self.assertEqual(result["cleanup"], "succeeded")

    def test_no_observable_fault_is_inconclusive_failure(self):
        # A healthy endpoint unrelated to the proxy must never produce a pass.
        config = scenario(self.proxy.url, self.backend.url + "/healthz")
        before = self.proxy.snapshot()
        code, output, report, junit = self.run_config(config)
        self.assertEqual(code, 1, output)
        self.assertEqual(self.proxy.snapshot(), before)
        check_reports(self, report, junit, failed=True)

    def test_cleanup_failure_has_dedicated_exit(self):
        app = self.app()
        self.proxy.fail_delete = True
        config = scenario(self.proxy.url, app.url + "/healthz", kind="latency")
        code, output, report, junit = self.run_config(config)
        self.assertEqual(code, 3, output)
        self.assertTrue(self.proxy.snapshot()["toxics"], "forced deletion failure should leave a toxic")
        check_reports(self, report, junit, failed=True)

    @unittest.skipUnless(os.name == "posix", "SIGINT cancellation is a POSIX integration test")
    def test_sigint_restores_outage(self):
        app = self.app()
        config = scenario(self.proxy.url, app.url + "/healthz")
        config["fault"]["duration_ms"] = 10000
        before = self.proxy.snapshot()
        process, report, junit = launch_cli(self.binary, self.directory, config)
        try:
            wait_for_fault(self.proxy.url)
            interrupt(process)
            code, output = finish_cli(process)
            self.assertEqual(code, 130, output)
            self.assertEqual(self.proxy.snapshot(), before)
            check_reports(self, report, junit, failed=True)
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate()

    @unittest.skipUnless(os.name == "posix", "SIGINT cancellation is a POSIX integration test")
    def test_sigint_removes_only_owned_toxic(self):
        app = self.app()
        config = scenario(self.proxy.url, app.url + "/healthz", kind="latency")
        config["fault"]["duration_ms"] = 10000
        before = self.proxy.snapshot()
        process, report, junit = launch_cli(self.binary, self.directory, config)
        try:
            wait_for_fault(self.proxy.url)
            interrupt(process)
            code, output = finish_cli(process)
            self.assertEqual(code, 130, output)
            self.assertEqual(self.proxy.snapshot(), before)
            check_reports(self, report, junit, failed=True)
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate()


if __name__ == "__main__":
    unittest.main()
