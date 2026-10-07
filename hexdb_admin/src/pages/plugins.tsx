import { Badge } from "@/components/ui/badge"
import { Card } from "@/components/ui/card"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { usePoll } from "@/hooks/use-poll"
import { api, type PluginStatus } from "@/lib/api"
import { formatNumber } from "@/lib/format"
import { cn } from "@/lib/utils"

const STATE: Record<string, { label: string; dot: string; hint: string }> = {
  running: { label: "Running", dot: "bg-status-good", hint: "Receiving changes." },
  standby: { label: "Standby", dot: "bg-muted-foreground", hint: "Plugins run on the Overseer; this hex is a replica." },
  disabled: { label: "Disabled", dot: "bg-muted-foreground", hint: "Set \"enabled\": true in the registry to run it." },
  invalid: { label: "Invalid", dot: "bg-destructive", hint: "The manifest couldn't be loaded." },
  error: { label: "Restarting", dot: "bg-status-warning", hint: "It stopped and is restarted every 5 seconds." },
}

/** Plugins from the registry and their delivery state. */
export function PluginsPage() {
  const plugins = usePoll(api.plugins, 5_000)
  const rows = plugins.data?.plugins ?? []

  return (
    <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
      <p className="text-muted-foreground max-w-3xl text-sm">
        Plugins receive every committed change, in order, while this hex is the Overseer. A process plugin reads changes as JSON lines on
        stdin; a webhook plugin receives them as batched POSTs.
        {plugins.data && (
          <>
            {" "}
            Registry: <span className="text-foreground font-mono text-xs">{plugins.data.registry}</span>
            {!plugins.data.enabled && " (plugins are disabled in hexdb.toml)"}.
          </>
        )}
      </p>
      {plugins.error && <p className="text-destructive text-sm">{plugins.error.message}</p>}
      <Card className="gap-0 overflow-hidden py-0">
        <Table>
          <TableHeader className="bg-muted/60">
            <TableRow>
              <TableHead className="pl-6">Plugin</TableHead>
              <TableHead>Runtime</TableHead>
              <TableHead>State</TableHead>
              <TableHead className="text-right">Delivered</TableHead>
              <TableHead className="text-right">Last seq</TableHead>
              <TableHead className="pr-6 text-right">Restarts</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((p) => (
              <PluginRow key={p.id} plugin={p} />
            ))}
            {plugins.data && rows.length === 0 && (
              <TableRow>
                <TableCell colSpan={6} className="text-muted-foreground py-10 text-center">
                  No plugins in the registry.
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </Card>
    </div>
  )
}

function PluginRow({ plugin: p }: { plugin: PluginStatus }) {
  const state = STATE[p.state] ?? { label: p.state, dot: "bg-muted-foreground", hint: "" }
  return (
    <TableRow className="align-top">
      <TableCell className="pl-6">
        <div className="font-medium">
          {p.name} {p.version && <span className="text-muted-foreground text-xs font-normal">v{p.version}</span>}
        </div>
        <div className="text-muted-foreground font-mono text-xs">{p.id}</div>
        {p.description && <div className="text-muted-foreground mt-1 max-w-xl text-xs whitespace-normal">{p.description}</div>}
        {p.last_error && <div className="text-destructive mt-1 max-w-xl text-xs whitespace-normal">{p.last_error}</div>}
      </TableCell>
      <TableCell>{p.runtime ? <Badge variant="outline">{p.runtime}</Badge> : <span className="text-muted-foreground">—</span>}</TableCell>
      <TableCell>
        <span className="flex items-center gap-2" title={state.hint}>
          <span className={cn("size-2 rounded-full", state.dot)} />
          {state.label}
        </span>
        {p.skipped > 0 && <div className="text-muted-foreground text-xs">{formatNumber(p.skipped)} skipped</div>}
      </TableCell>
      <TableCell className="text-right tabular-nums">{formatNumber(p.delivered)}</TableCell>
      <TableCell className="text-right tabular-nums">{p.last_seq ? formatNumber(p.last_seq) : "—"}</TableCell>
      <TableCell className="pr-6 text-right tabular-nums">{formatNumber(p.restarts)}</TableCell>
    </TableRow>
  )
}
