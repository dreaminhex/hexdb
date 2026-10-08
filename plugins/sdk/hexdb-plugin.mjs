// HexDB plugin helpers for Node.js (18 or later). No dependencies.
//
//   import { payloads, api, log } from "../../sdk/hexdb-plugin.mjs"
//
//   for await (const change of payloads()) { ... }   // stream, logs and metrics plugins
//   await api().post("/orders", { total: 12 })          // plugins with [access] in plugin.toml
//
// HexDB starts the plugin in its folder with these environment variables:
//   HEXDB_PLUGIN_ID     the plugin's id
//   HEXDB_PLUGIN_TYPE   stream, logs, metrics or source
//   HEXDB_API           the server's base URL
//   HEXDB_API_KEY       an API key for the plugin's own user (only with [access])
//   NODE_EXTRA_CA_CERTS the server's CA file, when the lattice uses private certificates
// plus whatever the manifest's [env] sets. Anything printed goes to the HexDB log.

import { createInterface } from "node:readline"

/** Each payload HexDB sends on stdin (one JSON object per line), in order. */
export async function* payloads() {
  for await (const line of createInterface({ input: process.stdin, crlfDelay: Infinity })) {
    if (line.trim()) yield JSON.parse(line)
  }
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
 * Retries 429/502/503 responses a few times, honoring Retry-After.
 */
export function api({ base = process.env.HEXDB_API, key = process.env.HEXDB_API_KEY, retries = 4 } = {}) {
  if (!base) throw new Error("HEXDB_API is not set (is this running as a HexDB plugin?)")
  if (!key) throw new Error("HEXDB_API_KEY is not set: add an [access] section to plugin.toml")

  async function request(method, path, body, headers = {}) {
    for (let attempt = 0; ; attempt++) {
      const response = await fetch(base + path, {
        method,
        headers: { Authorization: `Bearer ${key}`, Accept: "application/json", ...(body === undefined ? {} : { "Content-Type": "application/json" }), ...headers },
        body: body === undefined ? undefined : JSON.stringify(body),
      })
      if ([429, 502, 503].includes(response.status) && attempt < retries) {
        const wait = Number(response.headers.get("retry-after")) || 2 ** attempt
        await new Promise((resolve) => setTimeout(resolve, wait * 1000))
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
  return {
    request,
    get: (path) => request("GET", path),
    post: (path, body, headers) => request("POST", path, body, headers),
    put: (path, body, headers) => request("PUT", path, body, headers),
    patch: (path, body, headers) => request("PATCH", path, body, headers),
    delete: (path, headers) => request("DELETE", path, undefined, headers),
    /** Insert one document; returns it with its new `id`. */
    insert: (tessellation, doc) => request("POST", `/${enc(tessellation)}`, doc),
    /** Insert up to 1000 documents at once. An idempotency key makes retries safe. */
    insertMany: (tessellation, docs, idempotencyKey) =>
      request("POST", `/${enc(tessellation)}/_bulk`, docs, idempotencyKey ? { "Idempotency-Key": idempotencyKey } : {}),
    /** Replace an existing document by ID. */
    replace: (tessellation, id, doc) => request("PUT", `/${enc(tessellation)}/${enc(id)}`, doc),
    /**
     * Insert or replace documents by key fields (e.g. the ID in the source
     * system): `upsert("orders", ["source_id"], rows)`. Up to 1000 at a time.
     */
    upsert: (tessellation, key, docs, idempotencyKey) =>
      request("POST", `/${enc(tessellation)}/_upsert`, { key, documents: docs }, idempotencyKey ? { "Idempotency-Key": idempotencyKey } : {}),
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
