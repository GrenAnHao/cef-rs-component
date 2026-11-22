"""Protocol regressions for the external fixture, not CEF evidence."""
import http.client
import importlib.util
import json
from pathlib import Path
import threading
import unittest
from urllib.parse import urlsplit

spec = importlib.util.spec_from_file_location("profile_server", Path(__file__).with_name("component-profile-server.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class FixtureTests(unittest.TestCase):
    def setUp(self):
        self.server = module.Server()
        self.worker = threading.Thread(target=self.server.serve_forever, kwargs={"poll_interval": 0.01})
        self.worker.start()

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.worker.join(timeout=2)
        self.assertFalse(self.worker.is_alive())

    def request(self, method, path, payload=None):
        connection = http.client.HTTPConnection("127.0.0.1", self.server.server_port, timeout=2)
        body = json.dumps(payload).encode() if payload is not None else b""
        connection.request(method, path, body, {"Content-Type": "application/json"})
        response = connection.getresponse()
        data = response.read()
        status = response.status
        connection.close()
        return status, data

    def register_and_load(self, token="P1"):
        status, data = self.request("POST", "/register", {"token": token, "op": "read", "value": ""})
        self.assertEqual(status, 200)
        url = urlsplit(json.loads(data)["url"])
        self.assertEqual(self.request("GET", url.path + "?" + url.query)[0], 200)
        return token

    def test_explicit_null_is_a_result_but_missing_field_is_a_sticky_error(self):
        token = self.register_and_load()
        self.assertEqual(self.request("POST", "/result", {"token": token, "cookie": None})[0], 400)
        self.assertEqual(self.request("POST", "/result", {"token": token, "cookie": None, "local_storage": None})[0], 200)
        status, data = self.request("GET", "/observation?token=P1")
        self.assertEqual(status, 200)
        self.assertTrue(json.loads(data)["ready"])
        errors = json.loads(self.request("GET", "/health")[1])["errors"]
        self.assertEqual(errors, [{"token": "P1", "category": "invalid-result"}])

    def test_valid_result_does_not_hide_late_duplicate_or_page_error(self):
        token = self.register_and_load()
        payload = {"token": token, "cookie": None, "local_storage": None}
        self.assertEqual(self.request("POST", "/result", payload)[0], 200)
        self.assertEqual(self.request("POST", "/result", payload)[0], 400)
        self.assertEqual(self.request("POST", "/result", {"token": token, "error": "not stored"})[0], 400)
        self.assertEqual(len(json.loads(self.request("GET", "/health")[1])["errors"]), 2)

    def test_unknown_method_is_not_silently_ignored(self):
        self.assertEqual(self.request("PUT", "/result")[0], 501)
        errors = json.loads(self.request("GET", "/health")[1])["errors"]
        self.assertEqual(errors, [{"token": None, "category": "http-error-501"}])

    def test_registration_has_bounded_tokens_and_values(self):
        self.assertEqual(self.request("POST", "/register", {"token": "big", "op": "write", "value": "x" * 1025})[0], 400)
        for index in range(256):
            self.assertEqual(self.request("POST", "/register", {"token": str(index), "op": "read", "value": ""})[0], 200)
        self.assertEqual(self.request("POST", "/register", {"token": "overflow", "op": "read", "value": ""})[0], 400)
        self.assertEqual(len(self.server.tokens), 256)


if __name__ == "__main__":
    unittest.main()
