import { useEffect, useMemo, useState } from "react"
import { IconAlertTriangle, IconLoader2 } from "@tabler/icons-react"

import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { api, type DeleteSummary, errorMessage } from "@/lib/api"
import { toastDeleted } from "@/lib/deletes"
import { formatNumber } from "@/lib/format"

/** Above this many documents, confirming means typing the tessellation's name. */
const TYPE_TO_CONFIRM = 100

/** What to delete: the documents matching a filter, or a folder of files. */
export type DeleteTarget = { kind: "filter"; filter: unknown } | { kind: "folder"; folder: string; recursive: boolean }

function isEmptyFilter(filter: unknown): boolean {
  return filter === undefined || filter === null || (typeof filter === "object" && !Array.isArray(filter) && Object.keys(filter as object).length === 0)
}

function list(counts: Record<string, number>): string {
  return Object.entries(counts)
    .map(([tess, n]) => `${formatNumber(n)} in ${tess}`)
    .join(", ")
}

/** The delete (or with `dryRun`, its preview), with `all` when it's everything. */
function runDelete(tessellation: string, target: DeleteTarget, everything: boolean, dryRun: boolean, expect?: number): Promise<DeleteSummary> {
  return target.kind === "filter"
    ? api.deleteWhere(tessellation, {
        filter: target.filter ?? {},
        all: everything,
        dry_run: dryRun,
        expect,
      })
    : api.deleteFolder(tessellation, target.folder, {
        recursive: target.recursive,
        all: everything,
        dry_run: dryRun,
        expect,
      })
}

function isEverything(target: DeleteTarget): boolean {
  return target.kind === "filter" ? isEmptyFilter(target.filter) : target.folder === "/" && target.recursive
}

/**
 * Deleting by filter or folder: asks the server what would go (a dry run: the
 * matches and what cascades from them), shows it, and deletes only that many
 * (the request carries the count, so it fails if more would go now).
 */
export function DeletePreviewDialog({
  open,
  onOpenChange,
  tessellation,
  target,
  onDeleted,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  tessellation: string
  target: DeleteTarget
  onDeleted: () => void
}) {
  const [preview, setPreview] = useState<DeleteSummary>()
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [typed, setTyped] = useState("")
  // The same target from render to render while its contents don't change.
  const key = JSON.stringify(target)
  const stable = useMemo(() => JSON.parse(key) as DeleteTarget, [key])
  const everything = isEverything(stable)

  useEffect(() => {
    if (!open) return
    setPreview(undefined)
    setError(null)
    setTyped("")
    let cancelled = false
    runDelete(tessellation, stable, isEverything(stable), true)
      .then((p) => !cancelled && setPreview(p))
      .catch((e: unknown) => !cancelled && setError(errorMessage(e)))
    return () => {
      cancelled = true
    }
  }, [open, tessellation, stable])

  const blocked = preview?.blocked && Object.keys(preview.blocked).length > 0
  const mustType = everything || (preview?.total ?? 0) > TYPE_TO_CONFIRM
  const ready = !!preview && preview.total > 0 && !blocked && (!mustType || typed === tessellation)

  const confirm = async () => {
    if (!preview) return
    setBusy(true)
    setError(null)
    try {
      const done = await runDelete(tessellation, stable, everything, false, preview.total)
      toastDeleted(done, onDeleted)
      onDeleted()
      onOpenChange(false)
    } catch (e) {
      setError(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  const what =
    target.kind === "folder"
      ? `${target.recursive ? "Everything in" : "The files directly in"} ${target.folder}`
      : everything
        ? `Every document in ${tessellation}`
        : `The documents in ${tessellation} matching the filter`

  return (
    <Dialog open={open} onOpenChange={(next) => !busy && onOpenChange(next)}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Delete {what.charAt(0).toLowerCase() + what.slice(1)}?</DialogTitle>
          <DialogDescription>Deleted documents and files go to the trash, and the whole delete can be undone from the Deletes page.</DialogDescription>
        </DialogHeader>
        {!preview && !error && (
          <p className="text-muted-foreground flex items-center gap-2 text-sm">
            <IconLoader2 className="size-4 animate-spin" /> Checking what would be deleted…
          </p>
        )}
        {everything && (
          <p className="text-destructive flex items-start gap-2 text-sm">
            <IconAlertTriangle className="mt-0.5 size-4 shrink-0" />
            {target.kind === "filter" ? "There's no filter: this deletes every document in the tessellation." : "This deletes every file in the tessellation."}
          </p>
        )}
        {preview && (
          <div className="grid gap-2 text-sm">
            {preview.total === 0 ? (
              <p>Nothing matches, so nothing would be deleted.</p>
            ) : (
              <p>
                This deletes <span className="font-semibold">{formatNumber(preview.total)}</span> document(s) and file(s): {list(preview.deleted)}.
                {preview.total > preview.matched && " That includes what belongs to them through parent links."}
              </p>
            )}
            {Object.keys(preview.detached).length > 0 && <p>It also unlinks (sets the parent link to null): {list(preview.detached)}.</p>}
            {blocked && (
              <p className="text-destructive">
                Refused: documents link to these with on_delete restrict (
                {Object.entries(preview.blocked!)
                  .map(([tess, b]) => `${b.count} in ${tess}`)
                  .join(", ")}
                ). Delete or move them first.
              </p>
            )}
            {preview.sample && preview.sample.length > 0 && (
              <p className="text-muted-foreground font-mono text-xs break-all">
                {preview.sample.slice(0, 5).join(", ")}
                {preview.total > 5 && ", …"}
              </p>
            )}
          </div>
        )}
        {preview && preview.total > 0 && !blocked && mustType && (
          <div className="grid gap-2">
            <Label htmlFor="delete-confirm">
              Type <span className="font-mono font-semibold">{tessellation}</span> to confirm
            </Label>
            <Input id="delete-confirm" value={typed} onChange={(e) => setTyped(e.target.value)} autoComplete="off" autoFocus />
          </div>
        )}
        {error && <p className="text-destructive text-sm">{error}</p>}
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)} disabled={busy}>
            Cancel
          </Button>
          <Button variant="destructive" onClick={() => void confirm()} disabled={busy || !ready}>
            {busy && <IconLoader2 className="animate-spin" />}
            Delete {preview ? formatNumber(preview.total) : ""}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
