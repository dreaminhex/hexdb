import { lazy, Suspense, useState } from "react"
import { IconEye, IconEyeOff, IconFileDescription, IconHexagon, IconKey, IconLoader2, IconLock, IconPlus, IconTrash } from "@tabler/icons-react"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/confirm-dialog"
import { IndexesDialog } from "@/components/indexes-dialog"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { usePoll } from "@/hooks/use-poll"
import { api, errorMessage } from "@/lib/api"
import { can, has, useAuth } from "@/lib/auth"
import { formatBytes, formatNumber, formatTimestamp } from "@/lib/format"
import { href, linkHandler, navigate } from "@/lib/router"
import { cn } from "@/lib/utils"

// The schema editor uses CodeMirror, which loads on first use.
const SchemasDialog = lazy(() => import("@/components/schemas-dialog").then((m) => ({ default: m.SchemasDialog })))

const NAME_PATTERN = /^[A-Za-z0-9-][A-Za-z0-9_-]{0,63}$/
/** Whether the system group is expanded; remembered per browser. */
const SHOW_SYSTEM_KEY = "hexdb.tessellations.showSystem"

function loadShowSystem(): boolean {
  try {
    return localStorage.getItem(SHOW_SYSTEM_KEY) === "true"
  } catch {
    return false
  }
}

/** What a system tessellation holds, from its name, for the hint beside it. */
function systemPurpose(name: string): string {
  if (name.startsWith("_stream_")) return `messages of the stream ${name.slice("_stream_".length)}`
  const known: Record<string, string> = {
    users: "accounts (Users page)",
    roles: "roles and grants (Roles page)",
    _revoked_sessions: "signed-out sessions",
    _api_keys: "API keys",
    _login_failures: "sign-in throttling",
    _audit: "the audit trail",
    _streams: "stream configurations",
    _stream_offsets: "stream positions and consumer groups",
    _functions: "saved functions",
    _schedules: "schedules",
    _triggers: "triggers",
    _trigger_cursors: "trigger positions in the change feed",
    _plugin_cursors: "plugin positions in the change feed",
    _idempotency: "idempotency keys and their stored responses",
    _replication: "replication state",
    _system: "runtime settings and server state",
  }
  return known[name] ?? "managed by HexDB"
}

function CreateTessellationDialog({
  open,
  onOpenChange,
  onCreated,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  onCreated: (name: string) => void
}) {
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
            <Input id="tess-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="orders" autoFocus autoComplete="off" />
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
  const { me } = useAuth()
  const tessellations = usePoll(api.tessellations, 10_000)
  // Sizes come from /status (users with the status permission); other users get document counts per tessellation.
  const mayStatus = has(me, "status")
  const status = usePoll(() => (mayStatus ? api.status() : Promise.resolve(undefined)), 10_000, [mayStatus])
  const counts = usePoll(
    async () =>
      mayStatus
        ? {}
        : Object.fromEntries(
            await Promise.all((tessellations.data ?? []).map(async (t) => [t.name, (await api.tessellation(t.name)).document_count ?? 0] as const)),
          ),
    10_000,
    [mayStatus, tessellations.data?.length],
  )
  const mayCreate = me?.is_admin || (me?.grants ?? []).some((g) => g.permissions.includes("write") && g.tessellations.length > 0)
  const [creating, setCreating] = useState(false)
  const [deleting, setDeleting] = useState<string | null>(null)
  const [indexing, setIndexing] = useState<string | null>(null)
  const [schemaFor, setSchemaFor] = useState<string | null>(null)
  const [showSystem, setShowSystem] = useState(loadShowSystem)

  const metrics = new Map((status.data?.metrics.tessellations ?? []).map((t) => [t.name, t]))
  const all = [...(tessellations.data ?? [])].sort((a, b) => a.name.localeCompare(b.name))
  const userRows = all.filter((t) => t.kind === "user")
  const systemRows = all.filter((t) => t.kind !== "user")
  const rows = showSystem ? [...userRows, ...systemRows] : userRows
  const documentsRoute = (name: string) => `/documents?tessellation=${encodeURIComponent(name)}`
  const toggleSystem = () => {
    setShowSystem((v) => {
      try {
        localStorage.setItem(SHOW_SYSTEM_KEY, String(!v))
      } catch {
        // Not critical.
      }
      return !v
    })
  }

  return (
    <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
      <div className="flex flex-wrap items-center gap-2">
        <p className="text-muted-foreground text-sm">
          {tessellations.data ? (
            <>
              {userRows.length} user tessellation{userRows.length === 1 ? "" : "s"}
              {systemRows.length > 0 && (
                <>
                  {" · "}
                  {systemRows.length} system
                </>
              )}
            </>
          ) : (
            "Loading…"
          )}
        </p>
        <div className="ml-auto flex items-center gap-2">
          {systemRows.length > 0 && (
            <Button variant="ghost" size="sm" className="text-muted-foreground" onClick={toggleSystem} aria-pressed={showSystem}>
              {showSystem ? <IconEyeOff /> : <IconEye />}
              {showSystem ? "Hide system" : "Show system"}
            </Button>
          )}
          {mayCreate && (
            <Button size="sm" onClick={() => setCreating(true)}>
              <IconPlus /> New tessellation
            </Button>
          )}
        </div>
      </div>

      {tessellations.error && <p className="text-destructive text-sm">{tessellations.error.message}</p>}

      <Card className="gap-0 overflow-hidden py-0">
        <Table>
          <TableHeader className="bg-muted/60">
            <TableRow>
              <TableHead className="pl-6">Name</TableHead>
              <TableHead className="text-right">Documents</TableHead>
              <TableHead className="text-right">Total size</TableHead>
              <TableHead>Indexes</TableHead>
              <TableHead>Created</TableHead>
              <TableHead className="w-24 pr-6" />
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((t, i) => {
              const m = metrics.get(t.name)
              const user = t.kind === "user"
              const firstSystem = !user && (i === 0 || rows[i - 1].kind === "user")
              return [
                firstSystem && (
                  <TableRow key="__system" className="bg-muted/40 hover:bg-muted/40">
                    <TableCell colSpan={6} className="text-muted-foreground py-2 pl-6 text-xs font-medium tracking-wide uppercase">
                      <span className="inline-flex items-center gap-1.5">
                        <IconLock className="size-3.5" />
                        System tessellations
                      </span>
                      <span className="ml-2 font-normal normal-case tracking-normal">
                        Managed by HexDB and encrypted like everything else. Change them through the Users, Roles, Streams and Functions pages, not here.
                      </span>
                    </TableCell>
                  </TableRow>
                ),
                <TableRow
                  key={t.name}
                  className={cn(user ? "cursor-pointer" : "bg-muted/15 text-muted-foreground hover:bg-muted/25")}
                  onClick={user ? () => navigate(documentsRoute(t.name)) : undefined}
                >
                  <TableCell className="pl-6 font-medium">
                    <span className="flex items-center gap-2.5">
                      <span
                        className={cn(
                          "flex size-7 shrink-0 items-center justify-center rounded-md",
                          user ? "bg-primary/10 text-primary" : "bg-muted text-muted-foreground",
                        )}
                        aria-hidden
                      >
                        {user ? <IconHexagon className="size-4" /> : <IconLock className="size-3.5" />}
                      </span>
                      {user ? (
                        <a
                          href={href(documentsRoute(t.name))}
                          onClick={(e) => {
                            e.stopPropagation()
                            linkHandler(documentsRoute(t.name))(e)
                          }}
                          className="hover:underline"
                        >
                          {t.name}
                        </a>
                      ) : (
                        <span className="flex min-w-0 flex-col">
                          <span className="font-mono text-sm font-normal">{t.name}</span>
                          <span className="text-xs font-normal">{systemPurpose(t.name)}</span>
                        </span>
                      )}
                      {!user && (
                        <Badge variant="secondary" className="ml-1 h-4.5 px-1.5 text-[10px] tracking-wide uppercase">
                          system
                        </Badge>
                      )}
                    </span>
                  </TableCell>
                  <TableCell className="text-right tabular-nums">
                    {m ? formatNumber(m.document_count) : counts.data?.[t.name] !== undefined ? formatNumber(counts.data[t.name]) : "—"}
                  </TableCell>
                  <TableCell className="text-right tabular-nums">{m ? formatBytes(m.total_size_bytes) : "—"}</TableCell>
                  <TableCell>
                    {user && (can(me, "manage", t.name) || t.indexes.length > 0) ? (
                      <Button
                        variant="ghost"
                        size="sm"
                        className="text-muted-foreground -ml-2 h-7 gap-1.5 px-2 font-normal"
                        title={t.indexes.length ? t.indexes.join(", ") : "Manage indexes"}
                        onClick={(e) => {
                          e.stopPropagation()
                          setIndexing(t.name)
                        }}
                      >
                        <IconKey className="size-3.5" />
                        {t.indexes.length ? `${t.indexes.length} index${t.indexes.length === 1 ? "" : "es"}` : "Add"}
                      </Button>
                    ) : (
                      <span className="text-muted-foreground">—</span>
                    )}
                  </TableCell>
                  <TableCell className="text-muted-foreground text-sm">{formatTimestamp(t.created)}</TableCell>
                  <TableCell className="pr-6">
                    {user && can(me, "manage", t.name) && (
                      <div className="flex justify-end gap-1">
                        <Button
                          variant="ghost"
                          size="icon"
                          className="text-muted-foreground size-8"
                          aria-label={`Schema for ${t.name}`}
                          title="Schema"
                          onClick={(e) => {
                            e.stopPropagation()
                            setSchemaFor(t.name)
                          }}
                        >
                          <IconFileDescription />
                        </Button>
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
                      </div>
                    )}
                  </TableCell>
                </TableRow>,
              ]
            })}
            {tessellations.data && userRows.length === 0 && (
              <TableRow>
                <TableCell colSpan={6} className="text-muted-foreground py-10 text-center">
                  No tessellations yet. Create one, or insert a document and it will be created for you.
                  {!showSystem && systemRows.length > 0 && (
                    <>
                      {" "}
                      <button type="button" className="underline underline-offset-4 hover:text-foreground" onClick={toggleSystem}>
                        Show the {systemRows.length} system tessellation{systemRows.length === 1 ? "" : "s"}
                      </button>
                      .
                    </>
                  )}
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
      {schemaFor !== null && (
        <Suspense>
          <SchemasDialog tessellation={schemaFor} onOpenChange={(open) => !open && setSchemaFor(null)} />
        </Suspense>
      )}
      <IndexesDialog tessellation={indexing} onOpenChange={(open) => !open && setIndexing(null)} onChanged={() => void tessellations.refresh()} />
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
