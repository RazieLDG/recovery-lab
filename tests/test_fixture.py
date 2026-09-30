"""Fast tests for the standard-library reference app (no CLI binary required)."""
import time
import unittest

from fixtures.reference_app import BackendHandler, ReferenceApp, RunningServer, wait_healthy
from tests.mock_toxiproxy import MockToxiproxy
from tests.support import request_json


class ReferenceFixtureTests(unittest.TestCase):
    def test_backend_and_retry_app_are_healthy(self):
        with RunningServer(BackendHandler) as backend:
            with ReferenceApp(backend.url + "/work") as app:
                wait_healthy(app.url + "/healthz")
                state = request_json(app.url + "/state")
                self.assertTrue(state["healthy"])
                self.assertGreater(state["successes"], 0)

    def test_retry_recovers_and_latch_remains_failed(self):
        for mode in ("retry", "latch"):
            with self.subTest(mode=mode):
                with RunningServer(BackendHandler) as backend, MockToxiproxy(backend.url) as proxy:
                    with ReferenceApp(proxy.proxy_url + "/work", mode=mode) as app:
                        wait_healthy(app.url + "/healthz")
                        request_json(proxy.url + "/proxies/reference", "POST", {"enabled": False})
                        deadline = time.monotonic() + 2
                        while app.state()["failures"] == 0 and time.monotonic() < deadline:
                            time.sleep(0.01)
                        self.assertGreater(app.state()["failures"], 0)
                        request_json(proxy.url + "/proxies/reference", "POST", {"enabled": True})
                        if mode == "retry":
                            wait_healthy(app.url + "/healthz")
                        else:
                            time.sleep(0.1)
                            self.assertTrue(app.state()["latched"])
                            self.assertFalse(app.state()["healthy"])


if __name__ == "__main__":
    unittest.main()
