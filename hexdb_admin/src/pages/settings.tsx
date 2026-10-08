import { useEffect, useMemo, useState } from "react"
import { IconAlertTriangle, IconBolt, IconLoader2, IconRotate } from "@tabler/icons-react"
import { toast } from "sonner"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Checkbox } from "@/components/ui/checkbox"
import { Input } from "@/components/ui/input"
import { usePoll } from "@/hooks/use-poll"
import { api, errorMessage, type Setting } from "@/lib/api"
import { formatBytes, formatNumber } from "@/lib/format"

const SECTIONS: { prefix: string; title: string; description: string }[] = [
  { prefix: "limits", title: "Limits", description: "Document and request sizes, connections and timeouts." },
  { prefix: "storage", title: "Storage", description: "Disk space, the change history, flushing and compaction." },
  { prefix: "memory", title: "Memory", description: "How much of each hex's data stays in memory." },
  { prefix: "security", title: "Security", description: "Sessions, sign-in throttling and the audit trail." },
  { prefix: "replication", title: "Replication", description: "Synchronous acknowledgements, write forwarding and quorum." },
  { prefix: "compression", title: "Compression", description: "How data on disk is compressed." },
  { prefix: "plugins", title: "Plugins", description: "Whether plugins load at startup." },
]

function label(key: string): string {
  const name = key.split(".").slice(1).join(".").replace(/_/g, " ")
  return name.charAt(0).toUpperCase() + name.slice(1)
}

function SettingRow({ setting, draft, onChange }: { setting: Setting; draft: number | boolean | undefined; onChange: (value: number | boolean | undefined) => void }) {
  const current = draft ?? setting.saved ?? setting.value
  const changed = draft !== undefined && draft !== (setting.saved ?? setting.value)
  return (
    <div className="grid gap-2 py-3 sm:grid-cols-[1fr_14rem] sm:items-center">
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-1.5 text-sm font-medium">
          {label(setting.key)}
          {setting.live ? (
            <Badge variant="outline" className="gap-1 font-normal" title="Takes effect immediately">
              <IconBolt className="size-3" /> live
            </Badge>
          ) : (
            <Badge variant="outline" className="gap-1 font-normal" title="Takes effect at the next restart">
              <IconRotate className="size-3" /> restart
            </Badge>
          )}
          {setting.restart_required && (
            <Badge variant="secondary" className="gap-1 font-normal text-status-warning">
              saved: {String(setting.saved)}, running: {String(setting.value)}
            </Badge>
          )}
        </div>
        <p className="text-muted-foreground text-xs">
          {setting.description} <span className="font-mono">{setting.key}</span>
          {setting.saved !== null && setting.saved !== undefined ? " · changed here" : " · from the config file"}
        </p>
      </div>
      <div className="flex items-center gap-2 sm:justify-end">
        {setting.kind === "boolean" ? (
          <label className="flex items-center gap-2 text-sm">
            <Checkbox checked={current === true} onCheckedChange={(v) => onChange(v === true)} aria-label={setting.key} />
            {current ? "On" : "Off"}
          </label>
        ) : (
          <>
            <Input
              type="number"
              className={`h-8 w-32 text-right tabular-nums ${changed ? "border-primary" : ""}`}
              min={setting.min}
              max={setting.max}
              value={String(current)}
              onChange={(e) => onChange(e.target.value === "" ? undefined : Number(e.target.value))}
              aria-label={setting.key}
            />
            <span className="text-muted-foreground w-10 text-xs">{setting.unit}</span>
          </>
        )}
      </div>
    </div>
  )
}

/** Runtime settings saved in the data directory, and the effective configuration. */
export function SettingsPage() {
  const settings = usePoll(api.settings, 0)
  const [draft, setDraft] = useState<Record<string, number | boolean | undefined>>({})
  const [busy, setBusy] = useState(false)
  const [showConfig, setShowConfig] = useState(false)

  useEffect(() => setDraft({}), [settings.data])

  const changes = useMemo(() => {
    const out: Record<string, number | boolean> = {}
    for (const s of settings.data?.settings ?? []) {
      const d = draft[s.key]
      if (d !== undefined && d !== (s.saved ?? s.value)) out[s.key] = d
    }
    return out
  }, [draft, settings.data])

  const save = async (payload: Record<string, number | boolean | null>) => {
    setBusy(true)
    try {
      const result = await api.saveSettings(payload)
      settings.refresh()
      toast.success(result.restart_required ? "Saved. Some changes take effect at the next restart." : "Saved and applied.")
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  const data = settings.data
  const diskLimit = data?.settings.find((s) => s.key === "storage.disk_mb")?.value as number | undefined

  return (
    <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
      <p className="text-muted-foreground max-w-3xl text-sm">
        These settings override <span className="font-mono">{data?.config_file ?? "hexdb.toml"}</span> for this hex only. They are saved,
        encrypted, in its data directory. Live settings apply at once; the others at the next restart. Everything else is set in the config
        file.
      </p>
      {settings.error && <p className="text-destructive text-sm">{settings.error.message}</p>}
      {data?.restart_required && (
        <div className="border-status-warning/40 bg-status-warning/10 flex items-center gap-2 rounded-md border px-3 py-2 text-sm">
          <IconAlertTriangle className="text-status-warning size-4" /> Some saved settings take effect when this hex restarts.
        </div>
      )}
      {data && diskLimit !== undefined && (
        <p className="text-muted-foreground text-sm">
          Data directory: {formatBytes(data.disk_used_bytes)} of {formatNumber(diskLimit)} MB ({Math.round((data.disk_used_bytes / (diskLimit * 1024 * 1024)) * 100)}%).
        </p>
      )}
      <div className="flex items-center gap-2">
        <Button disabled={busy || Object.keys(changes).length === 0} onClick={() => void save(changes)}>
          {busy && <IconLoader2 className="animate-spin" />}
          Save {Object.keys(changes).length > 0 ? `${Object.keys(changes).length} change${Object.keys(changes).length === 1 ? "" : "s"}` : "changes"}
        </Button>
        <Button variant="ghost" disabled={Object.keys(draft).length === 0} onClick={() => setDraft({})}>
          Discard
        </Button>
      </div>
      <div className="grid gap-4 xl:grid-cols-2">
        {SECTIONS.map((section) => {
          const rows = (data?.settings ?? []).filter((s) => s.key.startsWith(section.prefix + "."))
          if (rows.length === 0) return null
          return (
            <Card key={section.prefix} className="gap-2">
              <CardHeader>
                <CardTitle>{section.title}</CardTitle>
                <CardDescription>{section.description}</CardDescription>
              </CardHeader>
              <CardContent className="divide-y">
                {rows.map((s) => (
                  <div key={s.key}>
                    <SettingRow setting={s} draft={draft[s.key]} onChange={(v) => setDraft((d) => ({ ...d, [s.key]: v }))} />
                    {s.saved !== null && s.saved !== undefined && (
                      <Button variant="link" size="sm" className="text-muted-foreground -mt-2 h-6 px-0 text-xs" onClick={() => void save({ [s.key]: null })}>
                        Use the config file's value ({String(s.default)} by default)
                      </Button>
                    )}
                  </div>
                ))}
              </CardContent>
            </Card>
          )
        })}
      </div>
      <Card className="gap-2">
        <CardHeader className="flex flex-row items-center justify-between">
          <div>
            <CardTitle>Effective configuration</CardTitle>
            <CardDescription>What this hex is running with. Secrets are hidden.</CardDescription>
          </div>
          <Button variant="outline" size="sm" onClick={() => setShowConfig((v) => !v)}>
            {showConfig ? "Hide" : "Show"}
          </Button>
        </CardHeader>
        {showConfig && data && (
          <CardContent>
            <pre className="bg-muted/50 max-h-[32rem] overflow-auto rounded-md p-3 text-xs">{JSON.stringify(data.config, null, 2)}</pre>
          </CardContent>
        )}
      </Card>
    </div>
  )
}
