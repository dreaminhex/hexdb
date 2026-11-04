const UNITS = ["B", "KB", "MB", "GB", "TB"]

/** 1536 -> "1.5 KB". */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B"
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), UNITS.length - 1)
  const value = bytes / 1024 ** exponent
  return `${value >= 100 || exponent === 0 ? value.toFixed(0) : value.toFixed(1)} ${UNITS[exponent]}`
}

const numberFormat = new Intl.NumberFormat()

/** 104678 -> "104,678". */
export function formatNumber(value: number): string {
  return numberFormat.format(value)
}

/** A time lag: "7 ms", "1.2 s", "3 min". */
export function formatLagMs(ms: number): string {
  if (ms < 1000) return `${Math.round(ms)} ms`
  if (ms < 60_000) return `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)} s`
  if (ms < 3_600_000) return `${Math.round(ms / 60_000)} min`
  return `${(ms / 3_600_000).toFixed(1)} h`
}

/**
 * A streaming replica's lag: "in sync · 0 ms", "7 ms behind (12 changes)", or, from
 * hexes before 2.3.3 (no time), "in sync" and "12 changes behind".
 */
export function formatLag(changes: number | null | undefined, ms: number | null | undefined): string | undefined {
  const known = ms !== null && ms !== undefined
  if (changes === 0) return known ? `in sync · ${formatLagMs(ms)}` : "in sync"
  if (changes === null || changes === undefined) return known ? `${formatLagMs(ms)} behind` : undefined
  const count = `${formatNumber(changes)} ${changes === 1 ? "change" : "changes"}`
  return known ? `${formatLagMs(ms)} behind (${count})` : `${count} behind`
}

/** 93784 -> "1d 2h 3m". */
export function formatDuration(seconds: number): string {
  const d = Math.floor(seconds / 86400)
  const h = Math.floor((seconds % 86400) / 3600)
  const m = Math.floor((seconds % 3600) / 60)
  if (d > 0) return `${d}d ${h}h ${m}m`
  if (h > 0) return `${h}h ${m}m`
  if (m > 0) return `${m}m ${Math.floor(seconds % 60)}s`
  return `${Math.floor(seconds)}s`
}

/** Epoch seconds (0 = never) -> locale date-time. */
export function formatEpochSeconds(seconds: number): string {
  return seconds > 0 ? new Date(seconds * 1000).toLocaleString() : "Never"
}

/** ISO timestamp -> locale date-time. */
export function formatTimestamp(iso: string | null | undefined): string {
  return iso ? new Date(iso).toLocaleString() : "—"
}

/** 0.4567 -> "46%". */
export function formatPercent(ratio: number): string {
  return `${Math.round(Math.max(0, Math.min(1, ratio)) * 100)}%`
}
