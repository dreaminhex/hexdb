import { useState } from "react"
import { IconLoader2, IconPlus, IconShieldCheck, IconTrash } from "@tabler/icons-react"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/confirm-dialog"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { usePoll } from "@/hooks/use-poll"
import { api, type BlockEntry, errorMessage } from "@/lib/api"

/** Addresses and ranges every hex refuses before authentication (administrators). */
export function BlocklistPage() {
  const entries = usePoll(() => api.blocklist(), 15_000, [])
  const [address, setAddress] = useState("")
  const [note, setNote] = useState("")
  const [hours, setHours] = useState("")
  const [busy, setBusy] = useState(false)
  const [removing, setRemoving] = useState<BlockEntry | null>(null)

  const add = async (event: React.FormEvent) => {
    event.preventDefault()
    const expires = hours.trim() ? Number(hours) : undefined
    if (expires !== undefined && !(expires > 0)) {
      toast.error("Expiry must be a positive number of hours.")
      return
    }
    setBusy(true)
    try {
      const entry = await api.addBlock({ address: address.trim(), note: note.trim(), expires_in_hours: expires })
      toast.success(`Blocked ${entry.address}.`)
      setAddress("")
      setNote("")
      setHours("")
      await entries.refresh()
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
      <p className="text-muted-foreground max-w-3xl text-sm">
        Every hex refuses requests from these addresses and ranges before they're authenticated (403). Health checks and the hexes' own traffic are
        never blocked, and an entry can't cover your own address. Adding and removing entries is audited.
      </p>
      <form onSubmit={add} className="flex flex-wrap items-end gap-2">
        <Input value={address} onChange={(e) => setAddress(e.target.value)} placeholder="203.0.113.0/24 or 2001:db8::1" className="h-8 w-64 font-mono text-xs" aria-label="Address or range" required />
        <Input value={note} onChange={(e) => setNote(e.target.value)} placeholder="Why (optional)" className="h-8 w-64 text-xs" aria-label="Note" />
        <Input value={hours} onChange={(e) => setHours(e.target.value)} placeholder="Expires in hours (optional)" inputMode="decimal" className="h-8 w-52 text-xs" aria-label="Expires in hours" />
        <Button type="submit" size="sm" disabled={busy || !address.trim()}>
          {busy ? <IconLoader2 className="animate-spin" /> : <IconPlus />} Block
        </Button>
      </form>
      {entries.error && <p className="text-destructive text-sm">{entries.error.message}</p>}
      <Card className="p-0">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Address</TableHead>
              <TableHead>Note</TableHead>
              <TableHead>Added</TableHead>
              <TableHead>Expires</TableHead>
              <TableHead className="w-12" />
            </TableRow>
          </TableHeader>
          <TableBody>
            {entries.data?.length === 0 && (
              <TableRow>
                <TableCell colSpan={5} className="text-muted-foreground text-center text-sm">
                  <span className="inline-flex items-center gap-2">
                    <IconShieldCheck className="size-4" /> Nothing is blocked.
                  </span>
                </TableCell>
              </TableRow>
            )}
            {entries.data?.map((e) => (
              <TableRow key={e.id}>
                <TableCell className="font-mono text-xs">{e.address}</TableCell>
                <TableCell className="text-xs">{e.note}</TableCell>
                <TableCell className="text-xs">
                  {new Date(e.created_at).toLocaleString()} · {e.created_by}
                </TableCell>
                <TableCell className="text-xs">{e.expires_at ? new Date(e.expires_at).toLocaleString() : "Never"}</TableCell>
                <TableCell>
                  <Button variant="ghost" size="icon" className="size-8" aria-label={`Unblock ${e.address}`} title="Unblock" onClick={() => setRemoving(e)}>
                    <IconTrash className="size-4" />
                  </Button>
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </Card>
      <ConfirmDialog
        open={removing !== null}
        onOpenChange={(open) => !open && setRemoving(null)}
        title={`Unblock ${removing?.address}?`}
        description="Requests from it are accepted again on every hex within a second."
        confirmLabel="Unblock"
        onConfirm={async () => {
          if (!removing) return
          await api.removeBlock(removing.id)
          toast.success(`Unblocked ${removing.address}.`)
          await entries.refresh()
        }}
      />
    </div>
  )
}
