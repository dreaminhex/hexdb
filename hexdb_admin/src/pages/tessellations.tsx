import { useState } from "react"
import { IconLoader2, IconPlus, IconTrash } from "@tabler/icons-react"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/confirm-dialog"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { usePoll } from "@/hooks/use-poll"
import { api, errorMessage } from "@/lib/api"
import { formatBytes, formatNumber, formatTimestamp } from "@/lib/format"
import { href, linkHandler, navigate } from "@/lib/router"

const NAME_PATTERN = /^[A-Za-z0-9-][A-Za-z0-9_-]{0,63}$/

function CreateTessellationDialog({ open, onOpenChange, onCreated }: { open: boolean; onOpenChange: (open: boolean) => void; onCreated: (name: string) => void }) {
  const [name, setName] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const valid = NAME_PATTERN.test(name)

  const submit = async (event: React.FormEvent) => {
    event.preventDefault()
    if (!valid) return
    setBusy(true)
    setError(null)
    try {
      await api.createTessellation(name)
      toast.success(`Created tessellation '${name}'.`)
      onCreated(name)
      onOpenChange(false)
      setName("")
    } catch (e) {
      setError(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={(next) => !busy && onOpenChange(next)}>
      <DialogContent>
        <form onSubmit={submit} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>New tessellation</DialogTitle>
            <DialogDescription>A tessellation is a collection of documents.</DialogDescription>
          </DialogHeader>
          <div className="grid gap-2">
            <Label htmlFor="tess-name">Name</Label>
            <Input id="tess-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="articles" autoFocus autoComplete="off" />
            <p className="text-muted-foreground text-xs">
              1–64 letters, digits, <code>_</code> or <code>-</code>; can't start with <code>_</code>. Names are unique ignoring case.
            </p>
          </div>
          {error && <p className="text-destructive text-sm">{error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)} disabled={busy}>
              Cancel
            </Button>
            <Button type="submit" disabled={!valid || busy}>
              {busy && <IconLoader2 className="animate-spin" />}
              Create
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

export function TessellationsPage() {
  const tessellations = usePoll(api.tessellations, 10_000)
  const status = usePoll(api.status, 10_000)
  const [creating, setCreating] = useState(false)
  const [deleting, setDeleting] = useState<string | null>(null)

  const metrics = new Map((status.data?.metrics.tessellations ?? []).map((t) => [t.name, t]))
  const rows = [...(tessellations.data ?? [])].sort((a, b) => (a.kind === b.kind ? a.name.localeCompare(b.name) : a.kind === "user" ? -1 : 1))
  const documentsRoute = (name: string) => `/documents?tessellation=${encodeURIComponent(name)}`

  return (
    <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
      <div className="flex items-center gap-2">
        <p className="text-muted-foreground text-sm">
          {tessellations.data ? `${rows.filter((t) => t.kind === "user").length} user · ${rows.filter((t) => t.kind !== "user").length} system` : "Loading…"}
        </p>
        <Button className="ml-auto" size="sm" onClick={() => setCreating(true)}>
          <IconPlus /> New tessellation
        </Button>
      </div>

      {tessellations.error && <p className="text-destructive text-sm">{tessellations.error.message}</p>}

      <Card className="gap-0 overflow-hidden py-0">
        <Table>
          <TableHeader className="bg-muted/60">
            <TableRow>
              <TableHead className="pl-6">Name</TableHead>
              <TableHead>Kind</TableHead>
              <TableHead className="text-right">Documents</TableHead>
              <TableHead className="text-right">Total size</TableHead>
              <TableHead>Created</TableHead>
              <TableHead className="w-12 pr-6" />
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((t) => {
              const m = metrics.get(t.name)
              const user = t.kind === "user"
              return (
                <TableRow
                  key={t.name}
                  className={user ? "cursor-pointer" : undefined}
                  onClick={user ? () => navigate(documentsRoute(t.name)) : undefined}
                >
                  <TableCell className="pl-6 font-medium">
                    {user ? (
                      <a href={href(documentsRoute(t.name))} onClick={(e) => { e.stopPropagation(); linkHandler(documentsRoute(t.name))(e) }} className="hover:underline">
                        {t.name}
                      </a>
                    ) : (
                      <span className="text-muted-foreground" title="Managed by HexDB; use the Users and Roles pages.">
                        {t.name}
                      </span>
                    )}
                  </TableCell>
                  <TableCell>
                    <Badge variant={user ? "outline" : "secondary"}>{t.kind}</Badge>
                  </TableCell>
                  <TableCell className="text-right tabular-nums">{m ? formatNumber(m.document_count) : "—"}</TableCell>
                  <TableCell className="text-right tabular-nums">{m ? formatBytes(m.total_size_bytes) : "—"}</TableCell>
                  <TableCell className="text-muted-foreground text-sm">{formatTimestamp(t.created)}</TableCell>
                  <TableCell className="pr-6">
                    {user && (
                      <Button
                        variant="ghost"
                        size="icon"
                        className="text-muted-foreground hover:text-destructive size-8"
                        aria-label={`Delete ${t.name}`}
                        onClick={(e) => {
                          e.stopPropagation()
                          setDeleting(t.name)
                        }}
                      >
                        <IconTrash />
                      </Button>
                    )}
                  </TableCell>
                </TableRow>
              )
            })}
            {tessellations.data && rows.length === 0 && (
              <TableRow>
                <TableCell colSpan={6} className="text-muted-foreground py-10 text-center">
                  No tessellations yet. Create one, or insert a document and it will be created for you.
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </Card>

      <CreateTessellationDialog
        open={creating}
        onOpenChange={setCreating}
        onCreated={() => {
          void tessellations.refresh()
          void status.refresh()
        }}
      />
      <ConfirmDialog
        open={deleting !== null}
        onOpenChange={(open) => !open && setDeleting(null)}
        title={`Delete '${deleting}'?`}
        description={`This permanently deletes the tessellation and all ${formatNumber(metrics.get(deleting ?? "")?.document_count ?? 0)} of its documents. It can't be undone.`}
        confirmLabel="Delete tessellation"
        confirmText={deleting ?? undefined}
        onConfirm={async () => {
          await api.deleteTessellation(deleting!)
          toast.success(`Deleted tessellation '${deleting}'.`)
          void tessellations.refresh()
          void status.refresh()
        }}
      />
    </div>
  )
}
