// HexDB client for Node.js (18+), Deno, Bun and browsers. No dependencies:
// it speaks HexDB's REST API with fetch.
//
//   import { HexDB } from "@dreaminhex/hexdb"
//   const db = new HexDB({ url: "http://127.0.0.1:7700", apiKey: process.env.HEXDB_API_KEY })
//   const orders = db.tessellation("orders")
//   const { id } = await orders.insert({ customer: "ada", total: 12 })
//   const page = await orders.query({ filter: { total: { $gt: 10 } }, sort: "-total" })
//
//   const media = db.files("media")            // a files tessellation
//   await media.upload("photos/IMG_1.jpg", bytes)
//   const hits = await media.search("glacier")  // names, tags and extracted metadata

export interface Options {
  /** Base URL of any hex, e.g. http://127.0.0.1:7700 (replicas forward writes to the Overseer). */
  url: string
  /** An API key (hxk_...) or a session token. */
  apiKey?: string
  /** Retries for 429 and 503 answers (honoring Retry-After). Default 3. */
  retries?: number
  /** Per-request timeout in milliseconds. Default 30,000. */
  timeoutMs?: number
  /** A fetch implementation (defaults to the global one). */
  fetch?: typeof fetch
}

/** An error answer from HexDB: `status` is the HTTP status, `code` HexDB's error code. */
export class HexDBError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
    readonly retryAfter?: number,
  ) {
    super(message)
    this.name = "HexDBError"
  }
}

/** A stored document: its fields plus `id` (and `_expires_at` with a TTL). */
export type Document = { id: string; _expires_at?: string; [field: string]: unknown }

export interface SortKey {
  field: string
  descending?: boolean
}

export interface Query {
  /** A filter like `{ status: "paid", total: { $gte: 10 } }` (see the HexDB manual). */
  filter?: Record<string, unknown>
  /** "-total,name" or sort keys. */
  sort?: string | SortKey[]
  limit?: number
  offset?: number
  /** Continue after this document ID (unsorted queries). */
  after?: string
  /** Only these (dotted) fields. */
  fields?: string[]
}

export interface QueryPage {
  documents: Document[]
  total: number
  next: string | null
  plan?: { indexes: string[]; scanned: number }
}

export interface WriteOptions {
  /** Makes retries safe: the same key returns the first result instead of writing again. */
  idempotencyKey?: string
  /** Expire the document after this many seconds. */
  ttlSeconds?: number
}

export interface Change {
  seq: number
  timestamp: string
  op: "put" | "delete" | "drop_tessellation"
  tessellation: string
  id?: string
  document?: Document
}

export interface StreamMessage {
  offset: string
  time: number
  payload: unknown
  key?: string | null
  headers?: Record<string, string>
}

type Method = "GET" | "POST" | "PUT" | "PATCH" | "DELETE"

const enc = encodeURIComponent
const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms))

export class HexDB {
  private readonly base: string
  private token?: string
  private readonly retries: number
  private readonly timeoutMs: number
  private readonly fetcher: typeof fetch

  constructor(options: Options) {
    this.base = options.url.replace(/\/+$/, "")
    this.token = options.apiKey
    this.retries = options.retries ?? 3
    this.timeoutMs = options.timeoutMs ?? 30_000
    this.fetcher = options.fetch ?? globalThis.fetch.bind(globalThis)
  }

  /** Send any request to the REST API and return the parsed JSON (undefined for 204). */
  async request<T = unknown>(method: Method, path: string, body?: unknown, headers: Record<string, string> = {}, timeoutMs = this.timeoutMs): Promise<T> {
    for (let attempt = 0; ; attempt++) {
      const controller = new AbortController()
      const timer = setTimeout(() => controller.abort(), timeoutMs)
      let response: Response
      try {
        response = await this.fetcher(this.base + path, {
          method,
          headers: {
            Accept: "application/json",
            ...(this.token ? { Authorization: `Bearer ${this.token}` } : {}),
            ...(body === undefined ? {} : { "Content-Type": "application/json" }),
            ...headers,
          },
          body: body === undefined ? undefined : JSON.stringify(body),
          signal: controller.signal,
        })
      } catch (e) {
        throw new HexDBError(0, "unreachable", `Couldn't reach HexDB at ${this.base}: ${e instanceof Error ? e.message : String(e)}`)
      } finally {
        clearTimeout(timer)
      }
      if (response.status === 204) return undefined as T
      const text = await response.text()
      const json = text ? safeJson(text) : undefined
      if (response.ok) return json as T
      const retryAfter = Number(response.headers.get("retry-after")) || undefined
      // Writes are retried only when they carry an idempotency key.
      const retryable = (response.status === 429 || response.status === 503) && (method === "GET" || "Idempotency-Key" in headers)
      if (retryable && attempt < this.retries) {
        await sleep((retryAfter ?? 2 ** attempt) * 1000)
        continue
      }
      const error = (json as { error?: { code?: string; message?: string } } | undefined)?.error
      throw new HexDBError(response.status, error?.code ?? "error", error?.message ?? (text || response.statusText), retryAfter)
    }
  }

  /** Send raw bytes (or nothing) and get the raw response; throws HexDBError on an error answer. */
  async requestRaw(method: Method, path: string, body?: BodyInit, headers: Record<string, string> = {}, timeoutMs = 600_000): Promise<Response> {
    const controller = new AbortController()
    const timer = setTimeout(() => controller.abort(), timeoutMs)
    let response: Response
    try {
      response = await this.fetcher(this.base + path, {
        method,
        headers: { ...(this.token ? { Authorization: `Bearer ${this.token}` } : {}), ...headers },
        body,
        signal: controller.signal,
      })
    } catch (e) {
      clearTimeout(timer)
      throw new HexDBError(0, "unreachable", `Couldn't reach HexDB at ${this.base}: ${e instanceof Error ? e.message : String(e)}`)
    }
    // The body may still be streaming; the timer covers the headers only.
    clearTimeout(timer)
    if (!response.ok) {
      const text = await response.text()
      const error = (safeJson(text) as { error?: { code?: string; message?: string } } | undefined)?.error
      throw new HexDBError(response.status, error?.code ?? "error", error?.message ?? (text || response.statusText))
    }
    return response
  }

  /** A files tessellation (photos, audio, video, documents: any binary file). */
  files(name: string): Files {
    return new Files(this, name)
  }

  /** Sign in with a login and password (and a one-time code with MFA); later requests use the session. */
  async login(login: string, password: string, code?: string): Promise<{ user: unknown; expires_at: number }> {
    const result = await this.request<{ token: string; user: unknown; expires_at: number }>("POST", "/auth/login", { login, password, code, return_token: true })
    this.token = result.token
    return { user: result.user, expires_at: result.expires_at }
  }

  /** End the session (a no-op for API keys). */
  async logout(): Promise<void> {
    await this.request("POST", "/auth/logout")
    this.token = undefined
  }

  /** Whether the server is up (and, signed in, which hex answered). */
  health(): Promise<{ status: string; name?: string; hex_type?: string; version?: string }> {
    return this.request("GET", "/health")
  }

  /** Server status and metrics (needs the status permission). */
  status(): Promise<Record<string, unknown>> {
    return this.request("GET", "/status")
  }

  /** A tessellation (collection of documents). */
  tessellation(name: string): Tessellation {
    return new Tessellation(this, name)
  }

  /** Tessellations you can read. */
  async tessellations(): Promise<{ name: string; kind: string; created: string | null; indexes: string[] }[]> {
    return (await this.request<{ tessellations: [] }>("GET", "/tessellations")).tessellations
  }

  /**
   * Operations across tessellations, all or nothing:
   * `[{ op: "insert" | "replace" | "patch" | "delete" | "get" | "check", tessellation, id?, data?, if_version? }]`.
   */
  transaction(operations: Record<string, unknown>[], options: { idempotencyKey?: string } = {}): Promise<{ results: unknown[]; writes: number }> {
    return this.request("POST", "/transactions", { operations }, idempotency(options.idempotencyKey))
  }

  /** Run a GraphQL query or mutation. Throws on GraphQL errors. */
  async graphql<T = Record<string, unknown>>(query: string, variables?: Record<string, unknown>): Promise<T> {
    const result = await this.request<{ data?: T; errors?: { message: string; extensions?: { code?: string } }[] }>("POST", "/graphql", { query, variables })
    if (result.errors?.length) {
      const first = result.errors[0]
      throw new HexDBError(200, first.extensions?.code ?? "GRAPHQL", first.message)
    }
    return result.data as T
  }

  /**
   * Committed changes after `after` (default: from now), as they happen.
   * Long-polls; stops when `signal` aborts.
   */
  async *changes(options: { after?: number; tessellation?: string; signal?: AbortSignal } = {}): AsyncGenerator<Change> {
    let cursor = options.after
    while (!options.signal?.aborted) {
      const params = new URLSearchParams({ wait: "30", limit: "500" })
      if (cursor !== undefined) params.set("after", String(cursor))
      if (options.tessellation) params.set("tessellation", options.tessellation)
      const page = await this.request<{ changes: Change[]; last_seq: number }>("GET", `/changes?${params}`, undefined, {}, 60_000)
      cursor = page.last_seq
      for (const change of page.changes) yield change
    }
  }

  /** A stream (publish/subscribe). */
  stream(name: string): Stream {
    return new Stream(this, name)
  }

  /** Run a saved function with parameters; returns its result. */
  async runFunction<T = unknown>(name: string, params: Record<string, unknown> = {}): Promise<T> {
    return (await this.request<{ result: T }>("POST", `/functions/${enc(name)}/run`, { params })).result
  }
}

function safeJson(text: string): unknown {
  try {
    return JSON.parse(text)
  } catch {
    return text
  }
}

function idempotency(key?: string): Record<string, string> {
  return key ? { "Idempotency-Key": key } : {}
}

function ttl(options: WriteOptions): string {
  return options.ttlSeconds ? `?ttl=${options.ttlSeconds}` : ""
}

export class Tessellation {
  constructor(
    protected readonly db: HexDB,
    readonly name: string,
  ) {}

  protected get path() {
    return `/${enc(this.name)}`
  }

  /** Insert a document; returns it with its new `id`. */
  insert(doc: Record<string, unknown>, options: WriteOptions = {}): Promise<Document> {
    return this.db.request("POST", `${this.path}${ttl(options)}`, doc, idempotency(options.idempotencyKey))
  }

  /** Insert up to 1000 documents atomically; returns their IDs in order. */
  async insertMany(docs: Record<string, unknown>[], options: WriteOptions = {}): Promise<string[]> {
    return (await this.db.request<{ ids: string[] }>("POST", `${this.path}/_bulk${ttl(options)}`, docs, idempotency(options.idempotencyKey))).ids
  }

  /** A document by ID, or null. */
  async get(id: string, fields?: string[]): Promise<Document | null> {
    try {
      return await this.db.request("GET", `${this.path}/${enc(id)}${fields ? `?fields=${enc(fields.join(","))}` : ""}`)
    } catch (e) {
      if (e instanceof HexDBError && e.status === 404) return null
      throw e
    }
  }

  /** Replace a document's fields. */
  replace(id: string, doc: Record<string, unknown>, options: WriteOptions = {}): Promise<Document> {
    return this.db.request("PUT", `${this.path}/${enc(id)}${ttl(options)}`, doc, idempotency(options.idempotencyKey))
  }

  /** Merge fields into a document (null removes a field). */
  patch(id: string, changes: Record<string, unknown>, options: WriteOptions = {}): Promise<Document> {
    return this.db.request("PATCH", `${this.path}/${enc(id)}${ttl(options)}`, changes, idempotency(options.idempotencyKey))
  }

  /** Delete a document; false if it didn't exist. */
  async delete(id: string, options: { idempotencyKey?: string } = {}): Promise<boolean> {
    try {
      await this.db.request("DELETE", `${this.path}/${enc(id)}`, undefined, idempotency(options.idempotencyKey))
      return true
    } catch (e) {
      if (e instanceof HexDBError && e.status === 404) return false
      throw e
    }
  }

  /** Documents matching a filter, one page at a time. */
  query(query: Query = {}): Promise<QueryPage> {
    return this.db.request("POST", `${this.path}/_query`, query)
  }

  /** Every matching document, page by page (unsorted, in ID order). */
  async *iterate(query: Omit<Query, "sort" | "offset" | "after"> = {}): AsyncGenerator<Document> {
    let after: string | undefined
    for (;;) {
      const page = await this.query({ ...query, limit: query.limit ?? 500, after })
      for (const doc of page.documents) yield doc
      if (!page.next) return
      after = page.next
    }
  }

  /** How many documents match (all of them without a filter). */
  async count(filter?: Record<string, unknown>): Promise<number> {
    const path = filter ? `${this.path}/count?filter=${enc(JSON.stringify(filter))}` : `${this.path}/count`
    return (await this.db.request<{ count: number }>("GET", path)).count
  }

  /** Group and summarize: `{ group_by: ["status"], aggregates: { total: { $sum: "amount" } } }`. */
  aggregate(spec: Record<string, unknown>): Promise<{ rows: Record<string, unknown>[]; total_groups: number; matched: number }> {
    return this.db.request("POST", `${this.path}/_aggregate`, spec)
  }

  /** Insert or replace documents matched by key fields (e.g. an ID from another system). */
  upsert(key: string | string[], docs: Record<string, unknown>[], options: { idempotencyKey?: string } = {}): Promise<{ inserted: number; replaced: number; ids: string[] }> {
    return this.db.request("POST", `${this.path}/_upsert`, { key, documents: docs }, idempotency(options.idempotencyKey))
  }

  /** Merge `update` into every document matching `filter`. */
  updateWhere(filter: Record<string, unknown>, update: Record<string, unknown>): Promise<{ matched: number; modified: number }> {
    return this.db.request("POST", `${this.path}/_update`, { filter, update })
  }

  /**
   * The values behind vault tokens of tokenized fields (up to 1,000; needs the
   * `detokenize` permission; audited). One-way and unknown tokens map to null.
   */
  async detokenize(tokens: string[]): Promise<Record<string, string | null>> {
    return (await this.db.request<{ values: Record<string, string | null> }>("POST", `/tessellations/${enc(this.name)}/detokenize`, { tokens })).values
  }
}

export class Stream {
  constructor(
    private readonly db: HexDB,
    readonly name: string,
  ) {}

  private get path() {
    return `/streams/${enc(this.name)}`
  }

  /** Publish messages; returns their offsets. */
  async publish(...messages: { payload: unknown; key?: string; headers?: Record<string, string> }[]): Promise<string[]> {
    return (await this.db.request<{ offsets: string[] }>("POST", `${this.path}/messages`, messages)).offsets
  }

  /** Messages after an offset or a group's committed offset; waits up to `wait` seconds for new ones. */
  read(options: { after?: string; group?: string; limit?: number; wait?: number } = {}): Promise<{ messages: StreamMessage[]; next: string | null }> {
    const params = new URLSearchParams()
    for (const [k, v] of Object.entries(options)) if (v !== undefined) params.set(k, String(v))
    return this.db.request("GET", `${this.path}/messages?${params}`, undefined, {}, ((options.wait ?? 0) + 30) * 1000)
  }

  /** Commit a consumer group's offset. */
  commit(group: string, offset: string): Promise<void> {
    return this.db.request("POST", `${this.path}/groups/${enc(group)}/commit`, { offset })
  }

  /**
   * Consume as a group: calls `handle` for each message in order, committing
   * after each batch it handled. Stops when `signal` aborts.
   */
  async consume(group: string, handle: (message: StreamMessage) => Promise<void> | void, options: { signal?: AbortSignal; batch?: number } = {}): Promise<void> {
    while (!options.signal?.aborted) {
      const { messages, next } = await this.read({ group, limit: options.batch ?? 100, wait: 30 })
      for (const message of messages) await handle(message)
      if (messages.length > 0 && next) await this.commit(group, next)
    }
  }
}

export default HexDB

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/** A file's document: its metadata. */
export type FileDocument = Document & {
  name: string
  path: string
  size: number
  content_type: string
  sha256: string
  version: number
  meta?: Record<string, unknown>
  extract?: { status: "pending" | "done"; extractors?: string[]; errors?: string[] }
  thumbnail?: { width: number; height: number; content_type: string }
}

export interface UploadOptions {
  contentType?: string
  /** When the file was last modified (RFC 3339 or epoch milliseconds). */
  modified?: string | number
  /** Bytes sent so far, for progress. */
  onProgress?: (sent: number) => void
}

const RESUMABLE_FROM = 16 * 1024 * 1024
const PART = 8 * 1024 * 1024
const ULID = /^[0-9A-HJKMNP-TV-Z]{26}$/

function filePath(path: string): string {
  return path
    .split(/[\\/]/)
    .filter(Boolean)
    .map(enc)
    .join("/")
}

/**
 * A files tessellation: each file's metadata is a document (query it like
 * any tessellation); its content is uploaded and downloaded here. Paths are
 * `folder/name`. Uploading to a path that has a file makes a new version.
 */
export class Files extends Tessellation {
  /** Create the files tessellation (no error if it exists). */
  async create(): Promise<{ name: string; kind: string }> {
    try {
      return await this.db.request("POST", "/tessellations", { name: this.name, kind: "files" })
    } catch (e) {
      if (e instanceof HexDBError && e.status === 409) return this.db.request("GET", `/tessellations/${enc(this.name)}`)
      throw e
    }
  }

  /** Upload bytes or a Blob to `path`. Files of 16 MB and more go in resumable parts. */
  async upload(path: string, data: Uint8Array | ArrayBuffer | Blob, options: UploadOptions = {}): Promise<FileDocument> {
    const blob = data instanceof Blob ? data : new Blob([data as BlobPart])
    const modified = options.modified === undefined ? undefined : String(options.modified)
    if (blob.size < RESUMABLE_FROM) {
      const query = modified ? `?modified=${enc(modified)}` : ""
      const response = await this.db.requestRaw("PUT", `${this.path}/_files/${filePath(path)}${query}`, blob, options.contentType ? { "Content-Type": options.contentType } : {})
      options.onProgress?.(blob.size)
      return (await response.json()) as FileDocument
    }
    const session = await this.db.request<{ id: string }>("POST", `${this.path}/_uploads`, { path, size: blob.size, content_type: options.contentType, modified })
    const url = `${this.path}/_uploads/${enc(session.id)}`
    for (let offset = 0; offset < blob.size; ) {
      const end = Math.min(offset + PART, blob.size)
      for (let attempt = 0; ; attempt++) {
        try {
          await this.db.requestRaw("PATCH", `${url}?offset=${offset}`, blob.slice(offset, end))
          break
        } catch (e) {
          if (attempt >= 3) throw e
          await sleep(1000 * (attempt + 1))
          const status = await this.db.request<{ offset: number }>("GET", url)
          if (status.offset >= end) break
        }
      }
      offset = end
      options.onProgress?.(offset)
    }
    return this.db.request("POST", `${url}/complete`, undefined, {}, 600_000)
  }

  private contentPath(pathOrId: string, version?: number): string {
    const query = version === undefined ? "" : `?version=${version}`
    return ULID.test(pathOrId) ? `${this.path}/${enc(pathOrId)}/content${query}` : `${this.path}/_files/${filePath(pathOrId)}${query}`
  }

  /** A file's content (by `folder/name` or ID), or an earlier version's. */
  async download(pathOrId: string, options: { version?: number } = {}): Promise<Uint8Array> {
    const response = await this.db.requestRaw("GET", this.contentPath(pathOrId, options.version))
    return new Uint8Array(await response.arrayBuffer())
  }

  /** A file's content as a stream (for large files). */
  async downloadStream(pathOrId: string, options: { version?: number } = {}): Promise<ReadableStream<Uint8Array>> {
    const response = await this.db.requestRaw("GET", this.contentPath(pathOrId, options.version))
    if (!response.body) throw new HexDBError(0, "empty", "The response had no body.")
    return response.body
  }

  /** Bytes `start` to `end` (exclusive) of a file. */
  async readRange(pathOrId: string, start: number, end: number): Promise<Uint8Array> {
    const response = await this.db.requestRaw("GET", this.contentPath(pathOrId), undefined, { Range: `bytes=${start}-${end - 1}` })
    return new Uint8Array(await response.arrayBuffer())
  }

  /** A file's document (metadata), or null. */
  async stat(pathOrId: string): Promise<FileDocument | null> {
    if (ULID.test(pathOrId)) return (await this.get(pathOrId)) as FileDocument | null
    const parts = pathOrId.split(/[\\/]/).filter(Boolean)
    const name = parts.pop() ?? ""
    const folder = parts.length ? `/${parts.join("/")}/` : "/"
    const page = await this.query({ filter: { path: folder, name }, limit: 1 })
    return (page.documents[0] as FileDocument | undefined) ?? null
  }

  /** The files directly in a folder, by name. */
  async list(folder = "/", limit = 1000): Promise<FileDocument[]> {
    const parts = folder.split(/[\\/]/).filter(Boolean)
    const path = parts.length ? `/${parts.join("/")}/` : "/"
    return (await this.query({ filter: { path }, sort: "name", limit })).documents as FileDocument[]
  }

  /** Files whose name, folders, tags or extracted metadata contain every word of `text`. */
  async search(text: string, filter: Record<string, unknown> = {}, limit = 100): Promise<FileDocument[]> {
    return (await this.query({ filter: { $text: text, ...filter }, limit })).documents as FileDocument[]
  }

  /** A small JPEG or PNG of an image (once its metadata has been read). */
  async thumbnail(id: string): Promise<Uint8Array> {
    const response = await this.db.requestRaw("GET", `${this.path}/${enc(id)}/thumbnail`)
    return new Uint8Array(await response.arrayBuffer())
  }

  versions(id: string): Promise<{ id: string; current: Record<string, unknown>; versions: Record<string, unknown>[] }> {
    return this.db.request("GET", `${this.path}/${enc(id)}/versions`)
  }

  /** Make an earlier version current again (as a new version). */
  restoreVersion(id: string, version: number): Promise<FileDocument> {
    return this.db.request("POST", `${this.path}/${enc(id)}/versions/${version}/restore`)
  }

  /** Deleted files (`delete` moves a file to the trash). */
  async trash(): Promise<FileDocument[]> {
    return (await this.db.request<{ documents: FileDocument[] }>("GET", `${this.path}/_trash`)).documents
  }

  restore(id: string, options: { name?: string; path?: string } = {}): Promise<FileDocument> {
    const params = new URLSearchParams()
    if (options.name) params.set("name", options.name)
    if (options.path) params.set("path", options.path)
    const query = params.toString()
    return this.db.request("POST", `${this.path}/_trash/${enc(id)}/restore${query ? `?${query}` : ""}`)
  }

  /** Delete a trashed file for good. */
  async purge(id: string): Promise<void> {
    await this.db.request("DELETE", `${this.path}/_trash/${enc(id)}`)
  }

  /** Read metadata again (one file, or all); returns how many were queued. */
  async reextract(id?: string): Promise<number> {
    return (await this.db.request<{ queued: number }>("POST", `${this.path}/_reextract${id ? `?id=${enc(id)}` : ""}`)).queued
  }
}
