import { useState } from "react"
import { IconAlertTriangle, IconLoader2, IconRefresh, IconRestore, IconShieldCheck, IconTrash } from "@tabler/icons-react"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/confirm-dialog"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { usePoll } from "@/hooks/use-poll"
import { api, type QuarantinedFile } from "@/lib/api"
import { formatBytes } from "@/lib/format"

type Action = { kind: "release" | "delete"; file: QuarantinedFile }

/** Uploads the malware scan flagged: what matched, and releasing or deleting them (administrators). */
export function QuarantinePage() {
  const files = usePoll(() => api.quarantine(), 15_000, [])
  const status = usePoll(() => api.status(), 30_000, [])
  const [action, setAction] = useState<Action | null>(null)
  const scan = status.data?.storage.scan
  const rescan = scan?.rescan

  return (
    <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
      <div className="flex flex-wrap items-start justify-between gap-2">
        <p className="text-muted-foreground max-w-3xl text-sm">
          Every upload is scanned before it's stored, and stored files are scanned again when the signatures change. A file that matches is kept here
          instead, on the Overseer only: it isn't in its tessellation, can't be downloaded, and never reaches another hex. Release a false positive to
          store it as a normal file, or delete it.
        </p>
        {scan && (
          <Button
            variant="outline"
            size="sm"
            disabled={rescan?.running}
            onClick={() =>
              void api
                .rescan()
                .then(() => toast.success("Rescanning every stored file."))
                .then(() => status.refresh())
                .catch((e: Error) => toast.error(e.message))
            }
          >
            {rescan?.running ? <IconLoader2 className="size-4 animate-spin" /> : <IconRefresh className="size-4" />} Rescan now
          </Button>
        )}
      </div>
      {rescan && (rescan.running || rescan.finished_at) && (
        <p className="text-muted-foreground text-xs">
          {rescan.running
            ? `Rescanning: ${rescan.scanned} file(s) so far, ${rescan.found} quarantined.`
            : `Last rescan ${new Date(rescan.finished_at as string).toLocaleString()}: ${rescan.scanned} file(s), ${rescan.found} quarantined.`}
          {rescan.error && ` Paused: ${rescan.error}`}
        </p>
      )}
      {scan === null && (
        <p className="text-destructive flex items-center gap-2 text-sm">
          <IconAlertTriangle className="size-4" /> Malware scanning is off (files.scan.enabled = false): uploads aren't scanned.
        </p>
      )}
      {scan?.warning && (
        <p className="flex items-center gap-2 text-sm text-amber-600 dark:text-amber-400">
          <IconAlertTriangle className="size-4 shrink-0" /> {scan.warning}
        </p>
      )}
      {files.error && <p className="text-destructive text-sm">{files.error.message}</p>}
      <Card className="p-0">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>File</TableHead>
              <TableHead>Matched</TableHead>
              <TableHead>Uploaded</TableHead>
              <TableHead className="text-right">Size</TableHead>
              <TableHead className="w-24" />
            </TableRow>
          </TableHeader>
          <TableBody>
            {files.data?.files.length === 0 && (
              <TableRow>
                <TableCell colSpan={5} className="text-muted-foreground text-center text-sm">
                  <span className="inline-flex items-center gap-2">
                    <IconShieldCheck className="size-4" /> Nothing is quarantined.
                  </span>
                </TableCell>
              </TableRow>
            )}
            {files.data?.files.map((f) => (
              <TableRow key={f.id}>
                <TableCell>
                  <div className="font-medium break-all">{f.name}</div>
                  <div className="text-muted-foreground text-xs break-all">
                    {f.tessellation} · {f.path}
                  </div>
                </TableCell>
                <TableCell>
                  <div className="flex flex-wrap gap-1">
                    {f.scan.detections.map((d) => (
                      <Badge key={`${d.engine}:${d.name}`} variant="destructive" title={d.engine}>
                        {d.name}
                      </Badge>
                    ))}
                  </div>
                  <div className="text-muted-foreground mt-1 text-xs">{f.scan.engines.join(", ")}</div>
                </TableCell>
                <TableCell className="text-xs">
                  <div>{f.uploaded_by}</div>
                  <div className="text-muted-foreground">{new Date(f.uploaded_at).toLocaleString()}</div>
                </TableCell>
                <TableCell className="text-right text-xs tabular-nums">{formatBytes(f.size)}</TableCell>
                <TableCell>
                  <div className="flex justify-end gap-1">
                    <Button variant="ghost" size="icon" className="size-8" aria-label="Release the file" title="Release" onClick={() => setAction({ kind: "release", file: f })}>
                      <IconRestore className="size-4" />
                    </Button>
                    <Button variant="ghost" size="icon" className="size-8" aria-label="Delete the file" title="Delete" onClick={() => setAction({ kind: "delete", file: f })}>
                      <IconTrash className="size-4" />
                    </Button>
                  </div>
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </Card>
      <ConfirmDialog
        open={action !== null}
        onOpenChange={(open) => !open && setAction(null)}
        title={action?.kind === "release" ? `Release ${action.file.name}?` : `Delete ${action?.file.name} for good?`}
        description={
          action?.kind === "release"
            ? `It matched ${action.file.scan.detections.map((d) => d.name).join(", ")}. Releasing stores it in ${action.file.tessellation} as a normal file that anyone with access can download, and it replicates to every hex. Only release a file you're sure is safe.`
            : "The file and its content are removed. This can't be undone."
        }
        confirmLabel={action?.kind === "release" ? "Release" : "Delete"}
        onConfirm={async () => {
          if (!action) return
          if (action.kind === "release") {
            await api.releaseQuarantined(action.file.id)
            toast.success(`Released ${action.file.name} into ${action.file.tessellation}.`)
          } else {
            await api.deleteQuarantined(action.file.id)
            toast.success(`Deleted ${action.file.name}.`)
          }
          await files.refresh()
        }}
      />
    </div>
  )
}
