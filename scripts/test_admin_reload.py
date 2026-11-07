"""Offline client tests. Only ephemeral loopback mocks; never a real daemon."""

import contextlib
import http.server
import importlib.util
import io
import os
from pathlib import Path
import socket
import subprocess
import threading
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("admin_reload", ROOT / "scripts/admin_reload.py")
CLIENT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CLIENT)
TOKEN = "offline-test-only-" + "x" * 32


@contextlib.contextmanager
def server(status):
    requests = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            requests.append((self.path, self.headers.get_all("Authorization")))
            accepted = self.headers.get("Authorization") == "Bearer " + TOKEN
            self.send_response(status if accepted else 401)
            self.send_header("Location", "http://127.0.0.1:1/must-not-follow")
            self.end_headers()
            # Deliberately echo the dummy token to catch output leaks.
            self.wfile.write(TOKEN.encode())

        def log_message(self, *args):
            pass

    httpd = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()
    try:
        yield httpd.server_port, requests
    finally:
        httpd.shutdown()
        httpd.server_close()
        thread.join()


class ReloadTests(unittest.TestCase):
    def run_make(self, port, token=TOKEN):
        env = os.environ.copy()
        env.pop("PG_ADMIN_TOKEN", None)
        env.pop("MAKEFLAGS", None)
        env.pop("MFLAGS", None)
        env["PG_HEALTH_PORT"] = str(port)
        if token is not None:
            env["PG_ADMIN_TOKEN"] = token
        result = subprocess.run(["make", "--no-print-directory", "reload"],
                                cwd=ROOT, env=env, capture_output=True, text=True, timeout=15)
        self.assertNotIn(TOKEN, result.stdout + result.stderr)
        if token:
            self.assertNotIn(token, result.stdout + result.stderr)
        self.assertNotIn(TOKEN, " ".join(result.args))
        return result

    def test_only_202_is_success_and_header_is_single_bearer(self):
        for status in (202, 200, 204, 301, 401, 403, 503):
            with self.subTest(status=status), server(status) as (port, requests):
                result = self.run_make(port)
                self.assertEqual(result.returncode == 0, status == 202)
                self.assertEqual(requests, [("/admin/reload", ["Bearer " + TOKEN])])
                self.assertIn(str(status), result.stdout + result.stderr)

    def test_wrong_validly_shaped_token_fails(self):
        with server(202) as (port, requests):
            result = self.run_make(port, "wrong-token-" + "y" * 32)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(len(requests), 1)
            self.assertIn("401", result.stderr)

    def test_invalid_or_missing_token_never_connects(self):
        for token in (None, "", "short", "x" * 513, TOKEN + "\n", TOKEN + " ", TOKEN + "é"):
            with self.subTest(token_length=len(token or "")):
                with mock.patch.dict(os.environ, {"PG_ADMIN_TOKEN": token or ""}, clear=True):
                    with mock.patch.object(CLIENT.http.client, "HTTPConnection") as connection:
                        with contextlib.redirect_stderr(io.StringIO()):
                            self.assertEqual(CLIENT.main(), 1)
                        connection.assert_not_called()
        self.assertNotEqual(self.run_make(1, None).returncode, 0)

    def test_invalid_port_never_connects(self):
        for port in ("0", "65536", "abc", "80/other"):
            with mock.patch.dict(os.environ, {"PG_ADMIN_TOKEN": TOKEN, "PG_HEALTH_PORT": port}):
                with mock.patch.object(CLIENT.http.client, "HTTPConnection") as connection:
                    with contextlib.redirect_stderr(io.StringIO()):
                        self.assertEqual(CLIENT.main(), 1)
                    connection.assert_not_called()

    def test_connection_failure_reaches_make(self):
        # Bound but not listening: no unrelated service can claim this port.
        with socket.socket() as reserved:
            reserved.bind(("127.0.0.1", 0))
            result = self.run_make(reserved.getsockname()[1])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("connection or HTTP protocol error", result.stderr)

    def test_timeout_is_redacted_and_connection_closed(self):
        with mock.patch.dict(os.environ, {"PG_ADMIN_TOKEN": TOKEN}, clear=True):
            with mock.patch.object(CLIENT.http.client, "HTTPConnection") as connection:
                connection.return_value.request.side_effect = TimeoutError(TOKEN)
                output = io.StringIO()
                with contextlib.redirect_stderr(output):
                    self.assertEqual(CLIENT.main(), 1)
                self.assertNotIn(TOKEN, output.getvalue())
                connection.assert_called_once_with("127.0.0.1", 8080, timeout=10)
                connection.return_value.close.assert_called_once()

    def test_recipe_does_not_expand_token_into_python_argv(self):
        recipe = (ROOT / "Makefile").read_text().split("\nreload:\n", 1)[1].split("\n\n", 1)[0]
        self.assertEqual(recipe.strip(), "@python3 scripts/admin_reload.py")
        result = subprocess.run(["make", "-n", "reload"], cwd=ROOT,
                                env={**os.environ, "PG_ADMIN_TOKEN": TOKEN},
                                capture_output=True, text=True, timeout=5)
        self.assertNotIn(TOKEN, result.stdout + result.stderr)
        self.assertEqual(result.stdout.strip(), "python3 scripts/admin_reload.py")


if __name__ == "__main__":
    unittest.main()
