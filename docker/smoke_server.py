"""Smoke test of a HexDB image's server: python3 docker/smoke_server.py <image> [platform] [--slim].

Starts the image (on the given platform, emulated if need be), signs in, and runs a script
function in each runtime through the API, the way users run them; with --slim (the image without
script runtimes), checks that script functions are refused instead. Exits non-zero on failure,
printing the server's log. Standard library only.
"""

import base64
import json
import os
import secrets
import subprocess
import sys
import time
import urllib.error
import urllib.request

SCRIPTS = {
    "python": 'import json, sys\nparams = json.load(sys.stdin)["params"]\nprint(json.dumps({"doubled": params["n"] * 2}))\n',
    "javascript": 'let s = ""\nprocess.stdin.on("data", (d) => (s += d)).on("end", () => {\n  const { params } = JSON.parse(s)\n  console.log(JSON.stringify({ doubled: params.n * 2 }))\n})\n',
    "typescript": 'let s: string = ""\nprocess.stdin.on("data", (d: string) => (s += d)).on("end", () => {\n  const { params } = JSON.parse(s) as { params: { n: number } }\n  console.log(JSON.stringify({ doubled: params.n * 2 }))\n})\n',
}


def main() -> int:
    args = [a for a in sys.argv[1:] if a != "--slim"]
    slim = "--slim" in sys.argv
    image = args[0]
    platform = args[1] if len(args) > 1 else "linux/amd64"
    name = f"hexdb-smoke-{os.getpid()}"
    password = "Smoke-" + secrets.token_urlsafe(18)
    key = "base64:" + base64.b64encode(secrets.token_bytes(32)).decode()
    subprocess.run(
        ["docker", "run", "-d", "--name", name, "--platform", platform, "-p", "127.0.0.1::7700",
         "-e", f"HEXDB_STORAGE__ENCRYPTION_KEY={key}", "-e", f"HEXDB_SECURITY__ADMIN_PASSWORD={password}", image],
        check=True, stdout=subprocess.DEVNULL,
    )
    try:
        port = subprocess.run(["docker", "port", name, "7700/tcp"], check=True, capture_output=True, text=True).stdout.split()[0].rsplit(":", 1)[1]
        url = f"http://127.0.0.1:{port}"

        def call(method: str, path: str, body=None, token=None, timeout=120):
            request = urllib.request.Request(url + path, method=method, data=None if body is None else json.dumps(body).encode())
            request.add_header("Content-Type", "application/json")
            if token:
                request.add_header("Authorization", f"Bearer {token}")
            with urllib.request.urlopen(request, timeout=timeout) as response:
                return json.loads(response.read() or b"null")

        # Emulated (another architecture), a start takes a while.
        deadline = time.time() + 300
        while True:
            try:
                call("GET", "/health", timeout=5)
                break
            except (urllib.error.URLError, ConnectionError, OSError):
                if time.time() > deadline:
                    raise RuntimeError("the server didn't start")
                time.sleep(2)
        # The first administrator's password works once (2.6): that sign-in must choose a new one.
        try:
            call("POST", "/auth/login", {"login": "hexdbadmin", "password": password, "return_token": True})
            raise RuntimeError("the first password signed in without choosing a new one")
        except urllib.error.HTTPError as e:
            if e.code != 403 or json.loads(e.read() or b"{}").get("error", {}).get("code") != "password_change_required":
                raise
        chosen = "Smoke-chosen-" + secrets.token_urlsafe(18)
        token = call("POST", "/auth/login", {"login": "hexdbadmin", "password": password, "new_password": chosen, "return_token": True})["token"]
        if slim:
            function = {"name": "smoke_slim", "kind": "script", "runtime": "python", "code": SCRIPTS["python"]}
            try:
                call("POST", "/functions", function, token)
                raise RuntimeError("the slim image accepted a script function")
            except urllib.error.HTTPError as e:
                if e.code != 400:
                    raise
            print(f"server smoke test passed: {image} ({platform}, slim)")
            return 0
        for runtime, code in SCRIPTS.items():
            function = {"name": f"smoke_{runtime}", "kind": "script", "runtime": runtime, "timeout_seconds": 120,
                        "params": [{"name": "n", "type": "integer"}], "code": code}
            call("POST", "/functions", function, token)
            result = call("POST", f"/functions/smoke_{runtime}/run", {"params": {"n": 21}}, token, timeout=300)["result"]
            if result != {"doubled": 42}:
                raise RuntimeError(f"{runtime} returned {result!r}")
            print(f"{platform} {runtime}: ok")
        print(f"server smoke test passed: {image} ({platform})")
        return 0
    except Exception as e:  # noqa: BLE001 - any failure fails the smoke test, with the server's log
        print(f"server smoke test failed ({platform}): {e}", file=sys.stderr)
        subprocess.run(["docker", "logs", "--tail", "60", name])
        return 1
    finally:
        subprocess.run(["docker", "rm", "-f", name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if __name__ == "__main__":
    sys.exit(main())
