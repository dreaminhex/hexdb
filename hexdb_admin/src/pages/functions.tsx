import { useEffect, useState } from "react"
import { IconLoader2, IconPencil, IconPlayerPlay, IconPlus, IconTrash } from "@tabler/icons-react"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/confirm-dialog"
import { JsonEditor, parseJson } from "@/components/json-editor"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Checkbox } from "@/components/ui/checkbox"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs"
import { usePoll } from "@/hooks/use-poll"
import { api, errorMessage, type FunctionDef, type Schedule } from "@/lib/api"
import { useAuth } from "@/lib/auth"

const TEMPLATES: Record<FunctionDef["kind"], { body?: string; code?: string; params: string }> = {
  query: {
    params: `[{ "name": "status", "type": "string", "required": true }]`,
    body: `{
  "filter": { "status": { "$param": "status" } },
  "sort": "-created",
  "limit": 50
}`,
  },
  aggregate: {
    params: "[]",
    body: `{
  "group_by": ["status"],
  "aggregates": { "orders": { "$count": true }, "revenue": { "$sum": "total" } }
}`,
  },
  transaction: {
    params: `[{ "name": "amount", "type": "number", "required": true }]`,
    body: `{
  "operations": [
    { "op": "insert", "tessellation": "ledger", "data": { "amount": { "$param": "amount" } } }
  ]
}`,
  },
  script: {
    params: `[{ "name": "tessellation", "type": "string", "default": "orders" }]`,
    code: `# Input arrives as JSON on stdin; print the result as JSON.
# HEXDB_API and HEXDB_TOKEN let the script call HexDB as whoever runs it.
import json, os, sys, urllib.request

params = json.load(sys.stdin)["params"]
url = os.environ["HEXDB_API"] + "/" + params["tessellation"] + "/count"
request = urllib.request.Request(url, headers={"Authorization": "Bearer " + os.environ["HEXDB_TOKEN"]})
print(json.dumps(json.load(urllib.request.urlopen(request))))
`,
  },
}

function FunctionDialog({ fn, open, onOpenChange, onSaved }: { fn: FunctionDef | null; open: boolean; onOpenChange: (open: boolean) => void; onSaved: () => void }) {
  const [name, setName] = useState("")
  const [kind, setKind] = useState<FunctionDef["kind"]>("query")
  const [description, setDescription] = useState("")
  const [tessellation, setTessellation] = useState("")
  const [params, setParams] = useState("[]")
  const [body, setBody] = useState("{}")
  const [runtime, setRuntime] = useState<NonNullable<FunctionDef["runtime"]>>("python")
  const [code, setCode] = useState("")
  const [timeout, setTimeoutSeconds] = useState("30")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()

  useEffect(() => {
    if (!open) return
    const k = fn?.kind ?? "query"
    setName(fn?.name ?? "")
    setKind(k)
    setDescription(fn?.description ?? "")
    setTessellation(fn?.tessellation ?? "")
    setParams(fn ? JSON.stringify(fn.params, null, 2) : TEMPLATES[k].params)
    setBody(fn?.body ? JSON.stringify(fn.body, null, 2) : TEMPLATES[k].body ?? "{}")
    setRuntime(fn?.runtime ?? "python")
    setCode(fn?.code ?? TEMPLATES.script.code ?? "")
    setTimeoutSeconds(String(fn?.timeout_seconds ?? 30))
    setError(undefined)
  }, [open, fn])

  const changeKind = (k: FunctionDef["kind"]) => {
    setKind(k)
    if (!fn) {
      setParams(TEMPLATES[k].params)
      if (TEMPLATES[k].body) setBody(TEMPLATES[k].body)
    }
  }

  const submit = async (event: React.FormEvent) => {
    event.preventDefault()
    setBusy(true)
    setError(undefined)
    try {
      const def: FunctionDef = {
        name: name.trim(),
        kind,
        description,
        params: parseJson(params, "Parameters"),
        timeout_seconds: Number(timeout) || 30,
        ...(kind === "query" || kind === "aggregate" ? { tessellation: tessellation.trim() } : {}),
        ...(kind === "script" ? { runtime, code } : { body: parseJson(body, "The body") }),
      }
      if (fn) await api.updateFunction(fn.name, def)
      else await api.createFunction(def)
      toast.success(fn ? `Updated ${def.name}.` : `Created ${def.name}.`)
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
      <DialogContent className="max-h-[94svh] overflow-y-auto sm:max-w-3xl">
        <form onSubmit={submit} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>{fn ? `Edit ${fn.name}` : "New function"}</DialogTitle>
            <DialogDescription>
              Saved on the server and run by name. A function runs with the permissions of whoever runs it. Reference parameters as{" "}
              <code>{'{"$param": "name"}'}</code>.
            </DialogDescription>
          </DialogHeader>
          <div className="grid gap-3 sm:grid-cols-3">
            <div className="grid gap-1.5">
              <Label htmlFor="fn-name">Name</Label>
              <Input id="fn-name" value={name} onChange={(e) => setName(e.target.value)} disabled={!!fn} required placeholder="orders_by_status" />
            </div>
            <div className="grid gap-1.5">
              <Label>Kind</Label>
              <Select value={kind} onValueChange={(v) => changeKind(v as FunctionDef["kind"])}>
                <SelectTrigger className="w-full">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="query">Query</SelectItem>
                  <SelectItem value="aggregate">Aggregation</SelectItem>
                  <SelectItem value="transaction">Transaction</SelectItem>
                  <SelectItem value="script">Script</SelectItem>
                </SelectContent>
              </Select>
            </div>
            {kind === "query" || kind === "aggregate" ? (
              <div className="grid gap-1.5">
                <Label htmlFor="fn-tess">Tessellation</Label>
                <Input id="fn-tess" value={tessellation} onChange={(e) => setTessellation(e.target.value)} required placeholder="orders" />
              </div>
            ) : kind === "script" ? (
              <div className="grid gap-1.5">
                <Label>Runtime</Label>
                <Select value={runtime} onValueChange={(v) => setRuntime(v as NonNullable<FunctionDef["runtime"]>)}>
                  <SelectTrigger className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="python">Python</SelectItem>
                    <SelectItem value="typescript">TypeScript</SelectItem>
                    <SelectItem value="javascript">JavaScript</SelectItem>
                  </SelectContent>
                </Select>
              </div>
            ) : (
              <div />
            )}
          </div>
          <div className="grid gap-1.5">
            <Label htmlFor="fn-description">Description</Label>
            <Input id="fn-description" value={description} onChange={(e) => setDescription(e.target.value)} />
          </div>
          <div className="grid gap-1.5">
            <Label>Parameters</Label>
            <JsonEditor value={params} onChange={setParams} label="Parameters" className="h-24" />
            <p className="text-muted-foreground text-xs">Each: name, type (string, number, integer, boolean, object, array, any), required, default.</p>
          </div>
          {kind === "script" ? (
            <div className="grid gap-1.5">
              <div className="flex items-end justify-between gap-2">
                <Label>Code</Label>
                <label className="flex items-center gap-2 text-xs">
                  Timeout (s)
                  <Input type="number" min={1} max={600} value={timeout} onChange={(e) => setTimeoutSeconds(e.target.value)} className="h-7 w-20" />
                </label>
              </div>
              <JsonEditor value={code} onChange={setCode} label="Script code" plain className="h-64" />
              <p className="text-muted-foreground text-xs">
                Reads <code>{'{"params", "function", "caller"}'}</code> on stdin and prints its result as JSON. <code>HEXDB_API</code> and{" "}
                <code>HEXDB_TOKEN</code> (a session of whoever runs it, ended afterwards) let it call HexDB.
              </p>
            </div>
          ) : (
            <div className="grid gap-1.5">
              <Label>{kind === "transaction" ? "Operations" : kind === "aggregate" ? "Aggregation" : "Query"}</Label>
              <JsonEditor value={body} onChange={setBody} label="Function body" className="h-48" />
            </div>
          )}
          {error && <p className="text-destructive text-sm whitespace-pre-wrap">{error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)} disabled={busy}>
              Cancel
            </Button>
            <Button type="submit" disabled={busy || !name.trim()}>
              {busy && <IconLoader2 className="animate-spin" />}
              {fn ? "Save" : "Create function"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

function RunDialog({ fn, onOpenChange }: { fn: FunctionDef | null; onOpenChange: (open: boolean) => void }) {
  const [params, setParams] = useState("{}")
  const [result, setResult] = useState<{ text: string; ms: number }>()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()

  useEffect(() => {
    if (!fn) return
    const sample = Object.fromEntries(fn.params.map((p) => [p.name, p.default ?? (p.type === "number" || p.type === "integer" ? 0 : p.type === "boolean" ? false : "")]))
    setParams(JSON.stringify(sample, null, 2))
    setResult(undefined)
    setError(undefined)
  }, [fn])

  const run = async () => {
    if (!fn) return
    setBusy(true)
    setError(undefined)
    try {
      const out = await api.runFunction(fn.name, parseJson(params, "Parameters"))
      setResult({ text: JSON.stringify(out.result, null, 2), ms: out.ms })
    } catch (e) {
      setError(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open={fn !== null} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[94svh] overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>Run {fn?.name}</DialogTitle>
          <DialogDescription>{fn?.description || "Runs with your permissions."}</DialogDescription>
        </DialogHeader>
        <div className="grid gap-3">
          <Label>Parameters</Label>
          <JsonEditor value={params} onChange={setParams} label="Run parameters" className="h-28" />
          <Button className="justify-self-start" onClick={() => void run()} disabled={busy}>
            {busy ? <IconLoader2 className="animate-spin" /> : <IconPlayerPlay />} Run
          </Button>
          {error && <p className="text-destructive text-sm">{error}</p>}
          {result && (
            <>
              <p className="text-muted-foreground text-xs">Finished in {result.ms} ms.</p>
              <JsonEditor value={result.text} label="Result" readOnly className="h-64" />
            </>
          )}
        </div>
      </DialogContent>
    </Dialog>
  )
}

function ScheduleDialog({ schedule, functions, open, onOpenChange, onSaved }: { schedule: Schedule | null; functions: FunctionDef[]; open: boolean; onOpenChange: (open: boolean) => void; onSaved: () => void }) {
  const [name, setName] = useState("")
  const [fn, setFn] = useState("")
  const [mode, setMode] = useState<"every" | "cron">("every")
  const [every, setEvery] = useState("3600")
  const [cron, setCron] = useState("0 * * * *")
  const [params, setParams] = useState("{}")
  const [enabled, setEnabled] = useState(true)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()

  useEffect(() => {
    if (!open) return
    setName(schedule?.name ?? "")
    setFn(schedule?.function ?? functions[0]?.name ?? "")
    setMode(schedule?.cron ? "cron" : "every")
    setEvery(String(schedule?.every_seconds ?? 3600))
    setCron(schedule?.cron ?? "0 * * * *")
    setParams(JSON.stringify(schedule?.params ?? {}, null, 2))
    setEnabled(schedule?.enabled ?? true)
    setError(undefined)
  }, [open, schedule, functions])

  const submit = async (event: React.FormEvent) => {
    event.preventDefault()
    setBusy(true)
    setError(undefined)
    try {
      const s: Schedule = {
        name: name.trim(),
        function: fn,
        params: parseJson(params, "Parameters"),
        enabled,
        ...(mode === "every" ? { every_seconds: Number(every) } : { cron: cron.trim() }),
      }
      if (schedule) await api.updateSchedule(schedule.name, s)
      else await api.createSchedule(s)
      toast.success(schedule ? `Updated ${s.name}.` : `Created ${s.name}.`)
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
      <DialogContent className="max-h-[94svh] overflow-y-auto sm:max-w-xl">
        <form onSubmit={submit} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>{schedule ? `Edit ${schedule.name}` : "New schedule"}</DialogTitle>
            <DialogDescription>Runs a function on the Overseer, as you, on an interval or a cron expression (UTC).</DialogDescription>
          </DialogHeader>
          <div className="grid gap-3 sm:grid-cols-2">
            <div className="grid gap-1.5">
              <Label htmlFor="sch-name">Name</Label>
              <Input id="sch-name" value={name} onChange={(e) => setName(e.target.value)} disabled={!!schedule} required placeholder="nightly-cleanup" />
            </div>
            <div className="grid gap-1.5">
              <Label>Function</Label>
              <Select value={fn} onValueChange={setFn}>
                <SelectTrigger className="w-full">
                  <SelectValue placeholder="Choose a function" />
                </SelectTrigger>
                <SelectContent>
                  {functions.map((f) => (
                    <SelectItem key={f.name} value={f.name}>
                      {f.name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          </div>
          <div className="grid gap-3 sm:grid-cols-[9rem_1fr]">
            <Select value={mode} onValueChange={(v) => setMode(v as "every" | "cron")}>
              <SelectTrigger className="w-full">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="every">Every</SelectItem>
                <SelectItem value="cron">Cron</SelectItem>
              </SelectContent>
            </Select>
            {mode === "every" ? (
              <label className="flex items-center gap-2 text-sm">
                <Input type="number" min={10} value={every} onChange={(e) => setEvery(e.target.value)} className="w-32" /> seconds
              </label>
            ) : (
              <Input value={cron} onChange={(e) => setCron(e.target.value)} className="font-mono" placeholder="minute hour day month weekday, or @daily" />
            )}
          </div>
          <div className="grid gap-1.5">
            <Label>Parameters</Label>
            <JsonEditor value={params} onChange={setParams} label="Schedule parameters" className="h-24" />
          </div>
          <label className="flex items-center gap-2 text-sm">
            <Checkbox checked={enabled} onCheckedChange={(v) => setEnabled(v === true)} /> Enabled
          </label>
          {error && <p className="text-destructive text-sm">{error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)} disabled={busy}>
              Cancel
            </Button>
            <Button type="submit" disabled={busy || !name.trim() || !fn}>
              {busy && <IconLoader2 className="animate-spin" />}
              {schedule ? "Save" : "Create schedule"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

const when = (ms?: number) => (ms ? new Date(ms).toLocaleString() : "—")

/** Saved functions (queries, aggregations, transactions, scripts) and their schedules. */
export function FunctionsPage() {
  const { me } = useAuth()
  const admin = !!me?.is_admin
  const functions = usePoll(api.functions, 0)
  const schedules = usePoll(() => (admin ? api.schedules() : Promise.resolve([])), 5000, [admin])
  const [editing, setEditing] = useState<FunctionDef | null | undefined>(undefined)
  const [running, setRunning] = useState<FunctionDef | null>(null)
  const [deleting, setDeleting] = useState<{ kind: "function" | "schedule"; name: string } | null>(null)
  const [editingSchedule, setEditingSchedule] = useState<Schedule | null | undefined>(undefined)

  const runSchedule = async (name: string) => {
    try {
      await api.runSchedule(name)
      toast.success(`Ran ${name}.`)
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      void schedules.refresh()
    }
  }

  return (
    <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
      <Tabs defaultValue="functions">
        <div className="flex items-center gap-2">
          <TabsList>
            <TabsTrigger value="functions">Functions</TabsTrigger>
            {admin && <TabsTrigger value="schedules">Schedules</TabsTrigger>}
          </TabsList>
        </div>
        <TabsContent value="functions" className="grid gap-4 pt-2">
          <div className="flex items-start gap-4">
            <p className="text-muted-foreground max-w-3xl text-sm">
              Functions are saved queries, aggregations, transactions and scripts (Python, TypeScript, JavaScript). Anyone signed in can run them;
              they run with the runner's permissions. Administrators create them.
            </p>
            {admin && (
              <Button size="sm" className="ml-auto shrink-0" onClick={() => setEditing(null)}>
                <IconPlus /> New function
              </Button>
            )}
          </div>
          {functions.error && <p className="text-destructive text-sm">{functions.error.message}</p>}
          <Card className="gap-0 overflow-hidden py-0">
            <Table>
              <TableHeader className="bg-muted/60">
                <TableRow>
                  <TableHead className="pl-6">Function</TableHead>
                  <TableHead>Kind</TableHead>
                  <TableHead>Parameters</TableHead>
                  <TableHead className="w-32 pr-6" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {(functions.data ?? []).map((f) => (
                  <TableRow key={f.name}>
                    <TableCell className="pl-6">
                      <div className="font-medium">{f.name}</div>
                      <div className="text-muted-foreground text-xs">{f.description}</div>
                    </TableCell>
                    <TableCell>
                      <Badge variant="outline">{f.kind === "script" ? `${f.runtime} script` : f.kind}</Badge>
                      {f.tessellation && <span className="text-muted-foreground ml-1.5 text-xs">on {f.tessellation}</span>}
                    </TableCell>
                    <TableCell className="text-muted-foreground font-mono text-xs">{f.params.map((p) => `${p.name}${p.required ? "" : "?"}: ${p.type}`).join(", ") || "—"}</TableCell>
                    <TableCell className="pr-6">
                      <div className="flex justify-end gap-1">
                        <Button variant="ghost" size="icon" className="text-muted-foreground size-8" aria-label={`Run ${f.name}`} onClick={() => setRunning(f)}>
                          <IconPlayerPlay />
                        </Button>
                        {admin && (
                          <>
                            <Button variant="ghost" size="icon" className="text-muted-foreground size-8" aria-label={`Edit ${f.name}`} onClick={() => setEditing(f)}>
                              <IconPencil />
                            </Button>
                            <Button variant="ghost" size="icon" className="text-muted-foreground hover:text-destructive size-8" aria-label={`Delete ${f.name}`} onClick={() => setDeleting({ kind: "function", name: f.name })}>
                              <IconTrash />
                            </Button>
                          </>
                        )}
                      </div>
                    </TableCell>
                  </TableRow>
                ))}
                {functions.data?.length === 0 && (
                  <TableRow>
                    <TableCell colSpan={4} className="text-muted-foreground py-10 text-center">
                      No functions.
                    </TableCell>
                  </TableRow>
                )}
              </TableBody>
            </Table>
          </Card>
        </TabsContent>
        {admin && (
          <TabsContent value="schedules" className="grid gap-4 pt-2">
            <div className="flex items-start gap-4">
              <p className="text-muted-foreground max-w-3xl text-sm">Schedules run functions on the Overseer, as the administrator who created them.</p>
              <Button size="sm" className="ml-auto shrink-0" onClick={() => setEditingSchedule(null)} disabled={!functions.data?.length}>
                <IconPlus /> New schedule
              </Button>
            </div>
            <Card className="gap-0 overflow-hidden py-0">
              <Table>
                <TableHeader className="bg-muted/60">
                  <TableRow>
                    <TableHead className="pl-6">Schedule</TableHead>
                    <TableHead>When</TableHead>
                    <TableHead>Last run</TableHead>
                    <TableHead>Next run</TableHead>
                    <TableHead className="w-32 pr-6" />
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {(schedules.data ?? []).map((s) => (
                    <TableRow key={s.name}>
                      <TableCell className="pl-6">
                        <div className="font-medium">
                          {s.name} {!s.enabled && <Badge variant="secondary">paused</Badge>}
                        </div>
                        <div className="text-muted-foreground text-xs">
                          runs {s.function} as {s.run_as_login}
                        </div>
                      </TableCell>
                      <TableCell className="font-mono text-xs">{s.cron ?? `every ${s.every_seconds}s`}</TableCell>
                      <TableCell className="text-sm">
                        {s.last_status ? (
                          <span className={s.last_status === "ok" ? "" : "text-destructive"} title={s.last_error ?? undefined}>
                            {s.last_status} · {when(s.last_run)}
                          </span>
                        ) : (
                          "—"
                        )}
                        {s.runs ? <div className="text-muted-foreground text-xs">{s.runs} runs</div> : null}
                      </TableCell>
                      <TableCell className="text-muted-foreground text-sm">{s.enabled ? when(s.next_run) : "—"}</TableCell>
                      <TableCell className="pr-6">
                        <div className="flex justify-end gap-1">
                          <Button variant="ghost" size="icon" className="text-muted-foreground size-8" aria-label={`Run ${s.name} now`} onClick={() => void runSchedule(s.name)}>
                            <IconPlayerPlay />
                          </Button>
                          <Button variant="ghost" size="icon" className="text-muted-foreground size-8" aria-label={`Edit ${s.name}`} onClick={() => setEditingSchedule(s)}>
                            <IconPencil />
                          </Button>
                          <Button variant="ghost" size="icon" className="text-muted-foreground hover:text-destructive size-8" aria-label={`Delete ${s.name}`} onClick={() => setDeleting({ kind: "schedule", name: s.name })}>
                            <IconTrash />
                          </Button>
                        </div>
                      </TableCell>
                    </TableRow>
                  ))}
                  {schedules.data?.length === 0 && (
                    <TableRow>
                      <TableCell colSpan={5} className="text-muted-foreground py-10 text-center">
                        No schedules.
                      </TableCell>
                    </TableRow>
                  )}
                </TableBody>
              </Table>
            </Card>
          </TabsContent>
        )}
      </Tabs>
      <FunctionDialog fn={editing ?? null} open={editing !== undefined} onOpenChange={(open) => !open && setEditing(undefined)} onSaved={() => void functions.refresh()} />
      <RunDialog fn={running} onOpenChange={(open) => !open && setRunning(null)} />
      <ScheduleDialog
        schedule={editingSchedule ?? null}
        functions={functions.data ?? []}
        open={editingSchedule !== undefined}
        onOpenChange={(open) => !open && setEditingSchedule(undefined)}
        onSaved={() => void schedules.refresh()}
      />
      <ConfirmDialog
        open={deleting !== null}
        onOpenChange={(open) => !open && setDeleting(null)}
        title={`Delete the ${deleting?.kind} ${deleting?.name}?`}
        description="It can't be undone."
        confirmLabel={`Delete ${deleting?.kind}`}
        onConfirm={async () => {
          if (deleting!.kind === "function") await api.deleteFunction(deleting!.name)
          else await api.deleteSchedule(deleting!.name)
          toast.success(`Deleted ${deleting!.name}.`)
          void functions.refresh()
          void schedules.refresh()
        }}
      />
    </div>
  )
}
