// Pure helpers (no vscode import), unit-tested in test/.

import { isConnectionUri, parseConnectionUri } from "@dreaminhex/hexdb"

import type { Column } from "./shared/messages"

export interface Grant {
  role?: string
  permissions: string[]
  tessellations: string[]
}

export interface Me {
  login: string
  is_admin: boolean
  grants: Grant[]
  demo_mode?: boolean
}

/** Whether `me` has `permission` ("read", "write", ...) on a tessellation, as the server decides it. */
export function can(me: Me | undefined, permission: string, tessellation: string): boolean {
  if (!me) return false
  if (me.is_admin) return true
  return me.grants.some((g) => g.permissions.includes(permission) && g.tessellations.some((t) => t === "*" || t === tessellation))
}

/** A tessellation name as a SQL identifier (quoted unless it's a plain word). */
export function sqlIdent(name: string): string {
  return /^[A-Za-z_][A-Za-z0-9_]*$/.test(name) ? name : `"${name.replace(/"/g, '""')}"`
}

/** The tessellation a simple `SELECT ... FROM t` reads, so its rows' ids can open documents. */
export function singleTable(sql: string): string | undefined {
  const text = sql.replace(/--[^\n]*/g, " ").replace(/\/\*[\s\S]*?\*\//g, " ")
  if (/\b(join|union|intersect|except)\b/i.test(text)) return undefined
  const froms = [...text.matchAll(/\bfrom\s+("(?:[^"]|"")+"|[A-Za-z0-9_-]+)/gi)]
  if (froms.length !== 1) return undefined
  const name = froms[0][1]
  return name.startsWith('"') ? name.slice(1, -1).replace(/""/g, '"') : name
}

/** The statement to run: the selection if there is one, otherwise the statement around the cursor. */
export function statementAt(text: string, offset: number): { text: string; start: number } {
  // Split on semicolons outside strings, identifiers and comments.
  const parts: { start: number; end: number }[] = []
  let start = 0
  let quote: string | null = null
  for (let i = 0; i < text.length; i++) {
    const c = text[i]
    if (quote) {
      if (c === quote) quote = null
      continue
    }
    if (c === "'" || c === '"') quote = c
    else if (c === "-" && text[i + 1] === "-") {
      const nl = text.indexOf("\n", i)
      i = nl < 0 ? text.length : nl
    } else if (c === "/" && text[i + 1] === "*") {
      const close = text.indexOf("*/", i + 2)
      i = close < 0 ? text.length : close + 1
    } else if (c === ";") {
      parts.push({ start, end: i })
      start = i + 1
    }
  }
  parts.push({ start, end: text.length })
  const nonEmpty = parts.filter((p) => stripComments(text.slice(p.start, p.end)).trim())
  if (nonEmpty.length === 0) return { text: "", start: 0 }
  const hit = nonEmpty.find((p) => offset >= p.start && offset <= p.end + 1) ?? nonEmpty[nonEmpty.length - 1]
  const raw = text.slice(hit.start, hit.end)
  const lead = raw.length - raw.trimStart().length
  return { text: raw.trim(), start: hit.start + lead }
}

function stripComments(sql: string): string {
  return sql.replace(/--[^\n]*/g, "").replace(/\/\*[\s\S]*?\*\//g, "")
}

/** Rows and columns for a JSON value that is a list of objects (GraphQL, function results), if it is one. */
export function tabulate(value: unknown): { columns: Column[]; rows: unknown[][] } | undefined {
  const found = findList(value, 0)
  if (!found || found.length === 0) return undefined
  // GraphQL documents are {id, data: {...}}: show their fields as columns.
  const list = found.map((item) => {
    const r = item as Record<string, unknown>
    return isRecord(r.data) && Object.keys(r).every((k) => k === "id" || k === "data" || k === "version") ? { ...(r.id === undefined ? {} : { id: r.id }), ...r.data } : r
  })
  const names: string[] = []
  const seen = new Set<string>()
  for (const item of list.slice(0, 500)) {
    for (const key of Object.keys(item as object)) {
      if (!seen.has(key)) {
        seen.add(key)
        names.push(key)
      }
    }
  }
  // id first, like SQL's SELECT *.
  names.sort((a, b) => (a === "id" ? -1 : b === "id" ? 1 : 0))
  return {
    columns: names.map((name) => ({ name })),
    rows: list.map((item) => names.map((n) => (item as Record<string, unknown>)[n] ?? null)),
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}

/** The first list of objects found by following single-key objects (`{data: {orders: [...]}}`, `{documents: [...]}`). */
function findList(value: unknown, depth: number): unknown[] | undefined {
  if (depth > 4) return undefined
  if (Array.isArray(value)) return value.length > 0 && value.every(isRecord) ? value : undefined
  if (!isRecord(value)) return undefined
  for (const key of ["documents", "rows", "items", "results", "groups"]) {
    if (Array.isArray(value[key])) return findList(value[key], depth + 1)
  }
  const keys = Object.keys(value).filter((k) => value[k] !== null && typeof value[k] === "object")
  return keys.length === 1 ? findList(value[keys[0]], depth + 1) : undefined
}

/** CSV text for columns and rows (objects and arrays as JSON). */
export function toCsv(columns: Column[], rows: unknown[][]): string {
  const cell = (v: unknown): string => {
    if (v === null || v === undefined) return ""
    const s = typeof v === "object" ? JSON.stringify(v) : String(v)
    return /[",\r\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s
  }
  return [columns.map((c) => cell(c.name)).join(","), ...rows.map((r) => r.map(cell).join(","))].join("\r\n") + "\r\n"
}

/** A function parameter's typed value from what was typed in an input box. */
export function parseParam(type: string, text: string): unknown {
  const t = text.trim()
  switch (type) {
    case "string":
      return text
    case "integer": {
      if (!/^-?\d+$/.test(t)) throw new Error(`"${text}" isn't an integer`)
      return Number(t)
    }
    case "number": {
      const n = Number(t)
      if (t === "" || Number.isNaN(n)) throw new Error(`"${text}" isn't a number`)
      return n
    }
    case "boolean":
      if (/^(true|false)$/i.test(t)) return t.toLowerCase() === "true"
      throw new Error(`"${text}" isn't true or false`)
    default:
      // object, array, any: JSON, or a bare string for `any`.
      try {
        return JSON.parse(t)
      } catch {
        if (type === "any") return text
        throw new Error(`"${text}" isn't valid JSON`)
      }
  }
}

/** Seconds as "3d 4h", "2h 5m", "4m 10s". */
export function duration(seconds: number): string {
  const s = Math.max(0, Math.floor(seconds))
  const d = Math.floor(s / 86400)
  const h = Math.floor((s % 86400) / 3600)
  const m = Math.floor((s % 3600) / 60)
  if (d) return `${d}d ${h}h`
  if (h) return `${h}h ${m}m`
  if (m) return `${m}m ${s % 60}s`
  return `${s}s`
}

/** The file extension for a script runtime. */
export function runtimeExtension(runtime: string | undefined): string {
  switch (runtime) {
    case "python":
      return ".py"
    case "typescript":
      return ".ts"
    case "javascript":
      return ".js"
    default:
      return ".txt"
  }
}

/** Turn what was typed (a hexdb:// URI or an http(s) URL) into a hexdb URI, adding credentials if given. */
export function toConnectionUri(input: string, credentials?: { apiKey: string } | { login: string; password: string }): string {
  let text = input.trim()
  if (!isConnectionUri(text)) {
    let url: URL
    try {
      url = new URL(/^[a-z]+:\/\//i.test(text) ? text : `http://${text}`)
    } catch {
      throw new Error("Enter a hexdb:// or hexdbs:// URI, or an http(s) address")
    }
    if (url.protocol !== "http:" && url.protocol !== "https:") throw new Error("Enter a hexdb:// or hexdbs:// URI, or an http(s) address")
    const tls = url.protocol === "https:"
    // An http URL without a port means port 80, which isn't hexdb://'s default.
    const port = url.port || (tls ? "" : "80")
    const userinfo = url.username ? `${url.username}${url.password ? `:${url.password}` : ""}@` : ""
    text = `${tls ? "hexdbs" : "hexdb"}://${userinfo}${url.hostname}${port ? `:${port}` : ""}${url.pathname.replace(/\/+$/, "")}${url.search}`
  }
  if (credentials) {
    const parsed = parseConnectionUri(text)
    if (parsed.login !== null || parsed.apiKey !== null) throw new Error("The URI already has credentials")
    if ("apiKey" in credentials) {
      text += `${text.includes("?") ? "&" : "?"}apiKey=${encodeURIComponent(credentials.apiKey)}`
    } else {
      text = text.replace(/^(hexdbs?:\/\/)/i, `$1${encodeURIComponent(credentials.login)}:${encodeURIComponent(credentials.password)}@`)
    }
  }
  parseConnectionUri(text) // throws with the problem if it isn't valid
  return text
}

/** The same URI with credentials removed (for sharing). */
export function withoutCredentials(uri: string): string {
  return uri
    .replace(/^(hexdbs?:\/\/)[^@/?#]*@/i, "$1")
    .replace(/([?&])apikey=[^&#]*&?/gi, "$1")
    .replace(/[?&]$/, "")
}

/** The tessellation a GraphQL query reads, when it names exactly one. */
export function graphqlTessellation(query: string): string | undefined {
  const names = new Set([...query.matchAll(/\btessellation\s*:\s*"([A-Za-z0-9_-]+)"/g)].map((m) => m[1]))
  return names.size === 1 ? [...names][0] : undefined
}
