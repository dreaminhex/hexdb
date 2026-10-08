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

/** Something a role allows. read/write/manage apply to the granted tessellations; the rest apply everywhere. */
export type Action = "read" | "write" | "manage" | "status" | "logs" | "audit" | "plugins" | "maintenance" | "admin"

export interface ResolvedGrant {
  role: string
  tessellations: string[]
  permissions: Action[]
}

export interface Me {
  user_id: string
  login: string
  email_address: string
  roles: RoleGrant[]
  /** The roles' permissions, resolved by the server. */
  grants: ResolvedGrant[]
  /** Permissions that apply everywhere (status, logs, audit, ...). */
  permissions: Action[]
  is_admin: boolean
  credential: { kind: "session"; session_id: string; expires_at: number } | { kind: "api_key"; key_id: string }
}

export interface MfaStatus {
  enabled: boolean
  backup_codes_left: number
  enrolling: boolean
}

export interface AuditEvent {
  time: number
  actor: string
  action: string
  target: string
  outcome: "ok" | "failed" | "denied"
  client: string
  hex: string
  details: Record<string, unknown>
}

export interface AuditQuery {
  actor?: string
  action?: string
  target?: string
  outcome?: string
  since?: number
  until?: number
  limit?: number
  offset?: number
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
  runtime: "process" | "webhook" | "builtin" | ""
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
  analyzer?: string
  documents: number
  keys: number
  ready: boolean
}

export interface NewIndex {
  fields: string[]
  name?: string
  kind?: "field" | "text"
  unique?: boolean
  analyzer?: string
}

export interface Analyzer {
  name: string
  description: string
  pipeline: string
  builtin: boolean
}

export interface IndexSuggestion {
  action: "create_index" | "drop_index"
  index?: NewIndex & { name: string }
  name?: string
  impact: "high" | "medium" | "low"
  reason: string
  source: "rules" | "ai"
}

export interface QueryShape {
  shape: { equality: string[]; range: string[]; other: string[]; text: boolean; sort: string[] }
  count: number
  avg_scanned: number
  avg_returned: number
  avg_ms: number
  indexes_used: string[]
}

export interface Advice {
  tessellation: string
  documents: number
  queries_observed: number
  suggestions: IndexSuggestion[]
  ai: null | { available: boolean; model?: string; error?: string; suggestions?: IndexSuggestion[] }
  shapes: QueryShape[]
}

export interface SchemaVersion {
  version: number
  created: number
  fields: Record<string, Record<string, unknown>>
  additional_fields: boolean
  migration?: Record<string, unknown>[]
}

export interface SchemaMigration {
  version: number
  state: "running" | "done" | "failed"
  migrated: number
  unchanged: number
  failed: number
  errors: { id: string; version: number; problems: { field: string; message: string }[] }[]
  started: number
  finished: number | null
}

export interface Schemas {
  tessellation: string
  current: number | null
  versions: SchemaVersion[]
  migration: SchemaMigration | null
}

export interface SchemaCheck {
  compatible: boolean
  error?: string
  version?: number
  checked?: number
  would_not_fit?: number
  errors?: { id: string; problems: { field: string; message: string }[] }[]
}

export interface Setting {
  key: string
  kind: "integer" | "boolean"
  min: number
  max: number
  unit: string
  live: boolean
  description: string
  value: number | boolean
  saved: number | boolean | null
  default: number | boolean
  restart_required: boolean
}

export interface SettingsResponse {
  settings: Setting[]
  restart_required: boolean
  config_file: string | null
  config: Record<string, unknown>
  disk_used_bytes: number
}

export interface JoinInfo {
  lattice_name: string
  seeds: string[]
  tls: boolean
  secret_setting: string
  hexdb_toml: string
  hexdb_local_toml: string
  notes: string[]
}

export interface StreamSource {
  tessellation: string
  filter?: Record<string, unknown>
  ops?: string[]
}

export interface StreamDestination {
  kind?: "webhook"
  url: string
  headers?: Record<string, string>
  batch_size?: number
}

export interface StreamConfig {
  name: string
  description: string
  retention_hours: number
  sources: StreamSource[]
  destinations: StreamDestination[]
  created?: number
  created_by?: string
}

export interface StreamDetail {
  stream: StreamConfig
  sources: { delivered: number; last_error: string | null }[]
  destinations: { delivered: number; last_error: string | null }[]
  groups: { group: string; offset: string | null; updated: number; pending: number }[]
}

export interface StreamMessage {
  offset: string
  time: number
  payload: unknown
  key?: string | null
  headers?: Record<string, string>
  published_by?: string
}

export interface FunctionParam {
  name: string
  type: string
  required?: boolean
  default?: unknown
  description?: string
}

export interface FunctionDef {
  name: string
  description: string
  kind: "query" | "aggregate" | "transaction" | "script"
  params: FunctionParam[]
  tessellation?: string
  body?: unknown
  runtime?: "python" | "typescript" | "javascript"
  code?: string
  timeout_seconds: number
  created_by?: string
  updated?: number
}

export interface Schedule {
  name: string
  function: string
  params: Record<string, unknown>
  every_seconds?: number
  cron?: string
  enabled: boolean
  run_as_login?: string
  next_run?: number
  last_run?: number
  last_status?: string
  last_error?: string | null
  last_duration_ms?: number
  runs?: number
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
  /** The tessellations the role applies to ("*" for all). */
  tessellations: string[]
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
  permissions: Action[]
  /** Built-in roles can't be changed or deleted. */
  builtin: boolean
}

export interface PermissionInfo {
  name: Action
  description: string
  /** Applies to the granted tessellations only. */
  scoped: boolean
}

export interface RoleInput {
  name?: string
  description?: string
  permissions?: Action[]
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
  login: (login: string, password: string, code?: string) =>
    request<{ user: Me; expires_at: number }>("POST", "/auth/login", code ? { login, password, code } : { login, password }),
  mfa: () => request<MfaStatus>("GET", "/auth/mfa"),
  mfaSetup: (password: string) => request<{ secret: string; otpauth_uri: string }>("POST", "/auth/mfa/setup", { password }),
  mfaEnable: (code: string) => request<{ backup_codes: string[] }>("POST", "/auth/mfa/enable", { code }),
  mfaDisable: (password: string, code: string) => request<void>("POST", "/auth/mfa/disable", { password, code }),
  mfaBackupCodes: (password: string, code: string) => request<{ backup_codes: string[] }>("POST", "/auth/mfa/backup-codes", { password, code }),
  audit: (query: AuditQuery) => {
    const params = new URLSearchParams()
    for (const [key, value] of Object.entries(query)) {
      if (value !== undefined && value !== "") params.set(key, String(value))
    }
    return request<{ events: AuditEvent[]; total: number }>("GET", `/audit?${params}`)
  },
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
  backup: (name?: string) => request<{ name: string; path: string; sequence: number; files: number; linked: number; bytes: number; millis: number }>("POST", "/backup", name ? { name } : {}),

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
  roleCatalog: () => request<{ roles: Role[]; permissions: PermissionInfo[] }>("GET", "/roles"),

  settings: () => request<SettingsResponse>("GET", "/settings"),
  saveSettings: (changes: Record<string, number | boolean | null>) => request<SettingsResponse>("PUT", "/settings", changes),
  joinInfo: (password: string) => request<JoinInfo>("POST", "/join", { password }),

  analyzers: () => request<{ analyzers: Analyzer[] }>("GET", "/analyzers").then((r) => r.analyzers),
  analyze: (analyzer: string, text: string) =>
    request<{ index_tokens: string[]; query_tokens: string[]; pipeline: string }>("POST", "/analyzers/_analyze", { analyzer, text }),
  advice: (tess: string, ai = false) => request<Advice>("GET", `/tessellations/${enc(tess)}/advice${ai ? "?ai=true" : ""}`),
  schemas: (tess: string) => request<Schemas>("GET", `/tessellations/${enc(tess)}/schemas`),
  addSchema: (tess: string, schema: unknown) => request<SchemaVersion>("POST", `/tessellations/${enc(tess)}/schemas`, schema),
  checkSchema: (tess: string, schema: unknown) => request<SchemaCheck>("POST", `/tessellations/${enc(tess)}/schemas/check`, schema),
  dropSchemas: (tess: string) => request<void>("DELETE", `/tessellations/${enc(tess)}/schemas`),

  streams: () => request<{ streams: StreamConfig[] }>("GET", "/streams").then((r) => r.streams),
  stream: (name: string) => request<StreamDetail>("GET", `/streams/${enc(name)}`),
  createStream: (config: StreamConfig) => request<StreamConfig>("POST", "/streams", config),
  updateStream: (name: string, config: StreamConfig) => request<StreamConfig>("PUT", `/streams/${enc(name)}`, config),
  deleteStream: (name: string) => request<void>("DELETE", `/streams/${enc(name)}`),
  publish: (name: string, messages: { payload: unknown; key?: string }[]) => request<{ offsets: string[] }>("POST", `/streams/${enc(name)}/messages`, messages),
  readStream: (name: string, after?: string, limit = 50) =>
    request<{ messages: StreamMessage[]; next: string | null }>("GET", `/streams/${enc(name)}/messages?limit=${limit}${after ? `&after=${enc(after)}` : ""}`),

  functions: () => request<{ functions: FunctionDef[] }>("GET", "/functions").then((r) => r.functions),
  createFunction: (def: FunctionDef) => request<FunctionDef>("POST", "/functions", def),
  updateFunction: (name: string, def: FunctionDef) => request<FunctionDef>("PUT", `/functions/${enc(name)}`, def),
  deleteFunction: (name: string) => request<void>("DELETE", `/functions/${enc(name)}`),
  runFunction: (name: string, params: Record<string, unknown>) => request<{ result: unknown; ms: number }>("POST", `/functions/${enc(name)}/run`, { params }),
  schedules: () => request<{ schedules: Schedule[] }>("GET", "/schedules").then((r) => r.schedules),
  createSchedule: (schedule: Schedule) => request<Schedule>("POST", "/schedules", schedule),
  updateSchedule: (name: string, schedule: Schedule) => request<Schedule>("PUT", `/schedules/${enc(name)}`, schedule),
  deleteSchedule: (name: string) => request<void>("DELETE", `/schedules/${enc(name)}`),
  runSchedule: (name: string) => request<{ result: unknown }>("POST", `/schedules/${enc(name)}/run`),
  createRole: (role: RoleInput) => request<Role>("POST", "/roles", role),
  updateRole: (name: string, role: RoleInput) => request<Role>("PATCH", `/roles/${enc(name)}`, role),
  deleteRole: (name: string) => request<void>("DELETE", `/roles/${enc(name)}`),
}

/** A readable message for any error. */
export function errorMessage(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}
