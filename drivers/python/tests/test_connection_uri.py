"""Connection URIs: the shared test vectors, and HexDB(uri) signing in. No server needed."""

import http.server
import json
import os
import threading
import unittest

from hexdb import ConnectionUri, HexDB, parse_connection_uri

VECTORS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "testing", "connection-uris.json")

# A word from each vector's reason that the error message must contain.
NAMED = {
    "mongodb://localhost:27017": "hexdb://",
    "hexdb://": "no host",
    "hexdb://:7700": "no host",
    "hexdb://ada:pw@localhost?apiKey=hxk_1": "login and apiKey",
    "hexdb://localhost?timeout=abc": "timeout",
    "hexdb://localhost?timeout=0": "timeout",
    "hexdb://localhost?pageSize=0": "pageSize",
    "hexdb://localhost?pageSize=10001": "pageSize",
    "hexdb://localhost:0": "port",
    "hexdb://localhost:65536": "port",
    "hexdb://localhost:abc": "port",
    "hexdb://a.example.com,b.example.com": "more than one host",
    "hexdb://localhost#section": "fragment",
    "hexdb://localhost?apiKey=": "apiKey",
    "hexdb:localhost": "//",
}


def load_vectors():
    with open(VECTORS, encoding="utf-8") as f:
        return json.load(f)


class VectorTests(unittest.TestCase):
    def test_valid(self):
        for v in load_vectors()["valid"]:
            with self.subTest(uri=v["uri"]):
                self.assertEqual(
                    parse_connection_uri(v["uri"]),
                    ConnectionUri(
                        url=v["url"],
                        login=v["login"],
                        password=v["password"],
                        api_key=v["apiKey"],
                        timeout_seconds=v["timeoutSeconds"],
                        page_size=v["pageSize"],
                        ca_file=v["caFile"],
                    ),
                )

    def test_invalid(self):
        for v in load_vectors()["invalid"]:
            with self.subTest(uri=v["uri"], reason=v["reason"]):
                with self.assertRaises(ValueError) as caught:
                    parse_connection_uri(v["uri"])
                self.assertIn(NAMED[v["uri"]], str(caught.exception))

    def test_bad_percent_encoding(self):
        with self.assertRaises(ValueError):
            parse_connection_uri("hexdb://ada:p%4@localhost")

    def test_plain_urls_unchanged(self):
        db = HexDB("http://127.0.0.1:7700/", "hxk_x")
        self.assertEqual((db.base, db.token, db.timeout), ("http://127.0.0.1:7700", "hxk_x", 30.0))

    def test_uri_options_and_explicit_arguments(self):
        db = HexDB("hexdbs://db.example.com/hexdb/?apiKey=hxk_uri&timeout=7")
        self.assertEqual((db.base, db.token, db.timeout), ("https://db.example.com/hexdb", "hxk_uri", 7))
        db = HexDB("hexdbs://db.example.com?apiKey=hxk_uri&timeout=7", api_key="hxk_arg", timeout=3)
        self.assertEqual((db.token, db.timeout), ("hxk_arg", 3))


class SignInTests(unittest.TestCase):
    """HexDB(uri) with a login signs in with POST /auth/login."""

    def test_login_in_uri_signs_in(self):
        seen = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                seen.append((self.path, body))
                answer = json.dumps({"token": "sess_1", "user": {"login": body["login"]}, "expires_at": "later"}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(answer)))
                self.end_headers()
                self.wfile.write(answer)

            def log_message(self, *args):
                pass

        httpd = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=httpd.serve_forever, daemon=True)
        thread.start()
        try:
            port = httpd.server_address[1]
            db = HexDB(f"hexdb://ada:p%40ss@127.0.0.1:{port}/base")
        finally:
            httpd.shutdown()
            httpd.server_close()
            thread.join()
        self.assertEqual(seen, [("/base/auth/login", {"login": "ada", "password": "p@ss", "return_token": True})])
        self.assertEqual(db.token, "sess_1")
        self.assertEqual(db.base, f"http://127.0.0.1:{port}/base")


if __name__ == "__main__":
    unittest.main()
