import { toast } from "sonner"

import { api, errorMessage } from "@/lib/api"
import { formatNumber } from "@/lib/format"

function list(counts: Record<string, number>): string {
  return Object.entries(counts)
    .map(([tess, n]) => `${formatNumber(n)} in ${tess}`)
    .join(", ")
}

/** Show an undo toast for a delete batch. */
export function toastDeleted(summary: { total: number; batch?: string | null }, onRestored?: () => void) {
  const batch = summary.batch
  toast.success(`Deleted ${formatNumber(summary.total)} document(s) and file(s). They're in the trash.`, {
    duration: 10_000,
    action: batch
      ? {
          label: "Undo",
          onClick: () =>
            void api
              .restoreBatch(batch)
              .then((r) => toast.success(`Restored ${list(r.restored)}.`))
              .then(() => onRestored?.())
              .catch((e: unknown) => toast.error(errorMessage(e))),
        }
      : undefined,
  })
}
