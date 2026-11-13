import { useEffect, useState } from "react"
import { IconArrowBackUp, IconCheck, IconDownload, IconLoader2, IconPlus, IconRefresh, IconSend, IconTrash } from "@tabler/icons-react"
import { toast } from "sonner"

import { CodeSnippets } from "@/components/code-snippets"
import { ConfirmDialog } from "@/components/confirm-dialog"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Textarea } from "@/components/ui/textarea"
import { usePoll } from "@/hooks/use-poll"
import { api, errorMessage, type QueueConfig, type QueueMessage, type QueueStats } from "@/lib/api"
import { can, useAuth } from "@/lib/auth"
import { queueSnippets } from "@/lib/examples"

function NewQueueDialog({ open, onOpenChange, queues, onCreated }: { open: boolean; onOpenChange: (o: boolean) => void; queues: QueueConfig[]; onCreated: () => void }) {
  const [name, setName] = useState("")
  const [visibility, setVisibility] = useState("30")
  const [maxReceives, setMaxReceives] = useState("5")
  const [deadLetter, setDeadLetter] = useState("")
  const [retention, setRetention] = useState("96")
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const create = async () => {
    setBusy(true)
    setError(null)
    try {
      await api.createQueue({
        name: name.trim(),
        visibility_seconds: Number(visibility),
        max_receives: Number(maxReceives),
        retention_hours: Number(retention),
        ...(deadLetter.trim() ? { dead_letter: deadLetter.trim() } : {}),
      })
      toast.success(`Created queue ${name}.`)
      onCreated()
      onOpenChange(false)
    } catch (e) {
      setError(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>New queue</DialogTitle>
          <DialogDescription>Each message goes to one consumer, which acknowledges it when done. Unacknowledged messages come back after the visibility timeout.</DialogDescription>
        </DialogHeader>
        <div className="grid gap-3">
          <div className="grid gap-1.5">
            <Label htmlFor="q-name">Name</Label>
            <Input id="q-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="jobs" />
          </div>
          <div className="grid grid-cols-3 gap-3">
            <div className="grid gap-1.5">
              <Label htmlFor="q-vis">Visibility (s)</Label>
              <Input id="q-vis" inputMode="numeric" value={visibility} onChange={(e) => setVisibility(e.target.value)} />
            </div>
            <div className="grid gap-1.5">
              <Label htmlFor="q-max">Max receives</Label>
              <Input id="q-max" inputMode="numeric" value={maxReceives} onChange={(e) => setMaxReceives(e.target.value)} />
            </div>
            <div className="grid gap-1.5">
              <Label htmlFor="q-ret">Keep (hours)</Label>
              <Input id="q-ret" inputMode="numeric" value={retention} onChange={(e) => setRetention(e.target.value)} />
            </div>
          </div>
          <div className="grid gap-1.5">
            <Label htmlFor="q-dead">Dead-letter queue (optional)</Label>
            <Input id="q-dead" list="q-names" value={deadLetter} onChange={(e) => setDeadLetter(e.target.value)} placeholder="Messages received too often go here" />
            <datalist id="q-names">
              {queues.map((q) => (
                <option key={q.name} value={q.name} />
              ))}
            </datalist>
          </div>
          {error && <p className="text-destructive text-sm">{error}</p>}
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button disabled={busy || !name.trim()} onClick={() => void create()}>
            Create queue
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

/** A leased message: what a worker holds between receive and acknowledge. */
type Lease = QueueMessage & { until: number }

/** Seconds left on a lease, counting down; 0 once it ran out and the message went back. */
function useNow(active: boolean) {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (!active) return
    const t = setInterval(() => setNow(Date.now()), 1000)
    return () => clearInterval(t)
  }, [active])
  return now
}

function QueueCard({ name, onDeleted }: { name: string; onDeleted: () => void }) {
  const { me } = useAuth()
  const stats = usePoll(() => api.queue(name), 5000, [name])
  const [body, setBody] = useState('{ "task": "example" }')
  const [deleting, setDeleting] = useState(false)
  const [leases, setLeases] = useState<Lease[]>([])
  const [busy, setBusy] = useState(false)
  const [showCode, setShowCode] = useState(false)
  const now = useNow(leases.length > 0)
  const s: QueueStats | undefined = stats.data
  const visibility = s?.queue.visibility_seconds ?? 30
  const mayManage = can(me, "manage", `queue:${name}`)
  const maySend = can(me, "write", `queue:${name}`)
  const mayReceive = can(me, "read", `queue:${name}`)

  const send = async () => {
    try {
      let parsed: unknown = body
      try {
        parsed = JSON.parse(body)
      } catch {
        // Plain text is a string body.
      }
      await api.sendToQueue(name, [{ body: parsed }])
      toast.success("Sent.")
      await stats.refresh()
    } catch (e) {
      toast.error(errorMessage(e))
    }
  }

  const receive = async () => {
    setBusy(true)
    try {
      const got = await api.receiveFromQueue(name, 3)
      if (got.length === 0) toast.info("Nothing ready: every message is in flight, delayed or done.")
      const until = Date.now() + visibility * 1000
      setLeases((l) => [...l.filter((x) => x.until > Date.now()), ...got.map((m) => ({ ...m, until }))])
      await stats.refresh()
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  const settle = async (lease: Lease, how: "ack" | "release") => {
    try {
      const r = how === "ack" ? await api.ackQueue(name, [lease.receipt]) : await api.releaseQueue(name, [lease.receipt])
      if (r.stale > 0) toast.warning("Too late: the lease ran out, so the message went back to the queue.")
      else toast.success(how === "ack" ? "Done: the message is deleted." : "Released: it's ready for the next worker.")
    } catch (e) {
      toast.error(errorMessage(e))
    }
    setLeases((l) => l.filter((x) => x.receipt !== lease.receipt))
    await stats.refresh()
  }

  return (
    <Card className="min-w-0 gap-3">
      <CardHeader className="flex flex-row items-start justify-between gap-2">
        <div>
          <CardTitle className="font-mono text-base">{name}</CardTitle>
          <CardDescription>
            {s?.queue.description ? `${s.queue.description} · ` : ""}
            {s
              ? `Visibility ${s.queue.visibility_seconds} s · ${s.queue.max_receives} receives${s.queue.dead_letter ? ` then ${s.queue.dead_letter}` : " then dropped"} · kept ${s.queue.retention_hours} h`
              : "Loading…"}
          </CardDescription>
        </div>
        <div className="flex gap-1">
          <Button variant="ghost" size="icon" className="size-8" aria-label="Refresh" onClick={() => void stats.refresh()}>
            <IconRefresh className="size-4" />
          </Button>
          {mayManage && (
            <Button variant="ghost" size="icon" className="size-8" aria-label={`Delete ${name}`} onClick={() => setDeleting(true)}>
              <IconTrash className="size-4" />
            </Button>
          )}
        </div>
      </CardHeader>
      <CardContent className="min-w-0 space-y-4">
        <div className="grid grid-cols-3 gap-2 text-center">
          {(
            [
              ["Ready", s?.ready, "Waiting for a worker"],
              ["In flight", s?.in_flight, "Leased to a worker, not acknowledged yet"],
              ["Delayed", s?.delayed, "Sent with a delay, or released with one"],
            ] as const
          ).map(([label, value, title]) => (
            <div key={label} className="bg-muted rounded-md py-2" title={title}>
              <div className="text-lg font-semibold tabular-nums">{value ?? "-"}</div>
              <div className="text-muted-foreground text-xs">{label}</div>
            </div>
          ))}
        </div>

        {maySend && (
          <div className="grid gap-1.5">
            <Label className="text-xs">1. Send a message (a producer)</Label>
            <div className="flex gap-2">
              <Textarea className="min-h-9 font-mono text-xs" rows={1} value={body} onChange={(e) => setBody(e.target.value)} aria-label="Message body" />
              <Button variant="outline" size="sm" onClick={() => void send()}>
                <IconSend /> Send
              </Button>
            </div>
          </div>
        )}

        {mayReceive && (
          <div className="grid gap-2">
            <div className="flex items-center justify-between gap-2">
              <Label className="text-xs">{maySend ? "2. " : ""}Receive (be a worker)</Label>
              <Button variant="outline" size="sm" onClick={() => void receive()} disabled={busy}>
                {busy ? <IconLoader2 className="animate-spin" /> : <IconDownload />} Receive up to 3
              </Button>
            </div>
            {leases.length === 0 ? (
              <p className="text-muted-foreground text-xs">
                Received messages are leased to you for {visibility} s: no other worker gets them. Acknowledge each when done, or release it; one you
                leave runs out and comes back.
              </p>
            ) : (
              <div className="divide-y rounded-md border">
                {leases.map((m) => {
                  const left = Math.max(0, Math.ceil((m.until - now) / 1000))
                  return (
                    <div key={m.receipt} className="grid gap-1.5 p-2.5">
                      <div className="text-muted-foreground flex flex-wrap items-center gap-2 text-xs">
                        <span className="font-mono">{m.id}</span>
                        <span>received {m.receives}×</span>
                        <span>sent {new Date(m.sent_at).toLocaleTimeString()}</span>
                        {left > 0 ? <Badge variant="secondary">lease {left} s</Badge> : <Badge variant="outline">lease ran out: back in the queue</Badge>}
                      </div>
                      <pre className="bg-muted/40 overflow-x-auto rounded p-1.5 text-xs">{JSON.stringify(m.body)}</pre>
                      <div className="flex gap-2">
                        <Button size="sm" className="h-7" onClick={() => void settle(m, "ack")} disabled={left === 0}>
                          <IconCheck /> Acknowledge
                        </Button>
                        <Button size="sm" variant="outline" className="h-7" onClick={() => void settle(m, "release")} disabled={left === 0}>
                          <IconArrowBackUp /> Release
                        </Button>
                        {left === 0 && (
                          <Button size="sm" variant="ghost" className="h-7" onClick={() => setLeases((l) => l.filter((x) => x.receipt !== m.receipt))}>
                            Dismiss
                          </Button>
                        )}
                      </div>
                    </div>
                  )
                })}
              </div>
            )}
          </div>
        )}

        <div className="grid min-w-0 gap-2">
          <Button variant="ghost" size="sm" className="text-muted-foreground justify-self-start px-0" onClick={() => setShowCode((v) => !v)}>
            {showCode ? "Hide" : "Use it from your code"}
          </Button>
          {showCode && <CodeSnippets snippets={queueSnippets(name, visibility)} />}
        </div>
      </CardContent>
      <ConfirmDialog
        open={deleting}
        onOpenChange={setDeleting}
        title={`Delete queue ${name}?`}
        description="Its messages are deleted with it."
        confirmLabel="Delete queue"
        confirmText={name}
        onConfirm={async () => {
          await api.deleteQueue(name)
          toast.success(`Deleted ${name}.`)
          onDeleted()
        }}
      />
    </Card>
  )
}

/** Queues: messages for competing consumers, with acknowledgements, retries and dead letters. */
export function QueuesPage() {
  const { me } = useAuth()
  const queues = usePoll(api.queues, 15_000)
  const [creating, setCreating] = useState(false)
  return (
    <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div className="text-muted-foreground max-w-3xl space-y-1.5 text-sm">
          <p>
            A queue hands out work. Producers <strong className="text-foreground font-medium">send</strong> messages; workers{" "}
            <strong className="text-foreground font-medium">receive</strong> them, each message leased to one worker at a time, and{" "}
            <strong className="text-foreground font-medium">acknowledge</strong> each when the work is done. A message that isn't acknowledged in time
            comes back for another worker, and after too many tries it goes to the dead-letter queue.
          </p>
          <p>
            Unlike a stream, where every subscriber sees every message, each queue message is handled once. Grant <code>queue:&lt;name&gt;</code> in a
            role: write to send, read to receive.
          </p>
        </div>
        {me?.is_admin && (
          <Button onClick={() => setCreating(true)}>
            <IconPlus /> New queue
          </Button>
        )}
      </div>
      {queues.error && <p className="text-destructive text-sm">{queues.error.message}</p>}
      {queues.data?.length === 0 && <p className="text-muted-foreground text-sm">No queues yet.</p>}
      <div className="grid grid-cols-1 gap-4 lg:grid-cols-2">
        {queues.data?.map((q) => <QueueCard key={q.name} name={q.name} onDeleted={() => void queues.refresh()} />)}
      </div>
      <NewQueueDialog open={creating} onOpenChange={setCreating} queues={queues.data ?? []} onCreated={() => void queues.refresh()} />
    </div>
  )
}
