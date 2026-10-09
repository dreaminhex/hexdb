import { useMemo, useState } from "react"
import { Area, AreaChart, CartesianGrid, Line, LineChart, XAxis, YAxis } from "recharts"
import {
  IconAlertTriangle,
  IconArchive,
  IconCircleCheck,
  IconDatabase,
  IconDeviceFloppy,
  IconHexagon,
  IconHexagons,
  IconLoader2,
  IconLock,
  IconPlus,
  IconRefresh,
} from "@tabler/icons-react"
import { toast } from "sonner"

import { JoinDialog } from "@/components/join-dialog"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardAction, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import {
  type ChartConfig,
  ChartContainer,
  ChartLegend,
  ChartLegendContent,
  ChartTooltip,
  ChartTooltipContent,
} from "@/components/ui/chart"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group"
import { usePoll } from "@/hooks/use-poll"
import { api, errorMessage, type MetricsSample, type Status, type VertexStatus } from "@/lib/api"
import { has, useAuth } from "@/lib/auth"
import { formatBytes, formatDuration, formatNumber, formatPercent } from "@/lib/format"
import { href, linkHandler } from "@/lib/router"
import { cn } from "@/lib/utils"

const SERIES = ["var(--series-1)", "var(--series-2)", "var(--series-3)", "var(--series-4)", "var(--series-5)"]
/** Tessellations beyond this many fold into "Other". */
const MAX_SERIES = 4

// ---------------------------------------------------------------------------
// Stat cards
// ---------------------------------------------------------------------------

function Stat({
  label,
  value,
  detail,
  meter,
}: {
  label: string
  value: string
  detail: React.ReactNode
  /** 0-1 fill for a usage bar. */
  meter?: number
}) {
  return (
    <Card className="@container/card gap-2 py-4">
      <CardHeader className="px-4">
        <CardDescription>{label}</CardDescription>
        <CardTitle className="text-2xl font-semibold tabular-nums @[250px]/card:text-3xl">{value}</CardTitle>
      </CardHeader>
      <CardContent className="text-muted-foreground space-y-2 px-4 text-xs">
        {meter !== undefined && (
          <div className="bg-muted h-1.5 overflow-hidden rounded-full" role="meter" aria-valuenow={Math.round(meter * 100)} aria-valuemin={0} aria-valuemax={100}>
            <div
              className={cn("h-full rounded-full", meter > 0.9 ? "bg-destructive" : "bg-primary")}
              style={{ width: formatPercent(meter) }}
            />
          </div>
        )}
        <div>{detail}</div>
      </CardContent>
    </Card>
  )
}

/** Per-minute rate of a cumulative counter between the last two samples. */
function ratePerMinute(samples: MetricsSample[], key: "reads_total" | "writes_total" | "queries_total"): number | null {
  if (samples.length < 2) return null
  const [a, b] = samples.slice(-2)
  const minutes = (Date.parse(b.timestamp) - Date.parse(a.timestamp)) / 60000
  return minutes > 0 ? Math.max(0, (b[key] - a[key]) / minutes) : null
}

function StatCards({ status, samples }: { status: Status; samples: MetricsSample[] }) {
  const user = status.metrics.tessellations.filter((t) => t.kind === "user")
  const userDocs = user.reduce((sum, t) => sum + t.document_count, 0)
  const userInRam = user.reduce((sum, t) => sum + t.documents_in_ram, 0)
  const userOnDisk = user.reduce((sum, t) => sum + t.documents_on_disk, 0)
  const queries = ratePerMinute(samples, "queries_total")
  const perMinute = (rate: number | null) => (rate === null ? "—" : formatNumber(Math.round(rate)))
  const memory = status.storage.memory_bytes / status.storage.ram_budget_bytes
  const diskBudget = status.disk_mb * 1024 * 1024
  const writes = ratePerMinute(samples, "writes_total")
  const reads = ratePerMinute(samples, "reads_total")

  return (
    <div className="grid grid-cols-1 gap-4 px-4 @xl/main:grid-cols-2 @5xl/main:grid-cols-3 lg:px-6 @7xl/main:grid-cols-6">
      <Stat
        label="Documents"
        value={formatNumber(userDocs)}
        detail={`${formatNumber(userInRam)} in memory · ${formatNumber(userOnDisk)} on disk only`}
      />
      <Stat
        label="Tessellations"
        value={formatNumber(user.length)}
        detail={`plus ${status.metrics.tessellations.length - user.length} system`}
      />
      <Stat
        label="Memory"
        value={formatBytes(status.storage.memory_bytes)}
        meter={memory}
        detail={`${formatPercent(memory)} of ${formatBytes(status.storage.ram_budget_bytes)} · ${formatNumber(status.storage.unflushed_entries)} unflushed`}
      />
      <Stat
        label="Disk"
        value={formatBytes(status.storage.disk_bytes)}
        meter={diskBudget > 0 ? status.storage.disk_bytes / diskBudget : undefined}
        detail={`${formatNumber(status.storage.sstable_files)} SSTable file${status.storage.sstable_files === 1 ? "" : "s"} · ${formatBytes(diskBudget)} budget`}
      />
      <Stat
        label="Writes / min"
        value={perMinute(writes)}
        detail={`${perMinute(reads)} reads · ${perMinute(queries)} queries per minute`}
      />
      <Stat
        label="Uptime"
        value={formatDuration(status.uptime_seconds)}
        detail={`${status.name} · ${status.hex_type} · v${status.version}`}
      />
    </div>
  )
}

// ---------------------------------------------------------------------------
// Activity chart
// ---------------------------------------------------------------------------

type ChartMode = "documents" | "operations" | "storage"

const RANGES = [
  { value: "15", label: "Last 15 minutes" },
  { value: "60", label: "Last hour" },
  { value: "360", label: "Last 6 hours" },
]

function timeLabel(value: number) {
  return new Date(value).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
}

/** Stable series for tessellations: the largest keep their own color, the rest fold into "Other". */
function tessellationSeries(samples: MetricsSample[]): { keys: string[]; other: boolean } {
  const latest = samples[samples.length - 1]?.documents ?? {}
  const names = Array.from(new Set(samples.flatMap((s) => Object.keys(s.documents))))
  // Order by size so the biggest get the first (most distinct) colors; ties by name.
  names.sort((a, b) => (latest[b] ?? 0) - (latest[a] ?? 0) || a.localeCompare(b))
  return { keys: names.slice(0, MAX_SERIES), other: names.length > MAX_SERIES }
}

function ActivityChart({ samples, minutes, onMinutesChange }: { samples: MetricsSample[]; minutes: string; onMinutesChange: (v: string) => void }) {
  const [mode, setMode] = useState<ChartMode>("documents")

  const { data, config, keys, stacked, valueFormat } = useMemo(() => {
    const t = (s: MetricsSample) => Date.parse(s.timestamp)
    if (mode === "documents") {
      const { keys, other } = tessellationSeries(samples)
      const all = other ? [...keys, "__other"] : keys
      const config: ChartConfig = Object.fromEntries(
        all.map((key, i) => [key, { label: key === "__other" ? "Other" : key, color: SERIES[i] }]),
      )
      const data = samples.map((s) => {
        const row: Record<string, number> = { t: t(s) }
        for (const key of keys) row[key] = s.documents[key] ?? 0
        if (other) {
          row.__other = Object.entries(s.documents)
            .filter(([name]) => !keys.includes(name))
            .reduce((sum, [, n]) => sum + n, 0)
        }
        return row
      })
      return { data, config, keys: all, stacked: true, valueFormat: formatNumber }
    }
    if (mode === "operations") {
      const config: ChartConfig = {
        writes: { label: "Writes", color: SERIES[0] },
        reads: { label: "Reads", color: SERIES[1] },
        queries: { label: "Queries", color: SERIES[2] },
      }
      const data = samples.slice(1).map((s, i) => {
        const prev = samples[i]
        const minutes = Math.max((t(s) - t(prev)) / 60000, 1 / 60)
        return {
          t: t(s),
          writes: Math.max(0, (s.writes_total - prev.writes_total) / minutes),
          reads: Math.max(0, (s.reads_total - prev.reads_total) / minutes),
          queries: Math.max(0, (s.queries_total - prev.queries_total) / minutes),
        }
      })
      return { data, config, keys: ["writes", "reads", "queries"], stacked: false, valueFormat: (v: number) => `${formatNumber(Math.round(v))}/min` }
    }
    const config: ChartConfig = {
      memory: { label: "Memory", color: SERIES[0] },
      disk: { label: "Disk", color: SERIES[1] },
    }
    const data = samples.map((s) => ({ t: t(s), memory: s.memory_bytes, disk: s.disk_bytes }))
    return { data, config, keys: ["memory", "disk"], stacked: false, valueFormat: formatBytes }
  }, [mode, samples])

  const tooltip = (
    <ChartTooltip
      cursor={{ stroke: "var(--border)" }}
      content={
        <ChartTooltipContent
          indicator="dot"
          labelFormatter={(_, payload) => {
            const time = payload?.[0]?.payload?.t
            return time ? new Date(time).toLocaleTimeString() : ""
          }}
          formatter={(value, name, item) => (
            <div className="flex w-full items-center gap-2">
              <span className="size-2.5 shrink-0 rounded-[2px]" style={{ background: item.color }} />
              <span className="text-muted-foreground">{config[String(name)]?.label ?? name}</span>
              <span className="text-foreground ml-auto font-mono tabular-nums">{valueFormat(Number(value))}</span>
            </div>
          )}
        />
      }
    />
  )

  // Wide enough for the longest tick label: the axis rounds the top value up,
  // so measure a value a little above the largest point (stacked: the sum).
  const yLabel = (v: number) => valueFormat(v).replace("/min", "")
  const top = data.reduce((max, row) => {
    const values = keys.map((k) => Number((row as Record<string, number>)[k] ?? 0))
    return Math.max(max, stacked ? values.reduce((a, b) => a + b, 0) : Math.max(0, ...values))
  }, 0)
  const yWidth = Math.max(56, yLabel(top * 1.25).length * 7 + 12)

  // Recharts finds its children by type and doesn't look inside fragments, so use a keyed array.
  const axes = [
    <CartesianGrid key="grid" vertical={false} stroke="var(--border)" />,
    <XAxis key="x" dataKey="t" type="number" scale="time" domain={["dataMin", "dataMax"]} tickFormatter={timeLabel} tickLine={false} axisLine={false} tickMargin={8} minTickGap={48} />,
    <YAxis key="y" tickFormatter={yLabel} tickLine={false} axisLine={false} width={yWidth} />,
  ]

  return (
    <Card className="@container/card">
      <CardHeader>
        <CardTitle>Activity</CardTitle>
        <CardDescription>
          {mode === "documents" && "Documents per tessellation (stacked)"}
          {mode === "operations" && "Operations per minute"}
          {mode === "storage" && "Memory and disk use"}
        </CardDescription>
        <CardAction className="flex flex-wrap items-center gap-2">
          <ToggleGroup type="single" value={mode} onValueChange={(v) => v && setMode(v as ChartMode)} variant="outline" size="sm">
            <ToggleGroupItem value="documents" className="px-3">Documents</ToggleGroupItem>
            <ToggleGroupItem value="operations" className="px-3">Operations</ToggleGroupItem>
            <ToggleGroupItem value="storage" className="px-3">Storage</ToggleGroupItem>
          </ToggleGroup>
          <Select value={minutes} onValueChange={onMinutesChange}>
            <SelectTrigger size="sm" className="w-40" aria-label="Time range">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {RANGES.map((r) => (
                <SelectItem key={r.value} value={r.value}>
                  {r.label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </CardAction>
      </CardHeader>
      <CardContent className="px-2 sm:px-6">
        {data.length < 2 ? (
          <div className="text-muted-foreground flex h-[260px] items-center justify-center text-sm">
            Collecting samples… a point is recorded every 15 seconds.
          </div>
        ) : keys.length === 0 ? (
          <div className="text-muted-foreground flex h-[260px] items-center justify-center text-sm">No user tessellations yet.</div>
        ) : (
          <ChartContainer config={config} className="aspect-auto h-[260px] w-full">
            {stacked ? (
              <AreaChart data={data} margin={{ left: 4, right: 12, top: 8 }}>
                {axes}
                {tooltip}
                {keys.map((key) => (
                  <Area
                    key={key}
                    dataKey={key}
                    type="monotone"
                    stackId="docs"
                    stroke={`var(--color-${key})`}
                    strokeWidth={2}
                    fill={`var(--color-${key})`}
                    fillOpacity={0.18}
                    isAnimationActive={false}
                  />
                ))}
                {keys.length > 1 && <ChartLegend content={<ChartLegendContent />} />}
              </AreaChart>
            ) : (
              <LineChart data={data} margin={{ left: 4, right: 12, top: 8 }}>
                {axes}
                {tooltip}
                {keys.map((key) => (
                  <Line key={key} dataKey={key} type="monotone" stroke={`var(--color-${key})`} strokeWidth={2} dot={false} activeDot={{ r: 4 }} isAnimationActive={false} />
                ))}
                <ChartLegend content={<ChartLegendContent />} />
              </LineChart>
            )}
          </ChartContainer>
        )}
      </CardContent>
    </Card>
  )
}

// ---------------------------------------------------------------------------
// Vertices and lattice
// ---------------------------------------------------------------------------

/** Positions of six vertices on a pointy-top hexagon, starting at the top. */
function vertexPoint(index: number, radius: number, cx: number, cy: number) {
  const angle = (Math.PI / 3) * index - Math.PI / 2
  return { x: cx + radius * Math.cos(angle), y: cy + radius * Math.sin(angle) }
}

function VertexCard({ vertices }: { vertices: VertexStatus[] }) {
  const size = 220
  const c = size / 2
  const r = 78
  const points = vertices.map((_, i) => vertexPoint(i, r, c, c))
  const repaired = vertices.filter((v) => v.corrupt_shards_found > 0)
  const totalBytes = vertices.reduce((sum, v) => sum + v.bytes, 0)

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <IconHexagons className="text-muted-foreground size-5" /> Vertices
        </CardTitle>
        <CardDescription>4 data + 2 parity shards per document; any 2 vertices can be lost.</CardDescription>
        <CardAction>
          {repaired.length === 0 ? (
            <Badge variant="outline" className="gap-1">
              <IconCircleCheck className="text-status-good size-3.5" /> All healthy
            </Badge>
          ) : (
            <Badge variant="outline" className="gap-1">
              <IconAlertTriangle className="text-status-warning size-3.5" /> {repaired.length} repaired
            </Badge>
          )}
        </CardAction>
      </CardHeader>
      <CardContent className="flex flex-col items-center gap-4 sm:flex-row">
        <svg viewBox={`0 0 ${size} ${size}`} className="w-48 shrink-0" role="img" aria-label="Vertex health diagram">
          <polygon points={points.map((p) => `${p.x},${p.y}`).join(" ")} fill="color-mix(in oklch, var(--primary) 8%, transparent)" stroke="var(--border)" strokeWidth={2} />
          {points.map((p, i) => (
            <line key={`spoke-${i}`} x1={c} y1={c} x2={p.x} y2={p.y} stroke="var(--border)" strokeWidth={1} />
          ))}
          <text x={c} y={c - 4} textAnchor="middle" className="fill-foreground text-[13px] font-semibold">
            {formatBytes(totalBytes)}
          </text>
          <text x={c} y={c + 12} textAnchor="middle" className="fill-muted-foreground text-[10px]">
            shards in memory
          </text>
          {vertices.map((v, i) => {
            const p = points[i]
            const warn = v.corrupt_shards_found > 0
            return (
              <g key={v.id}>
                <title>{`Vertex ${v.id}: ${v.shards} shards, ${formatBytes(v.bytes)}${warn ? `, ${v.corrupt_shards_found} corrupt found, ${v.shards_repaired} repaired` : ""}`}</title>
                <circle cx={p.x} cy={p.y} r={15} fill="var(--card)" stroke={warn ? "var(--status-warning)" : "var(--status-good)"} strokeWidth={2.5} />
                <text x={p.x} y={p.y + 4} textAnchor="middle" className="fill-foreground text-[11px] font-semibold">
                  {i < 4 ? `D${i + 1}` : `P${i - 3}`}
                </text>
              </g>
            )
          })}
        </svg>
        <div className="grid w-full grid-cols-2 gap-x-6 gap-y-2 text-sm">
          {vertices.map((v, i) => (
            <div key={v.id} className="flex items-center justify-between gap-2">
              <span className="flex items-center gap-1.5">
                {v.corrupt_shards_found > 0 ? (
                  <IconAlertTriangle className="text-status-warning size-3.5" aria-label="repaired" />
                ) : (
                  <IconCircleCheck className="text-status-good size-3.5" aria-label="healthy" />
                )}
                {i < 4 ? `Data ${i + 1}` : `Parity ${i - 3}`}
              </span>
              <span className="text-muted-foreground font-mono text-xs tabular-nums">{formatBytes(v.bytes)}</span>
            </div>
          ))}
        </div>
      </CardContent>
    </Card>
  )
}

function LatticeCard({ status }: { status: Status }) {
  const { lattice } = status.network
  const active = lattice.hexes.filter((h) => h.status === "active").length
  const { me } = useAuth()
  const [joining, setJoining] = useState(false)
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <IconDatabase className="text-muted-foreground size-5" /> Lattice
        </CardTitle>
        <CardDescription>
          {lattice.name} · {active} of {lattice.hexes.length} hex{lattice.hexes.length === 1 ? "" : "es"} active
        </CardDescription>
        {has(me, "admin") && (
          <CardAction>
            <Button variant="outline" size="sm" onClick={() => setJoining(true)}>
              <IconPlus /> Add a hex
            </Button>
          </CardAction>
        )}
        <JoinDialog open={joining} onOpenChange={setJoining} />
      </CardHeader>
      <CardContent className="space-y-2">
        {lattice.hexes.map((hex) => (
          <div key={hex.id || hex.name} className="flex items-center gap-3 rounded-md border px-3 py-2">
            <span
              className={cn("size-2 shrink-0 rounded-full", hex.status === "active" ? "bg-status-good" : "bg-status-warning")}
              title={hex.status}
            />
            <div className="min-w-0 flex-1">
              <div className="truncate text-sm font-medium">
                {hex.name}
                {hex.is_self && <span className="text-muted-foreground ml-1.5 text-xs font-normal">(this hex)</span>}
                {hex.status !== "active" && <span className="text-muted-foreground ml-1.5 text-xs font-normal">· {hex.status}</span>}
              </div>
              <div
                className="text-muted-foreground truncate font-mono text-xs"
                title={`Discovery ${hex.ip}. ${hex.is_self ? "" : "Replication figures are as of the last discovery round."}`}
              >
                {hex.api_endpoint || hex.ip}
                {hex.status === "active" && <> · {replicationLabel(hex)}</>}
                {!hex.is_self && hex.last_seen && <> · seen {formatAgo(hex.last_seen)}</>}
              </div>
            </div>
            {hex.status === "active" ? (
              <Badge variant={hex.role === "Overseer" ? "default" : "secondary"} title={`preference: ${hex.preference}`}>
                {hex.role}
              </Badge>
            ) : (
              // A lost hex's role is only what it was when last seen; another hex may hold it now.
              <Badge variant="outline" className="text-muted-foreground" title={`Last seen as ${hex.role}`}>
                {hex.status}
              </Badge>
            )}
          </div>
        ))}
        {lattice.hexes.length === 1 ? (
          <p className="text-muted-foreground pt-1 text-xs">
            No peers discovered. Use Add a hex for the settings a new hex needs, or add seed addresses to network.peers in hexdb.toml.
          </p>
        ) : (
          <p className="text-muted-foreground pt-1 text-xs">
            The Overseer takes writes; Harvesters and Replicants keep full copies and serve reads. Replicants never become Overseer.
          </p>
        )}
      </CardContent>
    </Card>
  )
}

function replicationLabel(hex: Status["network"]["lattice"]["hexes"][number]): string {
  switch (hex.replication_state) {
    case "leading":
      return `writes · seq ${formatNumber(hex.applied_seq)}`
    case "streaming":
      return hex.lag === null || hex.lag === undefined ? "replicating" : hex.lag === 0 ? "in sync" : `${formatNumber(hex.lag)} behind`
    case "syncing":
      return "full sync…"
    case "waiting":
      return "waiting for an Overseer"
    case "error":
      return "replication error"
    default:
      return `seq ${formatNumber(hex.last_seq)}`
  }
}

/** Shown on replicas: this hex is read-only and follows the Overseer. */
function ReplicaBanner({ status }: { status: Status }) {
  const r = status.replication
  if (!r || r.state === "leading") return null
  const overseer = status.network.lattice.hexes.find((h) => h.role === "Overseer" && h.status === "active")
  return (
    <div className="bg-muted/50 flex flex-wrap items-center gap-x-3 gap-y-1 rounded-lg border px-4 py-3 text-sm">
      <Badge variant="secondary">{status.hex_type}</Badge>
      <span>
        This hex is a read-only replica
        {overseer ? (
          <>
            {" "}
            of <span className="font-medium">{overseer.name}</span>. Send writes to the Overseer at{" "}
            <span className="font-mono">{overseer.api_endpoint}</span>.
          </>
        ) : (
          <>. No Overseer is reachable right now.</>
        )}
      </span>
      <span className="text-muted-foreground ml-auto text-xs">
        {r.state === "streaming" && (r.lag ? `${formatNumber(r.lag)} changes behind` : "in sync")}
        {r.state === "syncing" && "full sync in progress"}
        {r.state === "waiting" && "waiting for an Overseer"}
        {r.state === "error" && (r.last_error ?? "replication error")}
      </span>
    </div>
  )
}

function formatAgo(iso: string): string {
  const seconds = Math.max(0, Math.round((Date.now() - new Date(iso).getTime()) / 1000))
  return seconds < 60 ? `${seconds}s ago` : formatDuration(seconds) + " ago"
}

// ---------------------------------------------------------------------------
// Tessellation table
// ---------------------------------------------------------------------------

function TessellationTable({ status }: { status: Status }) {
  const rows = [...status.metrics.tessellations].sort(
    (a, b) => (a.kind === b.kind ? b.document_count - a.document_count : a.kind === "user" ? -1 : 1),
  )
  return (
    <Card className="gap-0 overflow-hidden pb-0">
      <CardHeader className="pb-4">
        <CardTitle>Tessellations</CardTitle>
        <CardDescription>Sizes are uncompressed in memory and compressed on disk.</CardDescription>
      </CardHeader>
      <Table>
        <TableHeader className="bg-muted/60">
          <TableRow>
            <TableHead className="pl-6">Name</TableHead>
            <TableHead>Kind</TableHead>
            <TableHead className="text-right">Documents</TableHead>
            <TableHead className="text-right">In memory</TableHead>
            <TableHead className="text-right">Disk only</TableHead>
            <TableHead className="text-right">Avg size</TableHead>
            <TableHead className="pr-6 text-right">Total size</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.map((t) => (
            <TableRow key={t.name} className={t.kind === "user" ? undefined : "bg-muted/15 text-muted-foreground hover:bg-muted/25"}>
              <TableCell className="pl-6 font-medium">
                <span className="flex items-center gap-2.5">
                  <span
                    className={cn(
                      "flex size-6 shrink-0 items-center justify-center rounded-md",
                      t.kind === "user" ? "bg-primary/10 text-primary" : "bg-muted text-muted-foreground",
                    )}
                    aria-hidden
                  >
                    {t.kind === "user" ? <IconHexagon className="size-3.5" /> : <IconLock className="size-3" />}
                  </span>
                  {t.kind === "user" ? (
                    <a className="hover:underline" href={href(`/documents?tessellation=${encodeURIComponent(t.name)}`)} onClick={linkHandler(`/documents?tessellation=${encodeURIComponent(t.name)}`)}>
                      {t.name}
                    </a>
                  ) : (
                    <span className="font-mono text-sm font-normal" title="Managed by HexDB">
                      {t.name}
                    </span>
                  )}
                </span>
              </TableCell>
              <TableCell>
                <Badge variant={t.kind === "user" ? "outline" : "secondary"} className={t.kind === "user" ? undefined : "text-[10px] tracking-wide uppercase"}>
                  {t.kind}
                </Badge>
              </TableCell>
              <TableCell className="text-right tabular-nums">{formatNumber(t.document_count)}</TableCell>
              <TableCell className="text-right tabular-nums">{formatNumber(t.documents_in_ram)}</TableCell>
              <TableCell className="text-right tabular-nums">{formatNumber(t.documents_on_disk)}</TableCell>
              <TableCell className="text-right tabular-nums">{formatBytes(t.avg_document_size_bytes)}</TableCell>
              <TableCell className="pr-6 text-right tabular-nums">{formatBytes(t.total_size_bytes)}</TableCell>
            </TableRow>
          ))}
          {rows.length === 0 && (
            <TableRow>
              <TableCell colSpan={7} className="text-muted-foreground py-8 text-center">
                No tessellations yet.
              </TableCell>
            </TableRow>
          )}
        </TableBody>
      </Table>
    </Card>
  )
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

export function OverviewPage() {
  const [minutes, setMinutes] = useState("60")
  const status = usePoll(api.status, 5_000)
  const history = usePoll(() => api.history(Number(minutes)), 15_000, [minutes])
  const [flushing, setFlushing] = useState(false)
  const [backingUp, setBackingUp] = useState(false)
  const { me } = useAuth()
  const maintenance = has(me, "maintenance")

  const backup = async () => {
    setBackingUp(true)
    try {
      const result = await api.backup()
      toast.success(`Backed up ${formatNumber(result.files)} files (${formatBytes(result.bytes)}) to ${result.path}.`)
    } catch (e) {
      toast.error(`Backup failed: ${errorMessage(e)}`)
    } finally {
      setBackingUp(false)
    }
  }

  const flush = async () => {
    setFlushing(true)
    try {
      const result = await api.flush()
      toast.success(result.entries ? `Flushed ${formatNumber(result.entries)} entries to SSTables.` : "Nothing to flush.")
      void status.refresh()
    } catch (e) {
      toast.error(`Flush failed: ${errorMessage(e)}`)
    } finally {
      setFlushing(false)
    }
  }

  if (!status.data) {
    return (
      <div className="text-muted-foreground flex flex-1 items-center justify-center gap-2 p-8 text-sm">
        {status.error ? (
          <>
            <IconAlertTriangle className="text-destructive size-4" /> {status.error.message}
          </>
        ) : (
          <>
            <IconLoader2 className="size-4 animate-spin" /> Loading status…
          </>
        )}
      </div>
    )
  }

  const samples = history.data?.samples ?? []
  return (
    <div className="@container/main flex flex-1 flex-col gap-4 py-4 md:gap-6 md:py-6">
      <div className="flex items-center justify-end gap-2 px-4 lg:px-6">
        {status.error && (
          <span className="text-destructive mr-auto flex items-center gap-1.5 text-sm">
            <IconAlertTriangle className="size-4" /> Connection lost; showing the last update.
          </span>
        )}
        <Button variant="outline" size="sm" onClick={() => void status.refresh()}>
          <IconRefresh /> Refresh
        </Button>
        {maintenance && (
          <>
            <Button variant="outline" size="sm" onClick={flush} disabled={flushing}>
              {flushing ? <IconLoader2 className="animate-spin" /> : <IconDeviceFloppy />} Flush to disk
            </Button>
            <Button variant="outline" size="sm" onClick={backup} disabled={backingUp} title="Write a consistent backup to storage.backup_path">
              {backingUp ? <IconLoader2 className="animate-spin" /> : <IconArchive />} Back up
            </Button>
          </>
        )}
      </div>
      {status.data.replication && status.data.replication.state !== "leading" && (
        <div className="px-4 lg:px-6">
          <ReplicaBanner status={status.data} />
        </div>
      )}
      <StatCards status={status.data} samples={samples} />
      <div className="px-4 lg:px-6">
        <ActivityChart samples={samples} minutes={minutes} onMinutesChange={setMinutes} />
      </div>
      <div className="grid gap-4 px-4 lg:grid-cols-2 lg:px-6">
        <VertexCard vertices={status.data.vertices} />
        <LatticeCard status={status.data} />
      </div>
      <div className="px-4 lg:px-6">
        <TessellationTable status={status.data} />
      </div>
    </div>
  )
}
