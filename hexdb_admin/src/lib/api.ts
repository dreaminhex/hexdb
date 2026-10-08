// Typed client for the HexDB REST API. Errors come back as ApiError, built
// from the server's `{"error": {"code", "message"}}` responses.

export class ApiError extends Error {
  status: number
  code: string

  constructor(status: number, code: string, message: string) {
    super(message)
    this.status = status
    this.code = code
  }
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  let response: Response
  try {
    response = await fetch(path, {
      method,
      headers: body === undefined ? { Accept: "application/json" } : { "Content-Type": "application/json", Accept: "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    })
  } catch (e) {
    throw new ApiError(0, "unreachable", `Could not reach HexDB: ${e instanceof Error ? e.message : String(e)}`)
  }
  if (response.status === 401 && !path.startsWith("/auth/login")) {
    // The session expired or was revoked; the app shows the sign-in screen.
    window.dispatchEvent(new Event(UNAUTHORIZED_EVENT))
  }
  if (response.status === 204) return undefined as T

  const text = await response.text()
  let json: unknown = undefined
  try {
    json = text ? JSON.parse(text) : undefined
  } catch {
    // Not JSON; handled below.
  }
  if (!response.ok) {
    const error = (json as { error?: { code?: string; message?: string } } | undefined)?.error
    throw new ApiError(response.status, error?.code ?? "error", error?.message ?? (text || response.statusText))
  }
  return json as T
}

const enc = encodeURIComponent

/** Fired when any request comes back 401. */
export const UNAUTHORIZED_EVENT = "hexdb:unauthorized"

export interface Me {
  user_id: string
  login: string
  email_address: string
  roles: RoleGrant[]
  is_admin: boolean
  credential: { kind: "session"; session_id: string; expires_at: number } | { kind: "api_key"; key_id: string }
}

export interface ApiKeyInfo {
  id: string
  name: string
  user_id: string
  login: string
  created: number
  /** Epoch seconds, or 0 for no expiry. */
  expires: number
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface VertexStatus {
  id: number
  status: string
  bytes: number
  shards: number
  corrupt_shards_found: number
  shards_repaired: number
}

export interface TessellationMetrics {
  name: string
  kind: string
  document_count: number
  documents_in_ram: number
  documents_on_disk: number
  avg_document_size_bytes: number
  max_document_size_bytes: number
  min_document_size_bytes: number
  total_size_bytes: number
}

export interface LatticeHex {
  id: string
  name: string
  role: string
  /** "active" or "lost" (missed several discovery rounds). */
  status: string
  ip: string
  api_endpoint: string
  /** Configured role preference: auto, overseer, harvester, or replicant. */
  preference: string
  last_seq: number
  last_seen: string | null
  is_self: boolean
  /** leading (Overseer), streaming, syncing, waiting, or error. */
  replication_state: string
  applied_seq: number
  /** Changes behind the Overseer (replicas only, when known). */
  lag: number | null
}

export interface ReplicationStatus {
  state: string
  source_id: string | null
  source_name: string | null
  applied_seq: number
  source_seq: number
  synced_documents: number
  last_sync: string | null
  last_error: string | null
  lag?: number
}

export interface Status {
  id: string
  name: string
  start_datetime: string
  uptime_seconds: number
  version: string
  status: string
  hex_type: string
  ram_mb: number
  disk_mb: number
  vertices: VertexStatus[]
  metrics: {
    total_document_count: number
    documents_in_ram: number
    documents_on_disk: number
    avg_document_size_bytes: number
    max_document_size_bytes: number
    min_document_size_bytes: number
    total_size_bytes: number
    tessellations: TessellationMetrics[]
  }
  storage: {
    ram_budget_bytes: number
    memory_bytes: number
    memory_entries: number
    unflushed_entries: number
    unflushed_bytes: number
    disk_bytes: number
    sstable_files: number
    next_sequence: number
  }
  operations: { reads_total: number; writes_total: number; queries_total: number }
  network: {
    api_endpoint: string
    discovery_endpoint: string
    lattice: { name: string; hexes: LatticeHex[] }
  }
  replication: ReplicationStatus
}

export interface LogRecord {
  seq: number
  timestamp: string
  level: "ERROR" | "WARN" | "INFO" | "DEBUG" | "TRACE"
  target: string
  message: string
  fields?: Record<string, string>
}

export interface LogQuery {
  level?: string
  after?: number
  before?: number
  q?: string
  target?: string
  limit?: number
}

export interface LogPage {
  records: LogRecord[]
  last_seq: number
  capacity: number
}

export interface PluginStatus {
  id: string
  name: string
  type: string
  version: string
  description: string
  runtime: "process" | "webhook" | ""
  path: string
  state: "running" | "standby" | "disabled" | "invalid" | "error"
  delivered: number
  last_seq: number
  skipped: number
  restarts: number
  last_error: string | null
  started_at: string | null
}

export interface MetricsSample {
  timestamp: string
  documents: Record<string, number>
  memory_bytes: number
  disk_bytes: number
  unflushed_entries: number
  reads_total: number
  writes_total: number
  queries_total: number
}

export interface Tessellation {
  name: string
  kind: string
  created: string | null
  document_count?: number
  /** Names of the tessellation's secondary indexes. */
  indexes: string[]
}

export interface IndexInfo {
  name: string
  kind: "field" | "text"
  fields: string[]
  unique: boolean
  documents: number
  keys: number
  ready: boolean
}

export interface NewIndex {
  fields: string[]
  name?: string
  kind?: "field" | "text"
  unique?: boolean
}

/** A document as returned by the API: its fields plus `id` and `_expires_at`. */
export type ApiDocument = { id: string; _expires_at?: string } & Record<string, unknown>

export interface DocumentPage {
  documents: ApiDocument[]
  total: number
  next: string | null
  /** How the query ran: indexes used (empty = full scan) and documents read. */
  plan?: { indexes: string[]; scanned: number }
}

export interface SortKey {
  field: string
  descending?: boolean
}

export interface DocumentQuery {
  filter?: unknown
  sort?: SortKey[] | string
  limit?: number
  offset?: number
}

export interface RoleGrant {
  name: string
  permissions: string[]
}

export interface User {
  id: string
  login: string
  email_address: string
  roles: RoleGrant[]
  created: number
  last_login: number
  is_locked: boolean
  use_mfa: boolean
  last_password_change: number
  password_expiration: number
}

export interface Role {
  id: string
  name: string
  description: string
}

export interface NewUser {
  login: string
  password: string
  email_address: string
  roles: RoleGrant[]
}

export type UserChanges = Partial<{
  login: string
  password: string
  email_address: string
  roles: RoleGrant[]
  is_locked: boolean
  use_mfa: boolean
}>

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

export const api = {
  me: () => request<Me>("GET", "/auth/me"),
  login: (login: string, password: string) => request<{ user: Me; expires_at: number }>("POST", "/auth/login", { login, password }),
  logout: () => request<void>("POST", "/auth/logout"),
  changePassword: (current_password: string, new_password: string) =>
    request<void>("POST", "/auth/password", { current_password, new_password }),
  apiKeys: () => request<{ keys: ApiKeyInfo[] }>("GET", "/auth/keys").then((r) => r.keys),
  createApiKey: (name: string, expires_in_days?: number) =>
    request<{ key: string; info: ApiKeyInfo }>("POST", "/auth/keys", { name, expires_in_days }),
  revokeApiKey: (id: string) => request<void>("DELETE", `/auth/keys/${enc(id)}`),
  status: () => request<Status>("GET", "/status"),
  history: (minutes: number) =>
    request<{ interval_seconds: number; samples: MetricsSample[] }>("GET", `/status/history?minutes=${minutes}`),
  plugins: () => request<{ plugins: PluginStatus[]; registry: string; enabled: boolean }>("GET", "/plugins"),
  logs: (query: LogQuery) => {
    const params = new URLSearchParams()
    for (const [key, value] of Object.entries(query)) {
      if (value !== undefined && value !== "") params.set(key, String(value))
    }
    return request<LogPage>("GET", `/logs?${params}`)
  },
  flush: () => request<{ entries: number; tessellations: number }>("POST", "/flush"),

  tessellations: () => request<{ tessellations: Tessellation[] }>("GET", "/tessellations").then((r) => r.tessellations),
  tessellation: (name: string) => request<Tessellation>("GET", `/tessellations/${enc(name)}`),
  createTessellation: (name: string) => request<Tessellation>("POST", "/tessellations", { name }),
  deleteTessellation: (name: string) => request<void>("DELETE", `/tessellations/${enc(name)}`),
  indexes: (tess: string) => request<{ indexes: IndexInfo[] }>("GET", `/tessellations/${enc(tess)}/indexes`).then((r) => r.indexes),
  createIndex: (tess: string, index: NewIndex) => request<IndexInfo>("POST", `/tessellations/${enc(tess)}/indexes`, index),
  dropIndex: (tess: string, name: string) => request<void>("DELETE", `/tessellations/${enc(tess)}/indexes/${enc(name)}`),

  queryDocuments: (tess: string, query: DocumentQuery) => request<DocumentPage>("POST", `/${enc(tess)}/_query`, query),
  document: (tess: string, id: string) => request<ApiDocument>("GET", `/${enc(tess)}/${enc(id)}`),
  insertDocument: (tess: string, data: unknown, ttl?: number) =>
    request<ApiDocument>("POST", `/${enc(tess)}${ttl ? `?ttl=${ttl}` : ""}`, data),
  replaceDocument: (tess: string, id: string, data: unknown, ttl?: number) =>
    request<ApiDocument>("PUT", `/${enc(tess)}/${enc(id)}${ttl ? `?ttl=${ttl}` : ""}`, data),
  deleteDocument: (tess: string, id: string) => request<void>("DELETE", `/${enc(tess)}/${enc(id)}`),

  users: () => request<{ users: User[] }>("GET", "/users").then((r) => r.users),
  createUser: (user: NewUser) => request<User>("POST", "/users", user),
  updateUser: (id: string, changes: UserChanges) => request<User>("PATCH", `/users/${enc(id)}`, changes),
  deleteUser: (id: string) => request<void>("DELETE", `/users/${enc(id)}`),

  roles: () => request<{ roles: Role[] }>("GET", "/roles").then((r) => r.roles),
}

/** A readable message for any error. */
export function errorMessage(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}
