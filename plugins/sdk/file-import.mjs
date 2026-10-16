// Shared by the file import plugins (sources/*-files): copy files from
// somewhere else into a HexDB files tessellation, keeping their folders.
//
//   await importFiles({ name: "s3", list, open })
//
// `list()` yields what's at the source: { key, path, version, size?, modified? }
//   key       a stable identifier (an object key, a file ID)
//   path      where it goes, relative to TARGET_FOLDER: "photos/2024/a.jpg"
//   version   changes when the file does (an ETag, a revision); a changed
//             version is uploaded again, as a new version in HexDB
// `open(item)` returns { body, contentType? }: a web ReadableStream, a Node
// stream, or a Buffer.
//
// Environment (from plugin.toml's [env]):
//   TARGET         the files tessellation (the plugin's [access] must grant it)
//   TARGET_FOLDER  folder in HexDB to import under (default "/")
//   POLL_SECONDS   look again this often; 0: import once, then stay idle
//
// What's been imported is remembered in <name>-import-state.json in the
// plugin's folder (key -> version), so a restart doesn't copy anything twice.
// Uploads send their size, so a user's quota refuses them before any bytes
// move; a refused or failed file is logged and tried again on the next pass.

import { readFileSync, renameSync, writeFileSync } from "node:fs"
import { Readable } from "node:stream"

import { log, warn } from "./hexdb-plugin.mjs"

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))

function encodePath(path) {
  return path
    .split("/")
    .filter((p) => p.length > 0)
    .map(encodeURIComponent)
    .join("/")
}

/** Upload one file (streamed) to HexDB as the plugin's user. */
export async function uploadFile({ base = process.env.HEXDB_API, key = process.env.HEXDB_API_KEY, tessellation, path, body, contentType, size, modified }) {
  if (!base || !key) throw new Error("HEXDB_API and HEXDB_API_KEY are needed: add an [access] section to plugin.toml")
  const query = modified ? `?modified=${encodeURIComponent(new Date(modified).toISOString())}` : ""
  const stream = Buffer.isBuffer(body) ? body : body instanceof ReadableStream ? body : Readable.toWeb(body)
  const headers = { Authorization: `Bearer ${key}`, Accept: "application/json" }
  if (contentType) headers["Content-Type"] = contentType
  if (Number.isFinite(size)) headers["Content-Length"] = String(size)
  const response = await fetch(`${base}/${encodeURIComponent(tessellation)}/_files/${encodePath(path)}${query}`, {
    method: "PUT",
    headers,
    body: stream,
    duplex: "half",
  })
  const text = await response.text()
  const json = text ? JSON.parse(text) : undefined
  if (!response.ok) {
    const error = new Error(json?.error?.message ?? `${response.status} ${response.statusText}`)
    error.status = response.status
    error.code = json?.error?.code
    throw error
  }
  return json
}

/** Copy what `list()` finds into TARGET, now and every POLL_SECONDS. */
export async function importFiles({ name, list, open }) {
  const target = process.env.TARGET
  if (!target) throw new Error("Set TARGET (a files tessellation) in plugin.toml")
  const folder = (process.env.TARGET_FOLDER || "/").replace(/^\/+|\/+$/g, "")
  const pollMs = Number(process.env.POLL_SECONDS ?? 300) * 1000
  const stateFile = `${name}-import-state.json`
  let state
  try {
    state = JSON.parse(readFileSync(stateFile, "utf8"))
  } catch {
    state = { imported: {} }
  }
  const save = () => {
    // Write then rename, so a crash never leaves a half-written state.
    writeFileSync(`${stateFile}.tmp`, JSON.stringify(state))
    renameSync(`${stateFile}.tmp`, stateFile)
  }
  log(`importing ${name} -> ${target}/${folder}`)

  for (;;) {
    let copied = 0
    let failed = 0
    for await (const item of list()) {
      if (state.imported[item.key] === item.version) continue
      const path = folder ? `${folder}/${item.path}` : item.path
      try {
        const { body, contentType } = await open(item)
        await uploadFile({ tessellation: target, path, body, contentType, size: item.size, modified: item.modified })
        state.imported[item.key] = item.version
        save()
        copied++
      } catch (e) {
        failed++
        warn(`couldn't import ${item.path}: ${e.message}`)
        if (e.code === "quota_exceeded") {
          warn("the plugin's user is over its upload quota; stopping this pass")
          break
        }
      }
    }
    if (copied || failed) log(`${name}: ${copied} file(s) imported${failed ? `, ${failed} failed (tried again next pass)` : ""}`)
    if (pollMs <= 0) {
      log(`${name}: done (POLL_SECONDS is 0)`)
      // Stay alive: HexDB restarts plugins that exit.
      await new Promise(() => {})
    }
    await sleep(pollMs)
  }
}
