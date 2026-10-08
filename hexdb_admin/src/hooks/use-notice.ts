import { useEffect, useState } from "react"

/**
 * The server's `ui.notice`, if it has one: a short line operators set in the
 * config (for example that a shared demo resets every hour). `/health` returns
 * it to everyone, so the sign-in screen can show it too.
 */
export function useNotice(): string | null {
  const [notice, setNotice] = useState<string | null>(null)
  useEffect(() => {
    let alive = true
    fetch("/health")
      .then((r) => (r.ok ? r.json() : null))
      .then((body: { notice?: unknown } | null) => {
        if (alive && body && typeof body.notice === "string" && body.notice.trim()) setNotice(body.notice.trim())
      })
      .catch(() => undefined)
    return () => {
      alive = false
    }
  }, [])
  return notice
}
