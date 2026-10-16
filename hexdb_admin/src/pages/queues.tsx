import { useState } from "react"
import { IconPlus, IconRefresh, IconSend, IconTrash } from "@tabler/icons-react"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/confirm-dialog"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Textarea } from "@/components/ui/textarea"
import { usePoll } from "@/hooks/use-poll"
import { api, errorMessage, type QueueConfig, type QueueStats } from "@/lib/api"

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

function QueueCard({ name, onDeleted }: { name: string; onDeleted: () => void }) {
  const stats = usePoll(() => api.queue(name), 5000, [name])
  const [body, setBody] = useState('{ "task": "example" }')
  const [deleting, setDeleting] = useState(false)
  const s: QueueStats | undefined = stats.data
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
  return (
    <Card className="gap-3">
      <CardHeader className="flex flex-row items-start justify-between gap-2">
        <div>
          <CardTitle className="font-mono text-base">{name}</CardTitle>
          <CardDescription>
            {s
              ? `Visibility ${s.queue.visibility_seconds} s · ${s.queue.max_receives} receives${s.queue.dead_letter ? ` then ${s.queue.dead_letter}` : " then dropped"} · kept ${s.queue.retention_hours} h`
              : "Loading…"}
          </CardDescription>
        </div>
        <div className="flex gap-1">
          <Button variant="ghost" size="icon" className="size-8" aria-label="Refresh" onClick={() => void stats.refresh()}>
            <IconRefresh className="size-4" />
          </Button>
          <Button variant="ghost" size="icon" className="size-8" aria-label={`Delete ${name}`} onClick={() => setDeleting(true)}>
            <IconTrash className="size-4" />
          </Button>
        </div>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="grid grid-cols-3 gap-2 text-center">
          {[
            ["Ready", s?.ready],
            ["In flight", s?.in_flight],
            ["Delayed", s?.delayed],
          ].map(([label, value]) => (
            <div key={label as string} className="bg-muted rounded-md py-2">
              <div className="text-lg font-semibold tabular-nums">{value ?? "-"}</div>
              <div className="text-muted-foreground text-xs">{label}</div>
            </div>
          ))}
        </div>
        <div className="flex gap-2">
          <Textarea className="min-h-9 font-mono text-xs" rows={1} value={body} onChange={(e) => setBody(e.target.value)} aria-label="Message body" />
          <Button variant="outline" size="sm" onClick={() => void send()}>
            <IconSend /> Send
          </Button>
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
  const queues = usePoll(api.queues, 15_000)
  const [creating, setCreating] = useState(false)
  return (
    <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-muted-foreground max-w-3xl text-sm">
          Each message goes to one consumer (<code>POST /queues/&lt;name&gt;/receive</code>), which acknowledges it when done. Messages that aren't
          acknowledged come back after the visibility timeout, and go to the dead-letter queue after too many tries. Grant <code>queue:&lt;name&gt;</code>{" "}
          in roles: write to send, read to receive.
        </p>
        <Button onClick={() => setCreating(true)}>
          <IconPlus /> New queue
        </Button>
      </div>
      {queues.error && <p className="text-destructive text-sm">{queues.error.message}</p>}
      {queues.data?.length === 0 && <p className="text-muted-foreground text-sm">No queues yet.</p>}
      <div className="grid gap-4 lg:grid-cols-2">
        {queues.data?.map((q) => <QueueCard key={q.name} name={q.name} onDeleted={() => void queues.refresh()} />)}
      </div>
      <NewQueueDialog open={creating} onOpenChange={setCreating} queues={queues.data ?? []} onCreated={() => void queues.refresh()} />
    </div>
  )
}
