"""Fast checks for the controlled 8.3 fixture, never CEF/UI evidence."""
import importlib.util
import io
import unittest
from pathlib import Path
from types import SimpleNamespace

spec = importlib.util.spec_from_file_location("policy_fixture", Path(__file__).with_name("component-policy-server.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class FixtureChecks(unittest.TestCase):
    def test_records_are_exact_typed_and_state_copies_do_not_clear_sticky_errors(self):
        core = fixture.Core()
        self.assertTrue(core.register("case1", "data:text/html,fixture"))
        row = {"label": "activation", "is_trusted": True,
               "has_been_active": True, "is_active": True}
        self.assertEqual(fixture.normalize_record(row), (row, None))
        self.assertEqual(fixture.normalize_record({"label": []})[1], "record-label")
        self.assertEqual(fixture.normalize_record({**row, "is_active": 1})[1], "record-fields")
        self.assertEqual(fixture.normalize_record({**row, "unknown": True})[1], "record-fields")
        core.add_record("case1", row)
        core.error("controlled-error", "case1")
        state = core.state("case1")
        state["records"].clear()
        state["errors"].clear()
        self.assertEqual(core.state("case1")["records"], [row])
        self.assertEqual(core.state("case1")["errors"], ["controlled-error"])
        self.assertNotIn("data_uri", state)
        # HTTP markup/scripts are raw UTF-8, not JSON-quoted strings. The first
        # native attempt exposed this despite the pure record protocol passing.
        responder = SimpleNamespace(_route="page", wfile=io.BytesIO(),
                                    send_response=lambda *_: None,
                                    send_header=lambda *_: None,
                                    end_headers=lambda: None)
        markup = '<!doctype html><title>Policy Probe</title><script>const x=1;</script>'
        fixture.Handler.respond(responder, 200, markup, "text/html; charset=utf-8")
        self.assertEqual(responder.wfile.getvalue(), markup.encode("utf-8"))
        responder.wfile = io.BytesIO()
        fixture.Handler.respond(responder, 200, {"ok": True})
        self.assertEqual(responder.wfile.getvalue(), b'{"ok":true}')

    def test_resource_and_control_budgets_and_all_forbidden_requests_remain_counted(self):
        core = fixture.Core()
        core.register("case2")
        for _ in range(fixture.MAX_REQUESTS):
            self.assertTrue(core.note("allowed", "case2", "plain"))
        self.assertFalse(core.note("allowed", "case2", "plain"))
        for _ in range(fixture.MAX_CONTROL_REQUESTS):
            self.assertTrue(core.note("allowed", "case2", "state", control=True))
        self.assertFalse(core.note("allowed", "case2", "state", control=True))
        core.note("forbidden", "case2", "unknown")
        core.note("forbidden", None, "unknown")
        self.assertEqual(core.state("case2")["forbidden_requests"], 1)
        self.assertEqual(core.report()["counters"]["forbidden_http"], 2)
        self.assertEqual(core.state("case2")["errors"], ["request-limit"])
        headers = fixture.LimitedHeaders(io.BytesIO(b"x" * (fixture.HEADER_LIMIT + 1)))
        with self.assertRaises(ValueError):
            headers.readline()


if __name__ == "__main__":
    unittest.main()
