import { useEffect, useState } from "react"
import { IconInfoCircle } from "@tabler/icons-react"

import { cn } from "@/lib/utils"

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

/** A slim bar with the notice; renders nothing when there is none. */
export function NoticeBar({ className }: { className?: string }) {
  const notice = useNotice()
  if (!notice) return null
  return (
    <div
      role="note"
      className={cn(
        "flex shrink-0 items-center justify-center gap-2 border-b border-amber-500/30 bg-amber-500/10 px-4 py-1.5 text-center text-xs text-amber-900 dark:text-amber-200",
        className,
      )}
    >
      <IconInfoCircle className="size-3.5 shrink-0" aria-hidden />
      <span>{notice}</span>
    </div>
  )
}
