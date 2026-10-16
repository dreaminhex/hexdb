import { useState } from "react"
import { IconCopy, IconLock, IconTrash } from "@tabler/icons-react"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/confirm-dialog"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Checkbox } from "@/components/ui/checkbox"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { usePoll } from "@/hooks/use-poll"
import { api, shareAddress, type ShareLink } from "@/lib/api"
import { useAuth } from "@/lib/auth"

function when(ms: number | null | undefined): string {
  return ms ? new Date(ms).toLocaleString() : "never"
}

/** Your sharing links (administrators can show everyone's), with their use, and deleting them. */
export function SharesPage() {
  const { me } = useAuth()
  const [all, setAll] = useState(false)
  const links = usePoll(() => api.shares(all), 15_000, [all])
  const [target, setTarget] = useState<ShareLink | null>(null)

  return (
    <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-muted-foreground max-w-3xl text-sm">
          Links that give people without an account a file or a folder. Make them from the Files page. A link shows only what its maker can see,
          and stops working when it expires, runs out of downloads, or is deleted.
        </p>
        {me?.is_admin && (
          <label className="flex items-center gap-2 text-sm">
            <Checkbox checked={all} onCheckedChange={(v) => setAll(v === true)} /> Everyone's links
          </label>
        )}
      </div>
      {links.error && <p className="text-destructive text-sm">{links.error.message}</p>}
      <Card className="p-0">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Shared</TableHead>
              <TableHead>Link</TableHead>
              <TableHead>Expires</TableHead>
              <TableHead className="text-right">Downloads</TableHead>
              {all && <TableHead>By</TableHead>}
              <TableHead className="w-24" />
            </TableRow>
          </TableHeader>
          <TableBody>
            {links.data?.shares.length === 0 && (
              <TableRow>
                <TableCell colSpan={6} className="text-muted-foreground text-center text-sm">
                  No links yet.
                </TableCell>
              </TableRow>
            )}
            {links.data?.shares.map((s) => (
              <TableRow key={s.code} className={s.expired ? "opacity-60" : undefined}>
                <TableCell>
                  <div className="font-medium">
                    {s.file_id ? "" : "📁 "}
                    {s.name}
                  </div>
                  <div className="text-muted-foreground text-xs">
                    {s.tessellation}
                    {s.folder ? ` · ${s.folder}` : ""}
                    {s.note ? ` · ${s.note}` : ""}
                  </div>
                </TableCell>
                <TableCell>
                  <div className="flex items-center gap-1 font-mono text-xs">
                    {s.code}
                    {s.password && <IconLock className="text-muted-foreground size-3" aria-label="Password" />}
                    {s.expired && <Badge variant="secondary">expired</Badge>}
                  </div>
                </TableCell>
                <TableCell className="text-xs">{when(s.expires_at)}</TableCell>
                <TableCell className="text-right tabular-nums">
                  {s.downloads}
                  {s.max_downloads ? ` / ${s.max_downloads}` : ""}
                </TableCell>
                {all && <TableCell className="text-xs">{s.created_by}</TableCell>}
                <TableCell>
                  <div className="flex justify-end gap-1">
                    <Button
                      variant="ghost"
                      size="icon"
                      className="size-8"
                      aria-label="Copy the link"
                      onClick={() => void navigator.clipboard.writeText(shareAddress(s)).then(() => toast.success("Link copied."))}
                    >
                      <IconCopy className="size-4" />
                    </Button>
                    <Button variant="ghost" size="icon" className="size-8" aria-label="Delete the link" onClick={() => setTarget(s)}>
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
        open={target !== null}
        onOpenChange={(open) => !open && setTarget(null)}
        title={`Delete the link to ${target?.name}?`}
        description="Anyone who has it can no longer use it. The file stays where it is."
        confirmLabel="Delete link"
        onConfirm={async () => {
          if (!target) return
          await api.deleteShare(target.code)
          toast.success("Link deleted.")
          await links.refresh()
        }}
      />
    </div>
  )
}
