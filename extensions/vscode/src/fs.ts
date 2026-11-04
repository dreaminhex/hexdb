// Functions and documents as files, so they open in ordinary editor tabs and save with Ctrl+S:
//
//   hexdbfs://<connection>/functions/<name>.py|.ts|.js   a script function's code
//   hexdbfs://<connection>/functions/<name>.json         a function's whole definition
//   hexdbfs://<connection>/documents/<tessellation>/<id>.json
//
// Files are read-only when the signed-in user couldn't save them (functions need an administrator).

import { HexDBError } from "@dreaminhex/hexdb"
import * as vscode from "vscode"

import type { Connections } from "./connections"
import { can, runtimeExtension } from "./lib"

export const scheme = "hexdbfs"

interface FunctionDef {
  name: string
  kind: string
  runtime?: string
  code?: string
  updated?: number
  created_by?: string
  [key: string]: unknown
}

type Target =
  | { type: "function-code"; connection: string; name: string }
  | { type: "function-json"; connection: string; name: string }
  | { type: "document"; connection: string; tessellation: string; id: string }

export function functionUri(connection: string, fn: { name: string; kind: string; runtime?: string }, definition = false): vscode.Uri {
  const ext = !definition && fn.kind === "script" ? runtimeExtension(fn.runtime) : ".json"
  return vscode.Uri.from({ scheme, authority: connection, path: `/functions/${fn.name}${ext}` })
}

export function documentUri(connection: string, tessellation: string, id: string): vscode.Uri {
  return vscode.Uri.from({ scheme, authority: connection, path: `/documents/${tessellation}/${id}.json` })
}

function parse(uri: vscode.Uri): Target {
  const parts = uri.path.split("/").filter(Boolean)
  if (parts[0] === "functions" && parts.length === 2) {
    const m = /^(.+?)(\.py|\.ts|\.js|\.json)$/.exec(parts[1])
    if (m) return { type: m[2] === ".json" ? "function-json" : "function-code", connection: uri.authority, name: m[1] }
  }
  if (parts[0] === "documents" && parts.length === 3 && parts[2].endsWith(".json")) {
    return { type: "document", connection: uri.authority, tessellation: parts[1], id: parts[2].slice(0, -5) }
  }
  throw vscode.FileSystemError.FileNotFound(uri)
}

// Fields the server sets itself; left out of the editable definition.
const serverFields = ["name", "created_by", "updated"]

export class HexDBFileSystem implements vscode.FileSystemProvider {
  private readonly emitter = new vscode.EventEmitter<vscode.FileChangeEvent[]>()
  readonly onDidChangeFile = this.emitter.event
  // Last-read times, so VS Code doesn't think a file changed underneath an editor between reads.
  private readonly mtimes = new Map<string, number>()
  private readonly saved = new vscode.EventEmitter<Target>()
  /** Fires after a function is saved (the tree refreshes). */
  readonly onDidSave = this.saved.event

  constructor(private readonly connections: Connections) {}

  watch(): vscode.Disposable {
    return new vscode.Disposable(() => undefined)
  }

  async stat(uri: vscode.Uri): Promise<vscode.FileStat> {
    // The folders: writes check that a file's parent exists.
    const parts = uri.path.split("/").filter(Boolean)
    if (parts.length === 0 || (parts.length === 1 && (parts[0] === "functions" || parts[0] === "documents")) || (parts.length === 2 && parts[0] === "documents")) {
      return { type: vscode.FileType.Directory, ctime: 0, mtime: 0, size: 0, permissions: vscode.FilePermission.Readonly }
    }
    const target = parse(uri)
    const session = await this.connections.session(target.connection)
    const writable = target.type === "document" ? can(session.me, "write", target.tessellation) : session.isAdmin
    const key = uri.toString()
    if (!this.mtimes.has(key)) this.mtimes.set(key, Date.now())
    // The size isn't known without reading; readFile reports a missing file.
    return {
      type: vscode.FileType.File,
      ctime: 0,
      mtime: this.mtimes.get(key) ?? 0,
      size: 0,
      permissions: writable ? undefined : vscode.FilePermission.Readonly,
    }
  }

  async readFile(uri: vscode.Uri): Promise<Uint8Array> {
    const target = parse(uri)
    const session = await this.connections.session(target.connection)
    try {
      if (target.type === "document") {
        const doc = await session.request<Record<string, unknown>>("GET", `/${encodeURIComponent(target.tessellation)}/${encodeURIComponent(target.id)}`)
        return encode(JSON.stringify(doc, null, 2) + "\n")
      }
      const def = await session.request<FunctionDef>("GET", `/functions/${encodeURIComponent(target.name)}`)
      if (target.type === "function-code") return encode(def.code ?? "")
      const editable: Record<string, unknown> = { ...def }
      for (const f of serverFields) delete editable[f]
      return encode(JSON.stringify(editable, null, 2) + "\n")
    } catch (e) {
      if (e instanceof HexDBError && e.status === 404) throw vscode.FileSystemError.FileNotFound(uri)
      throw e
    }
  }

  async writeFile(uri: vscode.Uri, content: Uint8Array): Promise<void> {
    const target = parse(uri)
    const session = await this.connections.session(target.connection)
    const text = new TextDecoder().decode(content)
    if (target.type === "document") {
      const doc = parseJson(text, "The document")
      delete doc.id
      delete doc._expires_at
      await session.request("PUT", `/${encodeURIComponent(target.tessellation)}/${encodeURIComponent(target.id)}`, doc)
    } else {
      const path = `/functions/${encodeURIComponent(target.name)}`
      let def: Record<string, unknown>
      if (target.type === "function-code") {
        const current = await session.request<FunctionDef>("GET", path)
        def = { ...current, code: text }
      } else {
        def = parseJson(text, "The function definition")
      }
      for (const f of serverFields) delete def[f]
      await session.request("PUT", path, def)
      this.saved.fire(target)
    }
    this.mtimes.set(uri.toString(), Date.now())
    this.emitter.fire([{ type: vscode.FileChangeType.Changed, uri }])
  }

  /** Forget a file (after it's deleted on the server). */
  forget(uri: vscode.Uri): void {
    this.mtimes.delete(uri.toString())
    this.emitter.fire([{ type: vscode.FileChangeType.Deleted, uri }])
  }

  readDirectory(): [string, vscode.FileType][] {
    return []
  }

  createDirectory(uri: vscode.Uri): void {
    throw vscode.FileSystemError.NoPermissions(uri)
  }

  delete(uri: vscode.Uri): void {
    throw vscode.FileSystemError.NoPermissions(uri)
  }

  rename(uri: vscode.Uri): void {
    throw vscode.FileSystemError.NoPermissions(uri)
  }
}

function encode(text: string): Uint8Array {
  return new TextEncoder().encode(text)
}

function parseJson(text: string, what: string): Record<string, unknown> {
  let value: unknown
  try {
    value = JSON.parse(text)
  } catch (e) {
    throw vscode.FileSystemError.Unavailable(`${what} isn't valid JSON: ${e instanceof Error ? e.message : String(e)}`)
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw vscode.FileSystemError.Unavailable(`${what} must be a JSON object`)
  return value as Record<string, unknown>
}
