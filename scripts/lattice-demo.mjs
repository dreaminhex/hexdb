#!/usr/bin/env node
// A small HexDB lattice on this machine, to see replication and failover in
// the admin UI. Starts several hexes on their own ports (7800, 7810, ...) in
// .hexdb-demo/ (git-ignored), loads sample data, and prints a link to each
// hex's UI. Your development server on 7700 is left alone.
//
//   node scripts/lattice-demo.mjs                 three hexes
//   node scripts/lattice-demo.mjs --hexes 5       up to seven
//   node scripts/lattice-demo.mjs --port 9000     other ports
//   node scripts/lattice-demo.mjs --keep          reuse the data of the last run
//
// Needs a built server (cargo build --release -p hexdb_api, or a debug build)
// and the built admin UI (npm run build in hexdb_admin). While it runs, type:
//
//   stop N      stop hex N gracefully (stop the Overseer to watch a failover)
//   start N     start it again (it rejoins and catches up)
//   list        the lattice as hex 1 (or the first running hex) sees it
//   quit        stop every hex (Ctrl+C does too)

import { spawn } from "node:child_process"
import { randomBytes } from "node:crypto"
import { existsSync, mkdirSync, openSync, readFileSync, rmSync, writeFileSync } from "node:fs"
import { dirname, join, resolve } from "node:path"
import { createInterface } from "node:readline"
import { fileURLToPath } from "node:url"

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..")
const args = process.argv.slice(2)
const option = (name, fallback) => {
  const i = args.indexOf(name)
  return i >= 0 && args[i + 1] ? args[i + 1] : fallback
}
const count = Math.min(7, Math.max(2, Number(option("--hexes", 3)) || 3))
const basePort = Number(option("--port", 7800)) || 7800
const keep = args.includes("--keep")

function fail(message) {
  console.error(message)
  process.exit(1)
}

const exe = process.platform === "win32" ? ".exe" : ""
const bin = process.env.HEXDB_SERVER_BIN || ["release", "debug"].map((p) => join(root, "target", p, `hexdb_api${exe}`)).find(existsSync)
if (!bin) fail("No server build found. Build one first: cargo build --release -p hexdb_api")
const ui = join(root, "hexdb_admin", "dist")
if (!existsSync(join(ui, "index.html"))) fail("The admin UI isn't built. Build it first: cd hexdb_admin && npm ci && npm run build")

const dir = join(root, ".hexdb-demo")
const settingsFile = join(dir, "demo.json")
if (!keep && existsSync(dir)) rmSync(dir, { recursive: true, force: true })
mkdirSync(dir, { recursive: true })

// One key, lattice secret and admin password per demo (kept with --keep).
const settings = keep && existsSync(settingsFile)
  ? JSON.parse(readFileSync(settingsFile, "utf8"))
  : {
      key: `base64:${randomBytes(32).toString("base64")}`,
      secret: `base64:${randomBytes(32).toString("base64")}`,
      password: `Demo-${randomBytes(12).toString("base64url")}`,
    }
writeFileSync(settingsFile, JSON.stringify(settings, null, 2))

const toml = (s) => JSON.stringify(s) // TOML basic strings use the same escapes as JSON here
const hexes = Array.from({ length: count }, (_, i) => {
  const api = basePort + i * 10
  const discovery = api + 2
  return { n: i + 1, api, discovery, url: `http://127.0.0.1:${api}`, folder: join(dir, `hex-${i + 1}`), child: null }
})

for (const hex of hexes) {
  mkdirSync(hex.folder, { recursive: true })
  const peers = hexes.filter((h) => h !== hex).map((h) => toml(`127.0.0.1:${h.discovery}`))
  writeFileSync(
    join(hex.folder, "hexdb.toml"),
    `# Written by scripts/lattice-demo.mjs.
[network]
api_endpoint = ${toml(`127.0.0.1:${hex.api}`)}
discovery_endpoint = ${toml(`127.0.0.1:${hex.discovery}`)}
lattice_name = "Demo Lattice"
peers = [${peers.join(", ")}]
scan_local_ports = false
discovery_interval_seconds = 2
lattice_secret = ${toml(settings.secret)}

[identity]
# Hex 1 leads at first; the others can take over if it stops.
role = ${toml(hex.n === 1 ? "overseer" : "auto")}

[memory]
ram_mb = 256

[storage]
path = "./data"
encryption_key = ${toml(settings.key)}

[ui]
path = ${toml(ui.replaceAll("\\", "/"))}

[security]
admin_login = "admin"
admin_password = ${toml(settings.password)}
admin_email = "admin@example.com"

[plugins]
enabled = false
`,
  )
}

const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith("HEXDB_")))
env.RUST_LOG = env.RUST_LOG || "info"

function start(hex) {
  if (hex.child) return
  const log = openSync(join(hex.folder, "server.log"), "a")
  hex.child = spawn(bin, ["--config", join(hex.folder, "hexdb.toml")], { cwd: hex.folder, env, stdio: ["ignore", log, log] })
  hex.child.on("exit", () => {
    hex.child = null
  })
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

async function healthy(hex) {
  try {
    return (await fetch(`${hex.url}/health`)).ok
  } catch {
    return false
  }
}

async function waitFor(what, check, seconds = 60) {
  const deadline = Date.now() + seconds * 1000
  while (Date.now() < deadline) {
    if (await check()) return
    await sleep(500)
  }
  throw new Error(`Timed out waiting for ${what}.`)
}

let token = null
async function signIn() {
  for (const hex of hexes.filter((h) => h.child)) {
    try {
      const res = await fetch(`${hex.url}/auth/login`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ login: "admin", password: settings.password, return_token: true }),
      })
      if (res.ok) {
        token = (await res.json()).token
        return
      }
    } catch {
      // try the next hex
    }
  }
}

async function status(hex) {
  const res = await fetch(`${hex.url}/status`, { headers: { authorization: `Bearer ${token}` } })
  if (res.status === 401) {
    await signIn()
    return status(hex)
  }
  return res.ok ? res.json() : null
}

async function list() {
  const viewer = hexes.find((h) => h.child)
  if (!viewer) return console.log("No hex is running.")
  const s = await status(viewer).catch(() => null)
  const seen = new Map((s?.network?.lattice?.hexes ?? []).map((h) => [h.api_endpoint, h]))
  console.log(`\n  ${s?.network?.lattice?.name ?? "Demo Lattice"}, as hex ${viewer.n} sees it:`)
  for (const hex of hexes) {
    const info = seen.get(`127.0.0.1:${hex.api}`)
    const role = hex.child ? (info ? `${info.role}${info.status === "active" ? "" : ` (${info.status})`}` : "starting") : "stopped"
    console.log(`  hex ${hex.n}  ${role.padEnd(22)} ${hex.url}/ui/`)
  }
  console.log()
}

async function shutdown(hex) {
  if (!hex.child) return
  try {
    await fetch(`${hex.url}/shutdown`, { method: "POST", headers: { authorization: `Bearer ${token}` } })
  } catch {
    // already gone
  }
  const deadline = Date.now() + 20_000
  while (hex.child && Date.now() < deadline) await sleep(200)
  hex.child?.kill()
}

let quitting = false
async function quit() {
  if (quitting) return
  quitting = true
  console.log("Stopping every hex...")
  await Promise.all(hexes.map(shutdown))
  console.log(`Stopped. The data is in ${dir} (node scripts/lattice-demo.mjs --keep reuses it).`)
  process.exit(0)
}

// Start everything and wait for one lattice.
console.log(`Starting ${count} hexes (${bin})...`)
hexes.forEach(start)
try {
  await waitFor("the hexes to start", async () => (await Promise.all(hexes.map(healthy))).every(Boolean))
  await waitFor("the first administrator", async () => {
    await signIn()
    return token !== null
  })
  await waitFor(
    "the hexes to find each other",
    async () => (await status(hexes[0]))?.network?.lattice?.hexes?.filter((h) => h.status === "active").length === count,
  )
} catch (e) {
  console.error(`${e.message} See the server.log files in ${dir}.`)
  await quit()
}

if (!keep) {
  console.log("Loading sample data into hex 1...")
  await new Promise((done) => {
    const seed = spawn(process.execPath, [join(root, "scripts", "seed.mjs")], {
      env: { ...env, HEXDB: hexes[0].url, HEXDB_USER: "admin", HEXDB_PASSWORD: settings.password },
      stdio: ["ignore", "ignore", "inherit"],
    })
    seed.on("exit", done)
  })
}

await list()
console.log(`  Sign in as admin with the password ${settings.password}`)
console.log("  Open the Dashboard on any hex: the Lattice card shows every hex, its role and replication lag.")
console.log("  Commands: stop N, start N, list, quit\n")

const rl = createInterface({ input: process.stdin, output: process.stdout, prompt: "lattice> " })
rl.on("SIGINT", () => void quit())
process.on("SIGINT", () => void quit())
rl.prompt()
rl.on("line", async (line) => {
  const [command, arg] = line.trim().split(/\s+/)
  const hex = hexes.find((h) => h.n === Number(arg))
  if (command === "quit" || command === "exit") return quit()
  if (command === "list" || command === "") await list()
  else if ((command === "stop" || command === "start") && !hex) console.log(`Which hex? 1-${count}`)
  else if (command === "stop") {
    await shutdown(hex)
    console.log(`Hex ${hex.n} stopped. Within a few discovery rounds the others notice; if it was the Overseer, one of them takes over.`)
  } else if (command === "start") {
    start(hex)
    await waitFor(`hex ${hex.n}`, () => healthy(hex)).catch((e) => console.log(e.message))
    console.log(`Hex ${hex.n} is back; it rejoins the lattice and catches up from the Overseer.`)
  } else console.log("Commands: stop N, start N, list, quit")
  rl.prompt()
})
