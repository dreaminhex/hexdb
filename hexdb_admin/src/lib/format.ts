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
