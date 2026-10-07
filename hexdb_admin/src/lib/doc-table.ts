// Turning JSON results into table rows and columns.

const MAX_TABLE_COLUMNS = 20
/** Document metadata shown after the document's own fields. */
const META_COLUMNS = ["tessellation", "expiresAt", "_expires_at"]

export interface TableData {
  /** Where the rows were found in a GraphQL result (e.g. "documents.documents"). */
  path: string
  columns: string[]
  rows: Record<string, unknown>[]
}

/** Find the first array of objects in a result, depth-first. */
function findRows(value: unknown, path: string[] = []): { path: string[]; items: Record<string, unknown>[] } | null {
  if (Array.isArray(value)) {
    if (value.length > 0 && value.every((v) => v !== null && typeof v === "object" && !Array.isArray(v))) {
      return { path, items: value as Record<string, unknown>[] }
    }
    return null
  }
  if (value !== null && typeof value === "object") {
    for (const [key, child] of Object.entries(value)) {
      const found = findRows(child, [...path, key])
      if (found) return found
    }
  }
  return null
}

/** Pick and order columns: id first, then scalar fields, then objects/arrays, then metadata. */
export function columnsFor(rows: Record<string, unknown>[]): string[] {
  const sample = rows.slice(0, 200)
  const columns: string[] = []
  for (const row of sample) {
    for (const key of Object.keys(row)) {
      if (!columns.includes(key)) columns.push(key)
    }
  }
  const isComplex = (key: string) => {
    const value = sample.find((row) => row[key] !== null && row[key] !== undefined)?.[key]
    return typeof value === "object"
  }
  const rank = (key: string) => (key === "id" ? 0 : META_COLUMNS.includes(key) ? 3 : isComplex(key) ? 2 : 1)
  return columns
    .filter((key) => sample.some((row) => row[key] !== null && row[key] !== undefined))
    .sort((a, b) => rank(a) - rank(b))
    .slice(0, MAX_TABLE_COLUMNS)
}

/** Turn a GraphQL result into a table, flattening document `json`/`data` fields into columns. */
export function toTable(data: unknown): TableData | null {
  const found = findRows(data)
  if (!found) return null

  const rows = found.items.map((item) => {
    const doc = (item.json ?? item.data) as Record<string, unknown> | undefined
    if (doc && typeof doc === "object" && !Array.isArray(doc)) {
      const rest = Object.fromEntries(Object.entries(item).filter(([key]) => key !== "json" && key !== "data"))
      return { ...rest, ...doc }
    }
    return item
  })
  return { path: found.path.join("."), columns: columnsFor(rows), rows }
}

