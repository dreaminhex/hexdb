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
export type Action = "read" | "write" | "manage" | "upload" | "delete" | "restore" | "status" | "logs" | "audit" | "plugins" | "maintenance" | "admin" | "ai" | "chat" | "share" | "detokenize"

/** A sharing link, as its maker sees it. */
export interface ShareLink {
  code: string
  path: string
  tessellation: string
  file_id: string | null
  folder: string | null
  name: string
  created_by: string
  created_at: number
  expires_at: number | null
  password: boolean
  max_downloads: number | null
  downloads: number
  note: string
  expired: boolean
}

/** The public address of a sharing link. */
export function shareAddress(link: ShareLink): string {
  return `${window.location.origin}${link.path}`
}

/** `GET /ai/chat`: whether the chat is on for this user, and what's left today. */
export interface ChatStatus {
  enabled: boolean
  max_question_chars: number
  left: { user_today: number | null; client_today: number | null }
}

/** `POST /ai/chat`. */
export interface ChatAnswer {
  answer: string
  steps: { tool: string; input: unknown; ok: boolean }[]
  model: string
  tokens: { input: number; output: number }
  left: ChatStatus["left"]
}

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
  /** The hex runs in demo mode: everyone may view the Configuration page (read-only). */
  demo_mode?: boolean
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
  /** replication.slicing on this hex, and how many of the 64 slices it keeps. */
  slicing?: boolean
  slices_held?: number
  /** The HexDB version the hex runs. */
  version?: string
}

/** The newest HexDB release, from the hex's last check (updates.check). */
export interface ReleaseCheck {
  latest: string | null
  url: string | null
  checked: number | null
  error: string | null
  enabled: boolean
  /** A hosted lattice, which can update its hexes from the admin UI. */
  hosted: boolean
}

/** A rolling update of a hosted lattice, as the Overseer running it reports. */
export interface Rollout {
  state: "running" | "done" | "failed"
  target: string
  started_by: string
  started: number
  finished: number | null
  steps: { hex: string; id: string; from: string; state: "waiting" | "updating" | "handing over" | "done" | "failed" }[]
  error: string | null
}

export interface Updates {
  running: string
  release: ReleaseCheck
  hosted: boolean
  /** What the hosting service would install (hosted lattices). */
  offer: { version: string; image?: string } | { error: string } | null
  rollout: Rollout | null
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
  /** Set while a newly elected Overseer collects slices from the replicas. */
  rebuilding?: string | null
  /** While rebuilding: unreachable hexes holding slices no reachable hex has. */
  waiting_for?: string[]
}

export interface CrashReport {
  detected_at: number
  previous_started_at: number
  previous_pid: number
  previous_version: string
  panic: { at: number; message: string; location: string; thread: string } | null
  exit: string | null
  supervisor_restarts: number | null
  recovery_ms: number
}

export interface Status {
  restarts?: { unclean_shutdowns: number; last: CrashReport | null; recent: CrashReport[]; panics_since_start: number
    supervised: boolean
    task_restarts: number
    tasks: { name: string; restarts: number; last_panic: string | null; last_panic_at: number | null; running: boolean }[]
  }
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
  updates?: ReleaseCheck
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
  /** The current schema version (null: schemaless). */
  schema_version?: number | null
  /** A schema version is saved, waiting to be applied. */
  schema_pending?: boolean
}

export type IndexKind = "field" | "text" | "vector"

export interface IndexInfo {
  name: string
  kind: IndexKind
  fields: string[]
  unique: boolean
  analyzer?: string
  dimensions?: number
  metric?: "cosine" | "dot"
  quantization?: "none" | "int8"
  source?: string[]
  documents: number
  keys: number
  ready: boolean
}

export interface NewIndex {
  fields: string[]
  name?: string
  kind?: IndexKind
  unique?: boolean
  analyzer?: string
  dimensions?: number
  metric?: "cosine" | "dot"
  quantization?: "none" | "int8"
  source?: string[]
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
  created_by_login?: string
  /** Set on a rollback: the version whose fields this one restores. */
  restores?: number
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

/** A next version saved to apply later. */
export interface PendingSchema {
  schema: { fields: Record<string, Record<string, unknown>>; additional_fields?: boolean; migration?: Record<string, unknown>[] }
  /** The current version when it was saved (0: none). */
  based_on: number
  saved: number
  saved_by_login?: string
  /** The number it would get now; null when it no longer fits (see `error`). */
  version: number | null
  error: string | null
}

export interface Schemas {
  tessellation: string
  current: number | null
  versions: SchemaVersion[]
  pending: PendingSchema | null
  migration: SchemaMigration | null
}

/** A first schema read from a tessellation's documents (GET .../schemas/infer). */
export interface InferredSchema {
  fields: Record<string, Record<string, unknown>>
  additional_fields: boolean
  /** Documents read (up to 1,000). */
  sampled: number
  /** How many of them have each field (not null). */
  present: Record<string, number>
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

/** A setting of the lattice configuration (see GET /config). */
export interface ConfigDef {
  key: string
  kind:
    | { type: "text" | "url" | "origins" | "secret" | "boolean" }
    | { type: "integer"; min: number; max: number }
    | { type: "choice" | "set"; choices: string[] }
  description: string
  default: unknown
  secret: boolean
}

/** What GET /config shows of a secret: never its value. */
export interface SecretInfo {
  set: true
  fingerprint: string
  updated: number
  updated_by: string
}

export interface ConfigHexState {
  name: string
  id: string
  role: string
  status: string
  is_self: boolean
  config: { version: number; sha256: string; state: string; error?: string | null; rejected_version?: number | null }
}

export interface LatticeConfig {
  version: number
  sha256: string
  valid: boolean
  problem: string | null
  time: number
  author: string
  comment: string
  lattice: Record<string, unknown>
  schema: ConfigDef[]
  hexes: ConfigHexState[]
  changes?: { version: number; changed: string[]; cleared: string[]; secrets_set: string[]; secrets_cleared: string[] }
}

export interface ConfigVersionSummary {
  version: number
  parent?: number
  time?: number
  author?: string
  comment?: string
  rollback_of?: number | null
  sha256: string
  valid: boolean
  problem?: string | null
  diff?: { changed: string[]; removed: string[] }
}

export interface AiUsage {
  month: string
  input_tokens: number
  output_tokens: number
  total_tokens: number
  calls: number
  failures: number
  budget: number
  remaining: number | null
  features: Record<string, { input_tokens?: number; output_tokens?: number; calls?: number; failures?: number }>
}

export interface AiStatus {
  ai: { configured: boolean; provider?: string; model?: string; base_url?: string; problem?: string }
  usage: AiUsage
}

export interface AiTestResult {
  ok: boolean
  provider?: string
  model?: string
  reply?: string
  input_tokens?: number
  output_tokens?: number
  millis?: number
  error?: string
}

export interface AiEstimate {
  send: string[]
  frames: number
  tessellations: {
    tessellation: string
    files: number
    tagging: { untagged: number; tokens_for_untagged: number; tokens_per_file: number }
    video_thumbnails: { videos: number; without_ai_thumbnail: number; tokens_for_those: number; tokens_per_video: number }
  }[]
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

export interface QueueConfig {
  name: string
  description?: string
  visibility_seconds: number
  max_receives: number
  dead_letter?: string | null
  retention_hours: number
  created?: number
  created_by?: string
}

export interface QueueStats {
  queue: QueueConfig
  ready: number
  in_flight: number
  delayed: number
  oldest_sent_at: number | null
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
  /** Field paths to return. */
  fields?: string[]
  /** false skips counting every match. */
  total?: boolean
}

/** A file in a files tessellation: its document. */
export interface FileDoc {
  id: string
  name: string
  path: string
  size: number
  content_type: string
  sha256: string
  version: number
  versions?: FileVersion[]
  created_at?: string
  modified_at?: string
  uploaded_at?: string
  uploaded_by?: string
  tags?: string[]
  description?: string
  thumbnail?: { chunk: string; width: number; height: number; content_type: string; chosen_by?: "ai"; frame?: number; of?: number }
  /** AI tags and description (see the lattice configuration's ai.tagging settings). */
  ai?: { status: "done" | "failed" | "skipped"; tags?: string[]; description?: string; model?: string; input_tokens?: number; output_tokens?: number; error?: string; at?: string }
  meta?: Record<string, unknown>
  extract?: { status: "pending" | "done"; version?: number; extractors?: string[]; errors?: string[]; at?: string }
  /** Trashed files only. */
  trashed_at?: string
  trashed_by?: string
  _expires_at?: string
  [field: string]: unknown
}

export interface FileVersion {
  version: number
  size: number
  content_type: string
  sha256: string
  modified_at?: string
  uploaded_at?: string
  uploaded_by?: string
}

export interface UploadSession {
  id: string
  path: string
  offset: number
  size: number | null
  expires_at: string | null
}

/** Uploads at least this large use a resumable session, in parts of `UPLOAD_PART` bytes. */
export const RESUMABLE_FROM = 16 * 1024 * 1024
export const UPLOAD_PART = 8 * 1024 * 1024

/** Path of a file's content (or `kind` = thumbnail) for <img>/<audio>/<video> and links. */
export function fileUrl(tess: string, id: string, kind: "content" | "thumbnail" = "content", query = ""): string {
  return `/${enc(tess)}/${enc(id)}/${kind}${query}`
}

/** Encode a folder path plus file name for /_files/ URLs, keeping the slashes. */
export function filePathUrl(folder: string, name: string): string {
  return `${folder}${name}`.split("/").filter(Boolean).map(enc).join("/")
}

/** PUT bytes with upload progress (fetch can't report it). */
function putWithProgress(url: string, method: string, body: Blob, onProgress: (sent: number) => void, headers: Record<string, string> = {}): Promise<unknown> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest()
    xhr.open(method, url)
    for (const [k, v] of Object.entries(headers)) xhr.setRequestHeader(k, v)
    xhr.upload.onprogress = (e) => onProgress(e.loaded)
    xhr.onerror = () => reject(new ApiError(0, "unreachable", "The upload was interrupted."))
    xhr.onload = () => {
      let json: unknown = undefined
      try {
        json = xhr.responseText ? JSON.parse(xhr.responseText) : undefined
      } catch {
        // not JSON
      }
      if (xhr.status >= 200 && xhr.status < 300) resolve(json)
      else {
        const error = (json as { error?: { code?: string; message?: string } } | undefined)?.error
        if (xhr.status === 401) window.dispatchEvent(new Event(UNAUTHORIZED_EVENT))
        reject(new ApiError(xhr.status, error?.code ?? "error", error?.message ?? xhr.statusText))
      }
    }
    xhr.send(body)
  })
}

/**
 * Upload one file to `folder` (e.g. "/photos/2024/"). Small files go in one
 * request; large ones in resumable parts. `onProgress` gets bytes sent.
 */
export async function uploadFile(tess: string, folder: string, file: File, onProgress: (sent: number) => void): Promise<FileDoc> {
  const modified = new Date(file.lastModified || Date.now()).toISOString()
  if (file.size < RESUMABLE_FROM) {
    const url = `/${enc(tess)}/_files/${filePathUrl(folder, file.name)}?modified=${enc(modified)}`
    return (await putWithProgress(url, "PUT", file, onProgress, file.type ? { "Content-Type": file.type } : {})) as FileDoc
  }
  const session = await request<UploadSession>("POST", `/${enc(tess)}/_uploads`, {
    path: `${folder}${file.name}`,
    size: file.size,
    content_type: file.type || undefined,
    modified,
  })
  let offset = 0
  while (offset < file.size) {
    const end = Math.min(offset + UPLOAD_PART, file.size)
    const start = offset
    // A failed part is retried a few times from where the server says it is.
    for (let attempt = 0; ; attempt++) {
      try {
        await putWithProgress(`/${enc(tess)}/_uploads/${enc(session.id)}?offset=${start}`, "PATCH", file.slice(start, end), (sent) => onProgress(start + sent))
        break
      } catch (e) {
        if (attempt >= 3) throw e
        const status = await request<UploadSession>("GET", `/${enc(tess)}/_uploads/${enc(session.id)}`)
        if (status.offset >= end) break
        await new Promise((r) => setTimeout(r, 1000 * (attempt + 1)))
      }
    }
    offset = end
  }
  return request<FileDoc>("POST", `/${enc(tess)}/_uploads/${enc(session.id)}/complete`)
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
  /** Values role filters can refer to: {"$user": "attributes.region"}. */
  attributes?: Record<string, unknown>
  /** Upload quotas (null: the lattice's defaults; 0: no limit). */
  quota_mb?: number | null
  quota_files?: number | null
}

/** `GET /auth/quota`: your upload quota and what you use of it. */
export interface QuotaView {
  limit_bytes: number | null
  limit_files: number | null
  used_bytes: number
  used_files: number
}

/** A role's row filter and hidden fields on one tessellation ("*" for all). */
export interface Restriction {
  filter?: Record<string, unknown>
  hide?: string[]
  /** Fields shown masked, e.g. `{ "card": { "show_last": 4 } }` reads as `************1234`. */
  mask?: Record<string, { show_first?: number; show_last?: number }>
  folders?: string[]
}

export interface Role {
  id: string
  name: string
  description: string
  permissions: Action[]
  restrictions?: Record<string, Restriction>
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
  restrictions?: Record<string, Restriction>
}

export interface NewUser {
  login: string
  password: string
  email_address: string
  roles: RoleGrant[]
  attributes?: Record<string, unknown>
  quota_mb?: number | null
  quota_files?: number | null
}

export type UserChanges = Partial<{
  login: string
  password: string
  email_address: string
  roles: RoleGrant[]
  is_locked: boolean
  use_mfa: boolean
  attributes: Record<string, unknown>
  quota_mb: number | null
  quota_files: number | null
}>

export interface Trigger {
  name: string
  description: string
  tessellation: string
  events: ("insert" | "update" | "delete")[]
  timing: "before" | "after"
  function: string
  filter?: Record<string, unknown>
  enabled: boolean
  run_as_login?: string
  updated?: number
  status?: { runs: number; failures: number; rejected: number; last_run: number | null; last_error: string | null; last_ms: number }
}

export interface SqlRequest {
  sql: string
  /** Values for `?` or `$1` placeholders, in order. */
  params?: unknown[]
  page_size?: number
  cursor?: string
}

export interface SqlColumn {
  name: string
  /** Inferred from the page's values: boolean, integer, number, string, json or null. */
  type: string
  nullable?: boolean
  source?: "id" | "schema" | "sample"
}

export interface SqlResult {
  columns: SqlColumn[]
  rows: unknown[][]
  /** A cursor for the next page, or null on the last one. */
  next: string | null
  /** The filter, sort and aggregation the statement was translated into. */
  translated: Record<string, unknown>
}

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

export const api = {
  me: () => request<Me>("GET", "/auth/me"),
  chatStatus: () => request<ChatStatus>("GET", "/ai/chat"),
  quota: () => request<QuotaView>("GET", "/auth/quota"),
  shares: (all = false) => request<{ shares: ShareLink[] }>("GET", `/shares${all ? "?all=true" : ""}`),
  createShare: (body: { tessellation: string; file_id?: string; folder?: string; expires_in_days?: number; password?: string; max_downloads?: number; note?: string }) =>
    request<ShareLink>("POST", "/shares", body),
  deleteShare: (code: string) => request<void>("DELETE", `/shares/${encodeURIComponent(code)}`),
  chat: (question: string, history: { role: "user" | "assistant"; content: string }[]) => request<ChatAnswer>("POST", "/ai/chat", { question, history }),
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
  createTessellation: (name: string, kind: "user" | "files" = "user") => request<Tessellation>("POST", "/tessellations", { name, kind }),
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
  aggregate: (tess: string, aggregation: unknown) =>
    request<{ rows: Record<string, unknown>[]; total_groups: number; matched: number }>("POST", `/${enc(tess)}/_aggregate`, aggregation),

  files: (tess: string, query: DocumentQuery) =>
    request<{ documents: FileDoc[]; total: number | null; next: string | null }>("POST", `/${enc(tess)}/_query`, query),
  file: (tess: string, id: string) => request<FileDoc>("GET", `/${enc(tess)}/${enc(id)}`),
  patchFile: (tess: string, id: string, changes: Partial<Pick<FileDoc, "name" | "path" | "tags" | "description">>) =>
    request<FileDoc>("PATCH", `/${enc(tess)}/${enc(id)}`, changes),
  trashFile: (tess: string, id: string) => request<void>("DELETE", `/${enc(tess)}/${enc(id)}`),
  trash: (tess: string) => request<{ documents: FileDoc[]; total: number }>("GET", `/${enc(tess)}/_trash`),
  restoreFile: (tess: string, id: string, name?: string) =>
    request<FileDoc>("POST", `/${enc(tess)}/_trash/${enc(id)}/restore${name ? `?name=${enc(name)}` : ""}`),
  purgeFile: (tess: string, id: string) => request<void>("DELETE", `/${enc(tess)}/_trash/${enc(id)}`),
  emptyTrash: (tess: string) => request<{ purged: number }>("DELETE", `/${enc(tess)}/_trash`),
  restoreVersion: (tess: string, id: string, version: number) => request<FileDoc>("POST", `/${enc(tess)}/${enc(id)}/versions/${version}/restore`),
  reextract: (tess: string, id?: string) => request<{ queued: number }>("POST", `/${enc(tess)}/_reextract${id ? `?id=${enc(id)}` : ""}`),

  users: () => request<{ users: User[] }>("GET", "/users").then((r) => r.users),
  createUser: (user: NewUser) => request<User>("POST", "/users", user),
  updateUser: (id: string, changes: UserChanges) => request<User>("PATCH", `/users/${enc(id)}`, changes),
  deleteUser: (id: string) => request<void>("DELETE", `/users/${enc(id)}`),

  roles: () => request<{ roles: Role[] }>("GET", "/roles").then((r) => r.roles),
  roleCatalog: () => request<{ roles: Role[]; permissions: PermissionInfo[] }>("GET", "/roles"),

  settings: () => request<SettingsResponse>("GET", "/settings"),
  saveSettings: (changes: Record<string, number | boolean | null>) => request<SettingsResponse>("PUT", "/settings", changes),
  config: () => request<LatticeConfig>("GET", "/config"),
  saveConfig: (changes: Record<string, unknown>, options: { comment?: string; base_version?: number }) =>
    request<LatticeConfig>("PUT", "/config", { changes, ...options }),
  configHistory: (limit = 50) => request<{ versions: ConfigVersionSummary[]; total: number }>("GET", `/config/history?limit=${limit}`),
  aiStatus: () => request<AiStatus>("GET", "/ai"),
  aiTest: () => request<AiTestResult>("POST", "/ai/test", {}),
  aiEstimate: () => request<AiEstimate>("GET", "/ai/estimate"),
  aiTagFiles: (tess: string) => request<{ queued: number }>("POST", `/${enc(tess)}/_ai_tag`),
  aiPickThumbnails: (tess: string) => request<{ queued: number }>("POST", `/${enc(tess)}/_ai_thumbnails`),
  rollbackConfig: (version: number, comment?: string) => request<LatticeConfig>("POST", "/config/rollback", { version, comment }),
  joinInfo: (password: string) => request<JoinInfo>("POST", "/join", { password }),

  analyzers: () => request<{ analyzers: Analyzer[] }>("GET", "/analyzers").then((r) => r.analyzers),
  analyze: (analyzer: string, text: string) =>
    request<{ index_tokens: string[]; query_tokens: string[]; pipeline: string }>("POST", "/analyzers/_analyze", { analyzer, text }),
  advice: (tess: string, ai = false) => request<Advice>("GET", `/tessellations/${enc(tess)}/advice${ai ? "?ai=true" : ""}`),
  schemas: (tess: string) => request<Schemas>("GET", `/tessellations/${enc(tess)}/schemas`),
  addSchema: (tess: string, schema: unknown) => request<SchemaVersion>("POST", `/tessellations/${enc(tess)}/schemas`, schema),
  inferSchema: (tess: string) => request<InferredSchema>("GET", `/tessellations/${enc(tess)}/schemas/infer`),
  checkSchema: (tess: string, schema: unknown) => request<SchemaCheck>("POST", `/tessellations/${enc(tess)}/schemas/check`, schema),
  dropSchemas: (tess: string) => request<void>("DELETE", `/tessellations/${enc(tess)}/schemas`),
  /** The values behind vault tokens (null for tokens that aren't vault tokens of this tessellation). Audited. */
  detokenize: (tess: string, tokens: string[]) =>
    request<{ values: Record<string, string | null> }>("POST", `/tessellations/${enc(tess)}/detokenize`, { tokens }).then((r) => r.values),
  savePendingSchema: (tess: string, schema: unknown) => request<Schemas>("PUT", `/tessellations/${enc(tess)}/schemas/pending`, schema),
  applyPendingSchema: (tess: string) => request<SchemaVersion>("POST", `/tessellations/${enc(tess)}/schemas/pending/apply`),
  discardPendingSchema: (tess: string) => request<void>("DELETE", `/tessellations/${enc(tess)}/schemas/pending`),
  updates: () => request<Updates>("GET", "/updates"),
  startRollout: () => request<Rollout>("POST", "/updates/rollout"),
  rollbackSchema: (tess: string, to: number, migration: unknown[] = []) =>
    request<SchemaVersion>("POST", `/tessellations/${enc(tess)}/schemas/rollback`, { to, migration }),

  triggers: () => request<{ triggers: Trigger[] }>("GET", "/triggers").then((r) => r.triggers),
  createTrigger: (trigger: Trigger) => request<Trigger>("POST", "/triggers", trigger),
  updateTrigger: (name: string, trigger: Trigger) => request<Trigger>("PUT", `/triggers/${enc(name)}`, trigger),
  deleteTrigger: (name: string) => request<void>("DELETE", `/triggers/${enc(name)}`),

  queues: () => request<{ queues: QueueConfig[] }>("GET", "/queues").then((r) => r.queues),
  queue: (name: string) => request<QueueStats>("GET", `/queues/${enc(name)}`),
  createQueue: (config: Omit<QueueConfig, "created" | "created_by">) => request<QueueConfig>("POST", "/queues", config),
  deleteQueue: (name: string) => request<void>("DELETE", `/queues/${enc(name)}`),
  sendToQueue: (name: string, messages: { body: unknown; delay_seconds?: number }[]) => request<{ ids: (string | null)[] }>("POST", `/queues/${enc(name)}/messages`, { messages }),
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
  sql: (query: SqlRequest) => request<SqlResult>("POST", "/sql", query),
  sqlTables: () => request<{ tables: { name: string }[] }>("GET", "/sql/tables").then((r) => r.tables.map((t) => t.name)),
  sqlColumns: (table: string) => request<{ table: string; columns: SqlColumn[] }>("GET", `/sql/columns?table=${enc(table)}`).then((r) => r.columns),
  createRole: (role: RoleInput) => request<Role>("POST", "/roles", role),
  updateRole: (name: string, role: RoleInput) => request<Role>("PATCH", `/roles/${enc(name)}`, role),
  deleteRole: (name: string) => request<void>("DELETE", `/roles/${enc(name)}`),
}

/** A readable message for any error. */
export function errorMessage(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}
