// Starts a throwaway HexDB server for driver tests and gives back its URL and
// an admin API key. Uses target/debug/hexdb_api (build it with
// `cargo build -p hexdb_api`), or HEXDB_SERVER_BIN.
//
//   import { startServer } from "../../testing/server.mjs"
//   const server = await startServer()
//   ... server.url, server.apiKey ...
//   await server.stop()
//
// Run as a script, it starts a server, runs the given command with HEXDB_URL
// and HEXDB_API_KEY set, and stops the server:
//   node drivers/testing/server.mjs python -m unittest discover -s drivers/python/tests
import { spawn } from "node:child_process"
import { mkdtempSync, rmSync, writeFileSync } from "node:fs"
import { createServer } from "node:net"
import { tmpdir } from "node:os"
import { dirname, join, resolve } from "node:path"
import { fileURLToPath } from "node:url"

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..")
const exe = process.platform === "win32" ? "hexdb_api.exe" : "hexdb_api"

function freePort() {
  return new Promise((resolvePort, reject) => {
    const server = createServer()
    server.on("error", reject)
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address()
      server.close(() => resolvePort(port))
    })
  })
}

export async function startServer() {
  const dir = mkdtempSync(join(tmpdir(), "hexdb-driver-test-"))
  const [api, discovery] = [await freePort(), await freePort()]
  const password = "Driver test passphrase 2026"
  writeFileSync(
    join(dir, "hexdb.toml"),
    `[network]
api_endpoint = "127.0.0.1:${api}"
discovery_endpoint = "127.0.0.1:${discovery}"
lattice_name = "driver-tests-${api}"
scan_local_ports = false

[storage]
path = "./data"
encryption_key = "base64:AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8="

[ui]
path = "./no-ui"

[security]
admin_login = "admin"
admin_password = "${password}"
admin_email = "admin@example.com"
`,
  )
  const bin = process.env.HEXDB_SERVER_BIN || join(root, "target", "debug", exe)
  const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith("HEXDB_")))
  const child = spawn(bin, ["--config", join(dir, "hexdb.toml")], { cwd: dir, env: { ...env, RUST_LOG: "warn" }, stdio: "ignore" })
  const url = `http://127.0.0.1:${api}`
  const deadline = Date.now() + 30_000
  for (;;) {
    try {
      if ((await fetch(`${url}/health`)).ok) break
    } catch {
      // not up yet
    }
    if (Date.now() > deadline || child.exitCode !== null) throw new Error(`HexDB didn't start (${bin})`)
    await new Promise((r) => setTimeout(r, 200))
  }
  const login = await (await fetch(`${url}/auth/login`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ login: "admin", password, return_token: true }) })).json()
  const key = await (await fetch(`${url}/auth/keys`, { method: "POST", headers: { "content-type": "application/json", authorization: `Bearer ${login.token}` }, body: JSON.stringify({ name: "driver tests" }) })).json()
  return {
    url,
    apiKey: key.key,
    adminLogin: "admin",
    adminPassword: password,
    async stop() {
      child.kill()
      await new Promise((r) => setTimeout(r, 300))
      rmSync(dir, { recursive: true, force: true })
    },
  }
}

// As a script: run a command against a fresh server.
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [command, ...args] = process.argv.slice(2)
  const server = await startServer()
  const code = await new Promise((resolveCode) => {
    const child = spawn(command, args, { stdio: "inherit", env: { ...process.env, HEXDB_URL: server.url, HEXDB_API_KEY: server.apiKey }, shell: process.platform === "win32" })
    child.on("exit", (c) => resolveCode(c ?? 1))
  })
  await server.stop()
  process.exit(code)
}
