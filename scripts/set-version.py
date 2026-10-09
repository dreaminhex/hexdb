#!/usr/bin/env python3
"""Set HexDB's version everywhere a release reads it.

    python scripts/set-version.py 1.0.1

Updates the Rust crates (and Cargo.lock), the Node, Python and .NET drivers,
the admin UI, the OpenAPI description, the package manifests (Homebrew,
Chocolatey, winget), the example plugin and the benchmark report. Each file
keeps its own line endings. Then commit, and tag with the same version:

    git tag v1.0.1 && git push origin main v1.0.1
"""
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CRATES = ["hexdb_core", "hexdb_api", "hexdb_cli", "hexdb_query", "hexdb_tests", "bench", "drivers/odbc"]
CRLF, LF = "\r\n", "\n"


def read(path: Path) -> tuple[str, str]:
    """The text with LF line endings, and the line ending the file uses (kept on write)."""
    raw = path.read_bytes().decode("utf-8")
    return raw.replace(CRLF, LF), (CRLF if CRLF in raw else LF)


def write(path: Path, text: str, eol: str) -> None:
    path.write_bytes(text.replace(LF, eol).encode("utf-8"))


def sub(rel: str, pattern: str, repl: str, count: int = 0, flags: int = re.M) -> None:
    path = ROOT / rel
    text, eol = read(path)
    new, n = re.subn(pattern, repl, text, count=count, flags=flags)
    if n == 0:
        sys.exit(f"{rel}: nothing matched {pattern!r}")
    if new != text:
        write(path, new, eol)
    print(f"  {rel}")


def set_json_version(rel: str, version: str, lock: bool = False) -> None:
    path = ROOT / rel
    text, eol = read(path)
    data = json.loads(text)
    data["version"] = version
    if lock and "" in data.get("packages", {}):
        data["packages"][""]["version"] = version
    new = json.dumps(data, indent=2, ensure_ascii=False) + LF
    if new != text:
        write(path, new, eol)
    print(f"  {rel}")


def main() -> None:
    if len(sys.argv) != 2 or not re.fullmatch(r"\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?", sys.argv[1].lstrip("v")):
        sys.exit(__doc__)
    v = sys.argv[1].lstrip("v")
    print(f"Setting the version to {v}:")

    for crate in CRATES:
        sub(f"{crate}/Cargo.toml", r'^version = "[^"]+"$', f'version = "{v}"', count=1)
    set_json_version("drivers/node/package.json", v)
    set_json_version("drivers/node/package-lock.json", v, lock=True)
    set_json_version("hexdb_admin/package.json", v)
    set_json_version("hexdb_admin/package-lock.json", v, lock=True)
    sub("drivers/python/pyproject.toml", r'^version = "[^"]+"$', f'version = "{v}"', count=1)
    sub("drivers/python/hexdb/__init__.py", r'^__version__ = "[^"]+"$', f'__version__ = "{v}"', count=1)
    for proj in ["HexDB.Client", "HexDB.EntityFrameworkCore"]:
        sub(f"drivers/dotnet/{proj}/{proj}.csproj", r"<Version>[^<]+</Version>", f"<Version>{v}</Version>", count=1)
    sub("hexdb_api/openapi.json", r'("info": \{[^}]*?"version": )"[^"]+"', rf'\g<1>"{v}"', count=1, flags=re.S)
    sub("scripts/openapi.py", r'("version": )"[^"]+"', rf'\g<1>"{v}"', count=1)
    sub("packaging/homebrew/hexdb.rb", r'^  version "[^"]+"$', f'  version "{v}"', count=1)
    sub("packaging/chocolatey/hexdb.nuspec", r"<version>[^<]+</version>", f"<version>{v}</version>", count=1)
    sub("packaging/chocolatey/tools/chocolateyinstall.ps1", r"download/v[^/]+/", f"download/v{v}/")
    for f in ["DreamInHex.HexDB.yaml", "DreamInHex.HexDB.installer.yaml", "DreamInHex.HexDB.locale.en-US.yaml"]:
        sub(f"packaging/winget/{f}", r"^PackageVersion: .*$", f"PackageVersion: {v}")
    sub("packaging/winget/DreamInHex.HexDB.installer.yaml", r"download/v[^/]+/", f"download/v{v}/")
    sub("packaging/winget/DreamInHex.HexDB.locale.en-US.yaml", r"tag/v\S+", f"tag/v{v}")
    sub("plugins/examples/change-logger/plugin.toml", r'^version = "[^"]+"$', f'version = "{v}"', count=1)
    sub("bench/results/windows-full.md", r"HexDB \d+\.\d+\.\d+\S*, release build", f"HexDB {v}, release build", count=1)

    print("Updating Cargo.lock...")
    subprocess.run(["cargo", "update", "--workspace", "--quiet"], cwd=ROOT, check=True)
    print(f"Done. Commit, then: git tag v{v} && git push origin main v{v}")


if __name__ == "__main__":
    main()
