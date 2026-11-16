#!/bin/sh
# Smoke test of a built HexDB image (docker/smoke.sh <image>): the programs it
# ships run, including every script runtime the way HexDB starts them
# (functions.rs: python3; node; node --experimental-strip-types for TypeScript).
set -eu
image="$1"
run() { docker run --rm --entrypoint sh "$image" -c "$1"; }

run 'hexdb_api --version && test -x /usr/local/bin/hexdb && test -s /usr/share/doc/nodejs/LICENSE && ffmpeg -hide_banner -version | head -1 && tesseract --version | head -1'
run 'python3 -c "import json, sys; print(json.dumps({\"python\": sys.version.split()[0]}))"'
run 'node -e "console.log(JSON.stringify({ node: process.version }))"'
# TypeScript functions need type stripping (Node.js 22.6 or later).
run 'cd /tmp && printf "const n: number = 21\nconsole.log(JSON.stringify({ typescript: n * 2 }))\n" > t.mts && node --experimental-strip-types --no-warnings t.mts' | grep -q '"typescript":42'
echo "smoke test passed: $image"
