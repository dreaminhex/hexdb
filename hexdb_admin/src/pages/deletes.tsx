import { useState } from "react"
import { IconAlertTriangle, IconEye, IconRestore, IconTrashX } from "@tabler/icons-react"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/confirm-dialog"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { usePoll } from "@/hooks/use-poll"
import { api, type DeleteBatch, errorMessage } from "@/lib/api"
import { has, useAuth } from "@/lib/auth"
import { formatNumber } from "@/lib/format"

const KINDS = [
  ["all", "Every kind"],
  ["delete", "Single deletes"],
  ["filter", "By filter"],
  ["transaction", "Transactions"],
  ["folder", "Folders"],
] as const

type Action = { kind: "restore" | "purge"; batch: DeleteBatch }

function state(b: DeleteBatch): {
  label: string
  variant: "default" | "secondary" | "outline" | "destructive"
} {
  if (b.purged_at) return { label: "Purged", variant: "destructive" }
  if (b.restored_at) return { label: "Restored", variant: "secondary" }
  if (!b.trashed) return { label: "Not kept", variant: "outline" }
  return { label: "In trash", variant: "default" }
}

function counts(c: Record<string, number>): string {
  return Object.entries(c)
    .map(([tess, n]) => `${formatNumber(n)} in ${tess}`)
    .join(", ")
}

/** The delete history: every delete, what it removed, and restoring or purging it. */
export function DeletesPage() {
  const { me } = useAuth()
  const [kind, setKind] = useState<string>("all")
  const [tessellation, setTessellation] = useState("")
  const [applied, setApplied] = useState("")
  const batches = usePoll(
    () =>
      api.deletes({
        kind: kind === "all" ? undefined : kind,
        tessellation: applied || undefined,
        limit: 200,
      }),
    15_000,
    [kind, applied],
  )
  const status = usePoll(() => api.status(), 60_000, [])
  const trashWarning = status.data?.storage.trash_warning
  const [action, setAction] = useState<Action | null>(null)
  const [details, setDetails] = useState<DeleteBatch | null>(null)

  const showDetails = (b: DeleteBatch) =>
    void api
      .deleteBatch(b.batch)
      .then(setDetails)
      .catch((e: unknown) => toast.error(errorMessage(e)))

  return (
    <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
      <p className="text-muted-foreground max-w-3xl text-sm">
        Every delete goes to the trash, with what was deleted along with it through parent links, as one batch. Restore a batch to put all of it back at once,
        or purge it to delete it for good. Deleted documents stay in the trash for storage.trash_days (30 by default).
        {!has(me, "audit") && " You see your own deletes; the audit permission shows everyone's."}
      </p>
      {trashWarning && (
        <p className="flex items-center gap-2 text-sm text-amber-600 dark:text-amber-400">
          <IconAlertTriangle className="size-4 shrink-0" /> {trashWarning}
        </p>
      )}
      <div className="flex flex-wrap items-center gap-2">
        <Select value={kind} onValueChange={setKind}>
          <SelectTrigger size="sm" className="w-44" aria-label="Kind">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {KINDS.map(([value, label]) => (
              <SelectItem key={value} value={value}>
                {label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <form
          onSubmit={(e) => {
            e.preventDefault()
            setApplied(tessellation.trim())
          }}
        >
          <Input
            value={tessellation}
            onChange={(e) => setTessellation(e.target.value)}
            placeholder="Tessellation"
            className="h-8 w-48 text-xs"
            aria-label="Tessellation"
          />
        </form>
      </div>
      {batches.error && <p className="text-destructive text-sm">{batches.error.message}</p>}
      <Card className="p-0">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>When</TableHead>
              <TableHead>Kind</TableHead>
              <TableHead>By</TableHead>
              <TableHead>Deleted</TableHead>
              <TableHead>State</TableHead>
              <TableHead className="w-28" />
            </TableRow>
          </TableHeader>
          <TableBody>
            {batches.data?.batches.length === 0 && (
              <TableRow>
                <TableCell colSpan={6} className="text-muted-foreground text-center text-sm">
                  No deletes in the history.
                </TableCell>
              </TableRow>
            )}
            {batches.data?.batches.map((b) => {
              const s = state(b)
              const restorable = b.trashed && !b.restored_at && !b.purged_at
              return (
                <TableRow key={b.batch}>
                  <TableCell className="text-xs whitespace-nowrap">{new Date(b.at).toLocaleString()}</TableCell>
                  <TableCell className="text-xs">{b.kind}</TableCell>
                  <TableCell className="text-xs">{b.by}</TableCell>
                  <TableCell className="text-xs">
                    {counts(b.counts)}
                    {b.detached?.length > 0 && <span className="text-muted-foreground">; {b.detached.length} unlinked</span>}
                  </TableCell>
                  <TableCell>
                    <Badge variant={s.variant}>{s.label}</Badge>
                  </TableCell>
                  <TableCell>
                    <div className="flex justify-end gap-1">
                      <Button variant="ghost" size="icon" className="size-8" aria-label="Details" title="Details" onClick={() => showDetails(b)}>
                        <IconEye className="size-4" />
                      </Button>
                      <Button
                        variant="ghost"
                        size="icon"
                        className="size-8"
                        aria-label="Restore"
                        title="Restore everything this delete removed"
                        disabled={!restorable}
                        onClick={() => setAction({ kind: "restore", batch: b })}
                      >
                        <IconRestore className="size-4" />
                      </Button>
                      <Button
                        variant="ghost"
                        size="icon"
                        className="size-8"
                        aria-label="Purge"
                        title="Delete for good"
                        disabled={!restorable}
                        onClick={() => setAction({ kind: "purge", batch: b })}
                      >
                        <IconTrashX className="size-4" />
                      </Button>
                    </div>
                  </TableCell>
                </TableRow>
              )
            })}
          </TableBody>
        </Table>
      </Card>
      {batches.data && batches.data.total > batches.data.batches.length && (
        <p className="text-muted-foreground text-xs">
          The newest {batches.data.batches.length} of {formatNumber(batches.data.total)} are shown.
        </p>
      )}

      <Dialog open={details !== null} onOpenChange={(open) => !open && setDetails(null)}>
        <DialogContent className="max-h-[90svh] overflow-y-auto sm:max-w-2xl">
          <DialogHeader>
            <DialogTitle>Delete batch</DialogTitle>
            <DialogDescription className="font-mono text-xs">{details?.batch}</DialogDescription>
          </DialogHeader>
          <pre className="bg-muted overflow-x-auto rounded-md p-3 text-xs">{JSON.stringify(details, null, 2)}</pre>
        </DialogContent>
      </Dialog>

      <ConfirmDialog
        open={action !== null}
        onOpenChange={(open) => !open && setAction(null)}
        title={action?.kind === "restore" ? "Restore this delete?" : "Purge this delete for good?"}
        description={
          action?.kind === "restore"
            ? `Puts back ${counts(action.batch.counts)} in one step${action.batch.detached?.length ? ", and sets the unlinked parent links back" : ""}. If a document with the same ID exists again, nothing is restored.`
            : `${counts(action?.batch.counts ?? {})} will be deleted for good. This can't be undone.`
        }
        confirmLabel={action?.kind === "restore" ? "Restore" : "Purge"}
        confirmText={action?.kind === "purge" ? "purge" : undefined}
        onConfirm={async () => {
          if (!action) return
          if (action.kind === "restore") {
            const r = await api.restoreBatch(action.batch.batch)
            toast.success(`Restored ${counts(r.restored)}.`)
          } else {
            const r = await api.purgeBatch(action.batch.batch)
            toast.success(`Purged ${formatNumber(r.purged)} for good.`)
          }
          await batches.refresh()
        }}
      />
    </div>
  )
}
