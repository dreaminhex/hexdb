import { useState } from "react"
import { IconChevronLeft, IconChevronRight, IconSearch } from "@tabler/icons-react"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { usePoll } from "@/hooks/use-poll"
import { api, type AuditEvent } from "@/lib/api"
import { setQueryParam, useQueryParam } from "@/lib/router"
import { cn } from "@/lib/utils"

const PAGE = 100

const AREAS = [
  { value: "all", label: "Everything" },
  { value: "auth.", label: "Sign-ins and credentials" },
  { value: "user.", label: "Users" },
  { value: "role.", label: "Roles" },
  { value: "access.", label: "Refused requests" },
  { value: "tessellation.", label: "Tessellations" },
  { value: "index.", label: "Indexes" },
  { value: "maintenance.", label: "Maintenance" },
  { value: "settings.", label: "Settings" },
  { value: "server.", label: "Server" },
]

const OUTCOMES = [
  { value: "all", label: "Any outcome" },
  { value: "ok", label: "Succeeded" },
  { value: "failed", label: "Failed" },
  { value: "denied", label: "Denied" },
]

const OUTCOME_STYLE: Record<string, string> = {
  ok: "text-muted-foreground",
  failed: "text-destructive",
  denied: "text-status-warning",
}

function details(event: AuditEvent): string {
  const entries = Object.entries(event.details ?? {})
  if (entries.length === 0) return ""
  return entries.map(([k, v]) => `${k}: ${typeof v === "string" ? v : JSON.stringify(v)}`).join(" · ")
}

/** Security events kept by the server (sign-ins, credential and permission changes, refused requests, ...), newest first. */
export function AuditPage() {
  const area = useQueryParam("area") ?? "all"
  const outcome = useQueryParam("outcome") ?? "all"
  const actor = useQueryParam("actor") ?? ""
  const [actorDraft, setActorDraft] = useState(actor)
  const [offset, setOffset] = useState(0)

  const events = usePoll(
    () =>
      api.audit({
        action: area === "all" ? undefined : area,
        outcome: outcome === "all" ? undefined : outcome,
        actor: actor || undefined,
        limit: PAGE,
        offset,
      }),
    10_000,
    [area, outcome, actor, offset],
  )
  const total = events.data?.total ?? 0

  const setFilter = (key: string, value: string) => {
    setOffset(0)
    setQueryParam(key, value === "all" || value === "" ? null : value)
  }

  return (
    <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
      <p className="text-muted-foreground max-w-3xl text-sm">
        Every hex records sign-ins, credential and permission changes, schema changes, maintenance and refused requests here. Events are kept
        for <code>security.audit_retention_days</code> and replicate across the lattice like any other data.
      </p>
      <div className="flex flex-wrap items-center gap-2">
        <Select value={area} onValueChange={(v) => setFilter("area", v)}>
          <SelectTrigger className="w-56" aria-label="Area">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {AREAS.map((a) => (
              <SelectItem key={a.value} value={a.value}>
                {a.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Select value={outcome} onValueChange={(v) => setFilter("outcome", v)}>
          <SelectTrigger className="w-40" aria-label="Outcome">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {OUTCOMES.map((o) => (
              <SelectItem key={o.value} value={o.value}>
                {o.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <form
          className="relative"
          onSubmit={(e) => {
            e.preventDefault()
            setFilter("actor", actorDraft.trim())
          }}
        >
          <IconSearch className="text-muted-foreground absolute top-1/2 left-2.5 size-4 -translate-y-1/2" />
          <Input value={actorDraft} onChange={(e) => setActorDraft(e.target.value)} placeholder="Actor (login)" className="w-48 pl-8" aria-label="Actor" />
        </form>
        <div className="text-muted-foreground ml-auto flex items-center gap-2 text-sm">
          {total > 0 ? `${offset + 1}-${Math.min(offset + PAGE, total)} of ${total}` : events.data ? "No events" : "Loading…"}
          <Button variant="outline" size="icon" className="size-8" aria-label="Newer" disabled={offset === 0} onClick={() => setOffset(Math.max(0, offset - PAGE))}>
            <IconChevronLeft />
          </Button>
          <Button variant="outline" size="icon" className="size-8" aria-label="Older" disabled={offset + PAGE >= total} onClick={() => setOffset(offset + PAGE)}>
            <IconChevronRight />
          </Button>
        </div>
      </div>
      {events.error && <p className="text-destructive text-sm">{events.error.message}</p>}
      <Card className="gap-0 overflow-hidden py-0">
        <Table>
          <TableHeader className="bg-muted/60">
            <TableRow>
              <TableHead className="pl-6">Time</TableHead>
              <TableHead>Actor</TableHead>
              <TableHead>Action</TableHead>
              <TableHead>Target</TableHead>
              <TableHead>Outcome</TableHead>
              <TableHead>Client</TableHead>
              <TableHead className="pr-6">Details</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {(events.data?.events ?? []).map((event, i) => (
              <TableRow key={`${event.time}-${i}`}>
                <TableCell className="text-muted-foreground pl-6 text-xs whitespace-nowrap tabular-nums">{new Date(event.time).toLocaleString()}</TableCell>
                <TableCell className="font-medium">{event.actor}</TableCell>
                <TableCell>
                  <Badge variant="outline" className="font-mono font-normal">
                    {event.action}
                  </Badge>
                </TableCell>
                <TableCell className="max-w-56 truncate font-mono text-xs" title={event.target}>
                  {event.target}
                </TableCell>
                <TableCell className={cn("text-sm", OUTCOME_STYLE[event.outcome])}>{event.outcome}</TableCell>
                <TableCell className="text-muted-foreground font-mono text-xs">
                  {event.client}
                  {event.hex && <div className="font-sans">{event.hex}</div>}
                </TableCell>
                <TableCell className="text-muted-foreground max-w-80 pr-6 text-xs whitespace-normal">{details(event)}</TableCell>
              </TableRow>
            ))}
            {events.data && events.data.events.length === 0 && (
              <TableRow>
                <TableCell colSpan={7} className="text-muted-foreground py-10 text-center">
                  No events match.
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </Card>
    </div>
  )
}
