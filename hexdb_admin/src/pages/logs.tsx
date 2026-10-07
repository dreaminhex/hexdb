import { useCallback, useEffect, useRef, useState } from "react"
import { IconPlayerPause, IconPlayerPlay, IconRefresh, IconSearch } from "@tabler/icons-react"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { api, errorMessage, type LogRecord } from "@/lib/api"
import { setQueryParam, useQueryParam } from "@/lib/router"
import { cn } from "@/lib/utils"

const PAGE = 300
/** Records kept on screen while tailing; older ones drop off the top. */
const MAX_ON_SCREEN = 3000
const TAIL_MS = 2000

const LEVELS = [
  { value: "all", label: "All levels" },
  { value: "error", label: "Errors" },
  { value: "warn", label: "Warnings and up" },
  { value: "info", label: "Info and up" },
  { value: "debug", label: "Debug and up" },
]

const LEVEL_STYLE: Record<string, string> = {
  ERROR: "text-destructive",
  WARN: "text-status-warning",
  INFO: "text-foreground",
  DEBUG: "text-muted-foreground",
  TRACE: "text-muted-foreground",
}

/** The server's recent log records (kept in memory, newest last), with live tailing. */
export function LogsPage() {
  const level = useQueryParam("level") ?? "all"
  const target = useQueryParam("target") ?? ""
  const search = useQueryParam("q") ?? ""
  const [searchDraft, setSearchDraft] = useState(search)
  const [records, setRecords] = useState<LogRecord[]>([])
  const [error, setError] = useState<string>()
  const [live, setLive] = useState(true)
  const [olderAvailable, setOlderAvailable] = useState(false)
  const [capacity, setCapacity] = useState(0)
  const [expanded, setExpanded] = useState<Set<number>>(new Set())
  const lastSeq = useRef(0)
  const scroller = useRef<HTMLDivElement>(null)
  const pinnedToBottom = useRef(true)

  const filters = { level: level === "all" ? undefined : level, target: target || undefined, q: search || undefined }
  const filterKey = JSON.stringify(filters)

  const scrollToBottom = () => {
    requestAnimationFrame(() => {
      const el = scroller.current
      if (el && pinnedToBottom.current) el.scrollTop = el.scrollHeight
    })
  }

  // Load the newest page whenever the filters change.
  const reload = useCallback(async () => {
    try {
      const page = await api.logs({ ...JSON.parse(filterKey), limit: PAGE })
      setRecords(page.records)
      setCapacity(page.capacity)
      setOlderAvailable(page.records.length === PAGE)
      lastSeq.current = page.last_seq
      setError(undefined)
      pinnedToBottom.current = true
      scrollToBottom()
    } catch (e) {
      setError(errorMessage(e))
    }
  }, [filterKey])

  useEffect(() => {
    void reload()
  }, [reload])

  // Tail: fetch records after the last one we've seen.
  useEffect(() => {
    if (!live) return
    const timer = window.setInterval(async () => {
      if (document.visibilityState !== "visible") return
      try {
        const page = await api.logs({ ...JSON.parse(filterKey), after: lastSeq.current, limit: 1000 })
        lastSeq.current = Math.max(page.last_seq, lastSeq.current)
        if (page.records.length) {
          setRecords((current) => [...current, ...page.records].slice(-MAX_ON_SCREEN))
          scrollToBottom()
        }
        setError(undefined)
      } catch (e) {
        setError(errorMessage(e))
      }
    }, TAIL_MS)
    return () => window.clearInterval(timer)
  }, [live, filterKey])

  const loadOlder = async () => {
    const first = records[0]
    if (!first) return
    try {
      const page = await api.logs({ ...filters, before: first.seq, limit: PAGE })
      const el = scroller.current
      const previousHeight = el?.scrollHeight ?? 0
      setRecords((current) => [...page.records, ...current])
      setOlderAvailable(page.records.length === PAGE)
      // Keep the view anchored on what the reader was looking at.
      requestAnimationFrame(() => {
        if (el) el.scrollTop += el.scrollHeight - previousHeight
      })
    } catch (e) {
      setError(errorMessage(e))
    }
  }

  const toggle = (seq: number) =>
    setExpanded((current) => {
      const next = new Set(current)
      if (next.has(seq)) next.delete(seq)
      else next.add(seq)
      return next
    })

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-4 p-4 lg:p-6">
      <div className="flex flex-wrap items-center gap-2">
        <Select value={level} onValueChange={(v) => v && setQueryParam("level", v === "all" ? null : v)}>
          <SelectTrigger className="w-44" aria-label="Minimum level">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {LEVELS.map((l) => (
              <SelectItem key={l.value} value={l.value}>
                {l.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <form
          className="relative min-w-48 flex-1"
          onSubmit={(e) => {
            e.preventDefault()
            setQueryParam("q", searchDraft.trim() || null)
          }}
        >
          <IconSearch className="text-muted-foreground pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2" />
          <Input
            className="pl-8"
            placeholder="Search messages and fields, then press Enter"
            value={searchDraft}
            onChange={(e) => setSearchDraft(e.target.value)}
            onBlur={() => setQueryParam("q", searchDraft.trim() || null)}
          />
        </form>
        <Input
          className="w-56 font-mono text-xs"
          placeholder="Module, e.g. hexdb_api"
          aria-label="Module prefix"
          defaultValue={target}
          onKeyDown={(e) => {
            if (e.key === "Enter") setQueryParam("target", e.currentTarget.value.trim() || null)
          }}
          onBlur={(e) => setQueryParam("target", e.currentTarget.value.trim() || null)}
        />
        <Button variant={live ? "secondary" : "outline"} onClick={() => setLive((v) => !v)} title={live ? "Pause tailing" : "Resume tailing"}>
          {live ? <IconPlayerPause /> : <IconPlayerPlay />}
          {live ? "Live" : "Paused"}
        </Button>
        <Button variant="outline" size="icon" onClick={() => void reload()} title="Reload" aria-label="Reload">
          <IconRefresh />
        </Button>
      </div>

      {error && <p className="text-destructive text-sm">{error}</p>}

      <Card className="gap-0 overflow-hidden py-0">
        <div
          ref={scroller}
          className="h-[calc(100svh-13rem)] min-h-[20rem] overflow-auto font-mono text-xs"
          onScroll={(e) => {
            const el = e.currentTarget
            pinnedToBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40
          }}
        >
          {olderAvailable && (
            <div className="border-b p-2 text-center font-sans">
              <Button variant="ghost" size="sm" onClick={() => void loadOlder()}>
                Load older records
              </Button>
            </div>
          )}
          {records.length === 0 && !error && (
            <p className="text-muted-foreground p-6 font-sans text-sm">No log records match these filters.</p>
          )}
          {records.map((r) => {
            const fields = Object.entries(r.fields ?? {})
            const open = expanded.has(r.seq)
            return (
              <div
                key={r.seq}
                className={cn("border-b px-4 py-1.5 last:border-b-0", fields.length > 0 && "hover:bg-muted/50 cursor-pointer")}
                onClick={() => fields.length > 0 && toggle(r.seq)}
              >
                <div className="flex gap-3">
                  <time className="text-muted-foreground shrink-0 tabular-nums" dateTime={r.timestamp} title={r.timestamp}>
                    {formatTime(r.timestamp)}
                  </time>
                  <span className={cn("w-12 shrink-0 font-semibold", LEVEL_STYLE[r.level])}>{r.level}</span>
                  <span className="text-muted-foreground hidden w-56 shrink-0 truncate lg:block" title={r.target}>
                    {r.target}
                  </span>
                  <span className="min-w-0 flex-1 break-words whitespace-pre-wrap">{r.message}</span>
                  {fields.length > 0 && !open && (
                    <Badge variant="outline" className="shrink-0 font-sans text-[10px]">
                      {fields.length} field{fields.length === 1 ? "" : "s"}
                    </Badge>
                  )}
                </div>
                {open && (
                  <dl className="text-muted-foreground mt-1 ml-[7.5rem] grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5">
                    <dt className="lg:hidden">target</dt>
                    <dd className="text-foreground break-all lg:hidden">{r.target}</dd>
                    {fields.map(([k, v]) => (
                      <div key={k} className="contents">
                        <dt>{k}</dt>
                        <dd className="text-foreground break-all">{v}</dd>
                      </div>
                    ))}
                  </dl>
                )}
              </div>
            )
          })}
        </div>
      </Card>
      <p className="text-muted-foreground text-xs">
        The server keeps its {capacity ? capacity.toLocaleString() : "most recent"} log records in memory. Reads are logged at debug level, which
        the default filter (RUST_LOG=info) leaves out. Writes, errors, and background tasks are logged at info and above.
      </p>
    </div>
  )
}

function formatTime(iso: string): string {
  const d = new Date(iso)
  const time = d.toLocaleTimeString(undefined, { hour12: false })
  const ms = String(d.getMilliseconds()).padStart(3, "0")
  const today = new Date().toDateString() === d.toDateString()
  return today ? `${time}.${ms}` : `${d.toLocaleDateString(undefined, { month: "short", day: "numeric" })} ${time}`
}
