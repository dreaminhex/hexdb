import { useEffect, useState } from "react"
import { IconArrowRight, IconLoader2, IconPencil, IconPlus, IconSend, IconTrash } from "@tabler/icons-react"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/confirm-dialog"
import { JsonEditor, parseJson } from "@/components/json-editor"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { usePoll } from "@/hooks/use-poll"
import { api, errorMessage, type StreamConfig, type StreamMessage } from "@/lib/api"
import { can, useAuth } from "@/lib/auth"
import { formatNumber } from "@/lib/format"

const EXAMPLE_SOURCES = `[
  { "tessellation": "orders", "ops": ["put"], "filter": { "total": { "$gt": 100 } } }
]`
const EXAMPLE_DESTINATIONS = `[
  { "url": "https://example.com/hooks/orders", "batch_size": 100 }
]`

function StreamDialog({ stream, open, onOpenChange, onSaved }: { stream: StreamConfig | null; open: boolean; onOpenChange: (open: boolean) => void; onSaved: () => void }) {
  const [name, setName] = useState("")
  const [description, setDescription] = useState("")
  const [retention, setRetention] = useState("168")
  const [sources, setSources] = useState("[]")
  const [destinations, setDestinations] = useState("[]")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()

  useEffect(() => {
    if (!open) return
    setName(stream?.name ?? "")
    setDescription(stream?.description ?? "")
    setRetention(String(stream?.retention_hours ?? 168))
    setSources(stream ? JSON.stringify(stream.sources, null, 2) : EXAMPLE_SOURCES)
    setDestinations(stream ? JSON.stringify(stream.destinations, null, 2) : "[]")
    setError(undefined)
  }, [open, stream])

  const submit = async (event: React.FormEvent) => {
    event.preventDefault()
    setBusy(true)
    setError(undefined)
    try {
      const config: StreamConfig = {
        name: name.trim(),
        description,
        retention_hours: Number(retention) || 0,
        sources: parseJson(sources, "Sources"),
        destinations: parseJson(destinations, "Destinations"),
      }
      if (stream) await api.updateStream(stream.name, config)
      else await api.createStream(config)
      toast.success(stream ? `Updated ${config.name}.` : `Created ${config.name}.`)
      onSaved()
      onOpenChange(false)
    } catch (e) {
      setError(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={(next) => !busy && onOpenChange(next)}>
      <DialogContent className="max-h-[92svh] overflow-y-auto sm:max-w-2xl">
        <form onSubmit={submit} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>{stream ? `Edit ${stream.name}` : "New stream"}</DialogTitle>
            <DialogDescription>
              A stream keeps messages in order for its retention period. Producers publish through the API; sources turn a tessellation's changes
              into messages; destinations receive every message by webhook.
            </DialogDescription>
          </DialogHeader>
          <div className="grid gap-3 sm:grid-cols-[1fr_9rem]">
            <div className="grid gap-1.5">
              <Label htmlFor="stream-name">Name</Label>
              <Input id="stream-name" value={name} onChange={(e) => setName(e.target.value)} disabled={!!stream} required placeholder="order-events" />
            </div>
            <div className="grid gap-1.5">
              <Label htmlFor="stream-retention">Retention (hours)</Label>
              <Input id="stream-retention" type="number" min={0} value={retention} onChange={(e) => setRetention(e.target.value)} title="0 keeps messages forever" />
            </div>
          </div>
          <div className="grid gap-1.5">
            <Label htmlFor="stream-description">Description</Label>
            <Input id="stream-description" value={description} onChange={(e) => setDescription(e.target.value)} />
          </div>
          <div className="grid gap-1.5">
            <Label>Sources</Label>
            <JsonEditor value={sources} onChange={setSources} label="Stream sources" className="h-32" />
            <p className="text-muted-foreground text-xs">Each: tessellation, optional ops (put, delete) and filter. Leave [] for a stream you publish to yourself.</p>
          </div>
          <div className="grid gap-1.5">
            <Label>Destinations</Label>
            <JsonEditor value={destinations} onChange={setDestinations} label="Stream destinations" className="h-28" />
            <p className="text-muted-foreground text-xs">
              Each: a webhook url, optional headers and batch_size. Retried until delivered. Example: <code>{EXAMPLE_DESTINATIONS.replace(/\s+/g, " ")}</code>
            </p>
          </div>
          {error && <p className="text-destructive text-sm">{error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)} disabled={busy}>
              Cancel
            </Button>
            <Button type="submit" disabled={busy || !name.trim()}>
              {busy && <IconLoader2 className="animate-spin" />}
              {stream ? "Save" : "Create stream"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

function StreamPanel({ name, onClose }: { name: string | null; onClose: () => void }) {
  const detail = usePoll(() => (name ? api.stream(name) : Promise.resolve(undefined)), 5000, [name])
  const [messages, setMessages] = useState<StreamMessage[]>([])
  const [next, setNext] = useState<string | null>(null)
  const [payload, setPayload] = useState('{ "hello": "world" }')
  const [busy, setBusy] = useState(false)

  const load = async (after?: string) => {
    if (!name) return
    const page = await api.readStream(name, after)
    setMessages((m) => (after ? [...m, ...page.messages] : page.messages))
    setNext(page.messages.length > 0 ? page.next : null)
  }

  useEffect(() => {
    setMessages([])
    setNext(null)
    if (name) void load().catch((e) => toast.error(errorMessage(e)))
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [name])

  const publish = async () => {
    if (!name) return
    setBusy(true)
    try {
      await api.publish(name, [{ payload: parseJson(payload, "The payload") }])
      toast.success("Published.")
      await load()
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  const d = detail.data
  return (
    <Sheet open={name !== null} onOpenChange={(open) => !open && onClose()}>
      <SheetContent className="w-full overflow-y-auto sm:max-w-xl">
        <SheetHeader>
          <SheetTitle>{name}</SheetTitle>
          <SheetDescription>{d?.stream.description || "Messages in publish order."}</SheetDescription>
        </SheetHeader>
        {d && (
          <div className="grid gap-5 px-4 pb-6">
            <div className="grid gap-2 text-sm">
              <div className="font-medium">Delivery</div>
              {d.stream.sources.map((s, i) => (
                <div key={`s${i}`} className="flex items-center gap-2">
                  <Badge variant="outline">source</Badge> {s.tessellation} <IconArrowRight className="size-3" /> {formatNumber(d.sources[i]?.delivered ?? 0)} messages
                  {d.sources[i]?.last_error && <span className="text-destructive text-xs">{d.sources[i]?.last_error}</span>}
                </div>
              ))}
              {d.stream.destinations.map((dest, i) => (
                <div key={`d${i}`} className="flex items-center gap-2">
                  <Badge variant="outline">destination</Badge> <span className="truncate font-mono text-xs">{dest.url}</span> · {formatNumber(d.destinations[i]?.delivered ?? 0)} delivered
                  {d.destinations[i]?.last_error && <span className="text-destructive text-xs">{d.destinations[i]?.last_error}</span>}
                </div>
              ))}
              {d.stream.sources.length + d.stream.destinations.length === 0 && <p className="text-muted-foreground">No sources or destinations.</p>}
            </div>
            <div className="grid gap-2 text-sm">
              <div className="font-medium">Consumer groups</div>
              {d.groups.filter((g) => !g.group.startsWith("__")).length === 0 && <p className="text-muted-foreground">None yet.</p>}
              {d.groups
                .filter((g) => !g.group.startsWith("__"))
                .map((g) => (
                  <div key={g.group} className="flex items-center gap-2">
                    <span className="font-medium">{g.group}</span>
                    <span className="text-muted-foreground font-mono text-xs">{g.offset ?? "start"}</span>
                    <Badge variant={g.pending > 0 ? "secondary" : "outline"}>{formatNumber(g.pending)} pending</Badge>
                  </div>
                ))}
            </div>
            <div className="grid gap-2">
              <div className="text-sm font-medium">Publish a message</div>
              <JsonEditor value={payload} onChange={setPayload} label="Message payload" className="h-24" />
              <Button size="sm" className="justify-self-start" onClick={() => void publish()} disabled={busy}>
                {busy ? <IconLoader2 className="animate-spin" /> : <IconSend />} Publish
              </Button>
            </div>
            <div className="grid gap-2">
              <div className="text-sm font-medium">Messages</div>
              {messages.length === 0 && <p className="text-muted-foreground text-sm">No messages.</p>}
              <div className="divide-y rounded-md border">
                {messages.map((m) => (
                  <div key={m.offset} className="grid gap-1 p-2.5">
                    <div className="text-muted-foreground flex flex-wrap gap-2 text-xs">
                      <span className="font-mono">{m.offset}</span>
                      <span>{new Date(m.time).toLocaleString()}</span>
                      {m.key && <span>key {m.key}</span>}
                      {m.published_by && <span>by {m.published_by}</span>}
                    </div>
                    <pre className="bg-muted/40 overflow-x-auto rounded p-1.5 text-xs">{JSON.stringify(m.payload)}</pre>
                  </div>
                ))}
              </div>
              {next && (
                <Button variant="outline" size="sm" onClick={() => void load(next)}>
                  Load more
                </Button>
              )}
            </div>
          </div>
        )}
      </SheetContent>
    </Sheet>
  )
}

/** Streams: ordered message logs with sources, destinations and consumer groups. */
export function StreamsPage() {
  const { me } = useAuth()
  const streams = usePoll(api.streams, 10_000)
  const [editing, setEditing] = useState<StreamConfig | null | undefined>(undefined)
  const [viewing, setViewing] = useState<string | null>(null)
  const [deleting, setDeleting] = useState<string | null>(null)
  const mayManage = (name: string) => can(me, "manage", `stream:${name}`)

  return (
    <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
      <div className="flex items-start gap-4">
        <p className="text-muted-foreground max-w-3xl text-sm">
          Streams are publish/subscribe logs. Producers publish messages; consumers read them in order by offset, as consumer groups, or live
          over Server-Sent Events. Grant access with <code>stream:&lt;name&gt;</code> in a role's tessellation list.
        </p>
        {me?.is_admin && (
          <Button size="sm" className="ml-auto shrink-0" onClick={() => setEditing(null)}>
            <IconPlus /> New stream
          </Button>
        )}
      </div>
      {streams.error && <p className="text-destructive text-sm">{streams.error.message}</p>}
      <Card className="gap-0 overflow-hidden py-0">
        <Table>
          <TableHeader className="bg-muted/60">
            <TableRow>
              <TableHead className="pl-6">Stream</TableHead>
              <TableHead>Sources</TableHead>
              <TableHead>Destinations</TableHead>
              <TableHead>Retention</TableHead>
              <TableHead className="w-24 pr-6" />
            </TableRow>
          </TableHeader>
          <TableBody>
            {(streams.data ?? []).map((s) => (
              <TableRow key={s.name} className="cursor-pointer" onClick={() => setViewing(s.name)}>
                <TableCell className="pl-6">
                  <div className="font-medium">{s.name}</div>
                  <div className="text-muted-foreground text-xs">{s.description}</div>
                </TableCell>
                <TableCell className="text-sm">{s.sources.map((x) => x.tessellation).join(", ") || "—"}</TableCell>
                <TableCell className="text-sm">{s.destinations.length || "—"}</TableCell>
                <TableCell className="text-muted-foreground text-sm">{s.retention_hours ? `${s.retention_hours} h` : "forever"}</TableCell>
                <TableCell className="pr-6">
                  {mayManage(s.name) && (
                    <div className="flex justify-end gap-1" onClick={(e) => e.stopPropagation()}>
                      <Button variant="ghost" size="icon" className="text-muted-foreground size-8" aria-label={`Edit ${s.name}`} onClick={() => setEditing(s)}>
                        <IconPencil />
                      </Button>
                      <Button variant="ghost" size="icon" className="text-muted-foreground hover:text-destructive size-8" aria-label={`Delete ${s.name}`} onClick={() => setDeleting(s.name)}>
                        <IconTrash />
                      </Button>
                    </div>
                  )}
                </TableCell>
              </TableRow>
            ))}
            {streams.data && streams.data.length === 0 && (
              <TableRow>
                <TableCell colSpan={5} className="text-muted-foreground py-10 text-center">
                  No streams.
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </Card>
      <StreamDialog stream={editing ?? null} open={editing !== undefined} onOpenChange={(open) => !open && setEditing(undefined)} onSaved={() => void streams.refresh()} />
      <StreamPanel name={viewing} onClose={() => setViewing(null)} />
      <ConfirmDialog
        open={deleting !== null}
        onOpenChange={(open) => !open && setDeleting(null)}
        title={`Delete the stream ${deleting}?`}
        description="Its messages and consumer group offsets are deleted too."
        confirmLabel="Delete stream"
        confirmText={deleting ?? undefined}
        onConfirm={async () => {
          await api.deleteStream(deleting!)
          toast.success(`Deleted ${deleting}.`)
          void streams.refresh()
        }}
      />
    </div>
  )
}
