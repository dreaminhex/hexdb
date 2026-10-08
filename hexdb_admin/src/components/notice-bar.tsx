import { IconInfoCircle } from "@tabler/icons-react"

import { useNotice } from "@/hooks/use-notice"
import { cn } from "@/lib/utils"

/** A slim bar with the server's `ui.notice`; renders nothing when there is none. */
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
