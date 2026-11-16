// HexDB plugin helpers for Node.js (18 or later). No dependencies.
//
//   import { payloads, api, log } from "../../sdk/hexdb-plugin.mjs"
//
//   for await (const change of payloads()) { ... }   // stream, logs and metrics plugins
//   ack(change)                                         // stream plugins with acks = true
//   await api().post("/orders", { total: 12 })          // plugins with [access] in plugin.toml
//
// HexDB starts the plugin in its folder with these environment variables:
//   HEXDB_PLUGIN_ID     the plugin's id
//   HEXDB_PLUGIN_TYPE   stream, logs, metrics or source
//   HEXDB_API           the server's base URL
//   HEXDB_API_KEY       an API key for the plugin's own user (only with [access])
//   HEXDB_PLUGIN_DATA   a folder the plugin can write and that survives restarts
//                       (the sandbox lets it write nowhere else unless plugin.toml
//                       grants it); TMPDIR is inside it
//   NODE_EXTRA_CA_CERTS the server's CA file, when the lattice uses private certificates
// plus whatever the manifest's [env] sets. Anything printed goes to the HexDB log.

import { randomUUID } from "node:crypto"
import { readFileSync, renameSync, writeFileSync } from "node:fs"
import { join } from "node:path"
import { createInterface } from "node:readline"

/** The path of a file in the plugin's data folder (HEXDB_PLUGIN_DATA, else its own folder). */
export function dataFile(name) {
  return join(process.env.HEXDB_PLUGIN_DATA || ".", name)
}

/** Read a JSON state file from the data folder; one left in the plugin's folder by HexDB 2.4 or earlier is read too. */
export function readState(name, fallback) {
  for (const path of [dataFile(name), name]) {
    try {
      return JSON.parse(readFileSync(path, "utf8"))
    } catch {
      // Missing or unreadable: try the next place.
    }
  }
  return fallback
}

/** Save a JSON state file in the data folder (written, then renamed, so a crash never leaves half of it). */
export function saveState(name, state) {
  const path = dataFile(name)
  writeFileSync(`${path}.tmp`, JSON.stringify(state))
  renameSync(`${path}.tmp`, path)
}

/** Each payload HexDB sends on stdin (one JSON object per line), in order. */
export async function* payloads() {
  for await (const line of createInterface({ input: process.stdin, crlfDelay: Infinity })) {
    if (line.trim()) yield JSON.parse(line)
  }
}

/**
 * Tell HexDB that a change (and every change before it) is safely at its destination.
 * For stream plugins with `acks = true` in plugin.toml: HexDB saves the plugin's
 * position only up to what it acknowledged, so after a crash or restart the plugin
 * gets every unacknowledged change again (deduplicate on `seq`). Call it after the
 * destination confirmed the write, in order; acknowledging a later change covers
 * the earlier ones. Takes the change or its `seq`.
 */
export function ack(change) {
  const seq = typeof change === "number" ? change : change?.seq
  if (Number.isSafeInteger(seq)) process.stdout.write(`{"ack":${seq}}\n`)
}

/** Log a line (it shows up on the HexDB Logs page, tagged with the plugin id). */
export function log(...parts) {
  console.log(...parts)
}

/** Log an error line (shown as a warning in HexDB). */
export function warn(...parts) {
  console.error(...parts)
}

export class HexDBError extends Error {
  constructor(status, code, message) {
    super(message)
    this.status = status
    this.code = code
  }
}

/**
 * A small client for the HexDB REST API, acting as the plugin's user.
 *
 * Retries are safe: every document write and transaction carries an
 * `Idempotency-Key`, the same on every try of that call, so HexDB applies it
 * once and answers a retry with the first result (see idempotentRoute). A 429 (refused before
 * anything was done) is retried for any request, honoring Retry-After; an
 * uncertain answer (502, 503, a dropped connection, a timeout) is retried for
 * reads (and queries), and for writes only when they carry a key. Give your
 * own key to make a write safe to repeat across process restarts too, or
 * `{ "Idempotency-Key": "" }` in its headers to send none (it then isn't
 * retried after an uncertain answer).
 */
// API routes that aren't tessellations (hexdb_core::catalog's reserved names and route names).
const ROUTES = new Set(
  ("health status flush shutdown tessellation tessellations ui wal graphql logs changes indexes auth transactions plugins lattice " +
    "replication compact audit settings join streams functions schedules analyzers schemas openapi backup backups triggers sql scrub " +
    "config ai deletes blocklist queues shares mcp quarantine users roles updates a2a ready security retire").split(" "),
)

/**
 * True if the server applies a write to `path` once per Idempotency-Key, so sending it again after
 * an uncertain answer is safe: document writes (insert, bulk, upsert, update and delete by filter,
 * replace, patch, delete) and transactions. Other writes (queue sends, stream publishes, function
 * runs, administration) aren't retried after an uncertain answer.
 */
export function idempotentRoute(method, path) {
  const parts = path.split("?")[0].split("/").filter(Boolean).map(decodeURIComponent)
  if (parts.length === 1 && parts[0] === "transactions") return method === "POST"
  if (parts.length === 0 || parts.length > 2 || ROUTES.has(parts[0].toLowerCase()) || parts[0].startsWith("_")) return false
  if (parts.length === 1) return method === "POST"
  if (["_bulk", "_upsert", "_update", "_delete"].includes(parts[1])) return method === "POST"
  return !parts[1].startsWith("_") && ["PUT", "PATCH", "DELETE"].includes(method)
}

export function api({ base = process.env.HEXDB_API, key = process.env.HEXDB_API_KEY, retries = 4, timeoutMs = 60_000 } = {}) {
  if (!base) throw new Error("HEXDB_API is not set (is this running as a HexDB plugin?)")
  if (!key) throw new Error("HEXDB_API_KEY is not set: add an [access] section to plugin.toml")

  const delay = (response, attempt) => {
    const after = Number(response?.headers.get("retry-after"))
    return (Number.isFinite(after) && after >= 0 ? after : 2 ** attempt) * 1000
  }

  async function request(method, path, body, headers = {}) {
    // Queries are reads sent as POST.
    const write = !["GET", "HEAD"].includes(method) && !path.split("?")[0].endsWith("/_query")
    const keyed = write && idempotentRoute(method, path)
    const given = Object.keys(headers).find((h) => h.toLowerCase() === "idempotency-key")
    const extra = { ...headers }
    if (keyed && given === undefined) extra["Idempotency-Key"] = randomUUID()
    if (given !== undefined && !extra[given]) delete extra[given]
    // A write whose answer is lost may have happened: it's sent again only where the server applies it
    // once per key, and with a key.
    const retryUncertain = !write || (keyed && Object.keys(extra).some((h) => h.toLowerCase() === "idempotency-key"))
    for (let attempt = 0; ; attempt++) {
      let response
      try {
        response = await fetch(base + path, {
          method,
          headers: { Authorization: `Bearer ${key}`, Accept: "application/json", ...(body === undefined ? {} : { "Content-Type": "application/json" }), ...extra },
          body: body === undefined ? undefined : JSON.stringify(body),
          signal: AbortSignal.timeout(timeoutMs),
        })
      } catch (e) {
        // No answer (a connection error or a timeout): whether a write happened is unknown.
        if (retryUncertain && attempt < retries) {
          await new Promise((resolve) => setTimeout(resolve, 2 ** attempt * 1000))
          continue
        }
        throw e
      }
      const retry = response.status === 429 || (retryUncertain && [502, 503].includes(response.status))
      if (retry && attempt < retries) {
        await response.body?.cancel()
        await new Promise((resolve) => setTimeout(resolve, delay(response, attempt)))
        continue
      }
      const text = await response.text()
      const json = text ? JSON.parse(text) : undefined
      if (!response.ok) {
        throw new HexDBError(response.status, json?.error?.code ?? "error", json?.error?.message ?? text ?? response.statusText)
      }
      return json
    }
  }

  const enc = encodeURIComponent
  const keyed = (idempotencyKey) => (idempotencyKey ? { "Idempotency-Key": idempotencyKey } : {})
  return {
    request,
    get: (path) => request("GET", path),
    post: (path, body, headers) => request("POST", path, body, headers),
    put: (path, body, headers) => request("PUT", path, body, headers),
    patch: (path, body, headers) => request("PATCH", path, body, headers),
    delete: (path, headers) => request("DELETE", path, undefined, headers),
    /** Insert one document; returns it with its new `id`. */
    insert: (tessellation, doc, idempotencyKey) => request("POST", `/${enc(tessellation)}`, doc, keyed(idempotencyKey)),
    /** Insert up to 1000 documents at once. */
    insertMany: (tessellation, docs, idempotencyKey) => request("POST", `/${enc(tessellation)}/_bulk`, docs, keyed(idempotencyKey)),
    /** Replace an existing document by ID. */
    replace: (tessellation, id, doc, idempotencyKey) => request("PUT", `/${enc(tessellation)}/${enc(id)}`, doc, keyed(idempotencyKey)),
    /**
     * Insert or replace documents by key fields (e.g. the ID in the source
     * system): `upsert("orders", ["source_id"], rows)`. Up to 1000 at a time.
     */
    upsert: (tessellation, key, docs, idempotencyKey) =>
      request("POST", `/${enc(tessellation)}/_upsert`, { key, documents: docs }, keyed(idempotencyKey)),
    query: (tessellation, query) => request("POST", `/${enc(tessellation)}/_query`, query),
  }
}

/** Run `fn` until the process is asked to stop; logs and rethrows errors. */
export async function main(fn) {
  try {
    await fn()
  } catch (e) {
    warn(e instanceof Error ? e.stack ?? e.message : String(e))
    process.exit(1)
  }
}
