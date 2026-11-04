// Messages between the extension and its webviews.

export interface Column {
  name: string
  type?: string
}

/** One run shown as a tab in the results panel. */
export interface ResultTab {
  id: string
  title: string
  connection: string
  /** What ran (SQL, GraphQL or a function call), shown above the results. */
  source: string
  ms?: number
  error?: string
  /** Tabular results (SQL, or GraphQL/function results that are a list of objects). */
  columns?: Column[]
  rows?: unknown[][]
  /** The raw JSON result, for the JSON view. */
  json?: unknown
  /** More rows can be fetched. */
  hasMore?: boolean
  /** The tessellation the rows come from, when known: their ids open the documents. */
  tessellation?: string
  /** Server notices (a query plan, a truncated result). */
  notes?: string[]
}

export type ToResults =
  | { type: "show"; tab: ResultTab }
  | { type: "update"; tab: ResultTab }

export type FromResults =
  | { type: "ready" }
  | { type: "loadMore"; id: string }
  | { type: "openDocument"; id: string; docId: string }
  | { type: "export"; id: string; format: "csv" | "json" }
  | { type: "close"; id: string }

export interface HexView {
  id: string
  name: string
  role: string
  status: string
  region: string
  city?: string
  country?: string
  version?: string
  lag?: number | null
  /** Lag in time, in ms (HexDB 2.3.3+). */
  lagMs?: number | null
  slicesHeld?: number | null
  slicing?: boolean
  replicationState?: string
  lastSeen?: string | null
  isSelf: boolean
  preference?: string
}

export interface LatticeView {
  connection: string
  url: string
  name: string
  answeredBy: string
  version: string
  uptimeSeconds?: number
  regions: { code: string; city?: string; country?: string }[]
  hexes: HexView[]
  fetchedAt: string
  error?: string
}

export type ToLattice = { type: "lattice"; lattice: LatticeView }
export type FromLattice = { type: "ready" } | { type: "refresh" }
