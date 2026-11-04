// Formatting shared by the extension and its webviews (no imports, so both can bundle it).

/** A time lag: "7 ms", "1.2 s", "3 min", "1.5 h". */
export function formatLagMs(ms: number): string {
  if (ms < 1000) return `${Math.round(ms)} ms`
  if (ms < 60_000) return `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)} s`
  if (ms < 3_600_000) return `${Math.round(ms / 60_000)} min`
  return `${(ms / 3_600_000).toFixed(1)} h`
}

/**
 * A replica's lag, as the admin dashboard shows it: "In sync · 0 ms", "7 ms behind (12 changes)",
 * or from hexes before 2.3.3 (no time) "In sync" and "12 changes behind". Undefined when unknown.
 */
export function formatLag(changes: number | null | undefined, ms: number | null | undefined): string | undefined {
  const known = ms !== null && ms !== undefined
  if (changes === 0) return known ? `In sync · ${formatLagMs(ms)}` : "In sync"
  if (changes === null || changes === undefined) return known ? `${formatLagMs(ms)} behind` : undefined
  const count = `${changes.toLocaleString("en-US")} ${changes === 1 ? "change" : "changes"}`
  return known ? `${formatLagMs(ms)} behind (${count})` : `${count} behind`
}
