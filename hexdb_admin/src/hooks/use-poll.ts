import { useCallback, useEffect, useRef, useState } from "react"

export interface PollState<T> {
  data: T | undefined
  error: Error | undefined
  /** True until the first response (success or failure). */
  loading: boolean
  refresh: () => Promise<void>
}

/**
 * Load data and optionally refresh it every `intervalMs` (0 = load once).
 * Keeps the last good data while a refresh is in flight or fails, and pauses
 * while the tab is hidden.
 */
export function usePoll<T>(load: () => Promise<T>, intervalMs = 0, deps: unknown[] = []): PollState<T> {
  const [data, setData] = useState<T>()
  const [error, setError] = useState<Error>()
  const [loading, setLoading] = useState(true)
  const loadRef = useRef(load)
  loadRef.current = load
  const generation = useRef(0)

  const refresh = useCallback(async () => {
    const mine = ++generation.current
    try {
      const result = await loadRef.current()
      if (mine !== generation.current) return
      setData(result)
      setError(undefined)
    } catch (e) {
      if (mine !== generation.current) return
      setError(e instanceof Error ? e : new Error(String(e)))
    } finally {
      if (mine === generation.current) setLoading(false)
    }
  }, [])

  useEffect(() => {
    setLoading(true)
    void refresh()
    if (!intervalMs) return
    const timer = window.setInterval(() => {
      if (document.visibilityState === "visible") void refresh()
    }, intervalMs)
    return () => window.clearInterval(timer)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [intervalMs, refresh, ...deps])

  return { data, error, loading, refresh }
}
