import { useEffect, useMemo, useState } from "react"
import { IconAlertTriangle, IconCircleCheck, IconHistory, IconKey, IconLoader2, IconRestore } from "@tabler/icons-react"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/confirm-dialog"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Checkbox } from "@/components/ui/checkbox"
import { Input } from "@/components/ui/input"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { usePoll } from "@/hooks/use-poll"
import { api, errorMessage, type ConfigDef, type ConfigVersionSummary, type LatticeConfig, type SecretInfo } from "@/lib/api"

const LATTICE = "__lattice__"

const BACKEND_LABELS: Record<string, string> = {
  local: "This hex's disks",
  s3: "Amazon S3 or S3-compatible",
  azure: "Azure Blob Storage",
  gdrive: "Google Drive",
  dropbox: "Dropbox",
}

const PROVIDER_LABELS: Record<string, string> = {
  none: "None (AI features off)",
  anthropic: "Anthropic (Claude)",
  openai: "OpenAI",
  github: "GitHub Models",
  azure_openai: "Azure OpenAI",
  openai_compatible: "OpenAI-compatible (Ollama, LM Studio, vLLM...)",
}

function when(ms: number | undefined): string {
  return ms ? new Date(ms).toLocaleString() : "-"
}

function label(key: string): string {
  const parts = key.split(".")
  const words = parts.slice(1).join(" ").replace(/_/g, " ").split(" ")
  const upper: Record<string, string> = { api: "API", url: "URL", id: "ID", ai: "AI" }
  const name = words.map((w) => upper[w] ?? w).join(" ")
  return name.charAt(0).toUpperCase() + name.slice(1)
}

function isSecretInfo(v: unknown): v is SecretInfo {
  return typeof v === "object" && v !== null && (v as SecretInfo).set === true
}

/** Settings shown for the chosen storage backend: the common ones plus that backend's. */
function storageVisible(key: string, backend: string): boolean {
  const parts = key.split(".")
  return parts.length === 2 || parts[1] === backend
}

type Draft = Record<string, unknown>

function SettingInput({
  def,
  value,
  draft,
  onChange,
}: {
  def: ConfigDef
  value: unknown
  draft: unknown
  onChange: (v: unknown) => void
}) {
  const current = draft !== undefined ? draft : value !== undefined ? value : def.default
  const kind = def.kind
  if (kind.type === "secret") {
    const info = isSecretInfo(value) ? value : undefined
    return (
      <div className="grid gap-1.5 sm:justify-items-end">
        {draft === null ? (
          <Badge variant="secondary">will be cleared</Badge>
        ) : info ? (
          <Badge variant="outline" className="gap-1 font-normal" title={`Set by ${info.updated_by} at ${when(info.updated)}`}>
            <IconKey className="size-3" /> set · {info.fingerprint.slice(0, 8)}
          </Badge>
        ) : (
          <Badge variant="outline" className="text-muted-foreground font-normal">not set</Badge>
        )}
        <div className="flex gap-1.5">
          <Input
            type="password"
            autoComplete="new-password"
            className="h-8 w-56"
            placeholder={info ? "Replace…" : "Enter…"}
            value={typeof draft === "string" ? draft : ""}
            onChange={(e) => onChange(e.target.value === "" ? undefined : e.target.value)}
            aria-label={def.key}
          />
          {info && draft !== null && (
            <Button variant="ghost" size="sm" className="h-8" onClick={() => onChange(null)}>
              Clear
            </Button>
          )}
        </div>
      </div>
    )
  }
  if (kind.type === "boolean") {
    return (
      <label className="flex items-center gap-2 text-sm">
        <Checkbox checked={current === true} onCheckedChange={(v) => onChange(v === true)} aria-label={def.key} />
        {current ? "On" : "Off"}
      </label>
    )
  }
  if (kind.type === "choice") {
    const labels = def.key === "file_storage.backend" ? BACKEND_LABELS : def.key === "ai.provider" ? PROVIDER_LABELS : {}
    return (
      <Select value={String(current)} onValueChange={onChange}>
        <SelectTrigger className="h-8 w-64" aria-label={def.key}>
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {kind.choices.map((c) => (
            <SelectItem key={c} value={c}>
              {labels[c] ?? c}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    )
  }
  if (kind.type === "set") {
    const selected = Array.isArray(current) ? (current as string[]) : []
    return (
      <div className="flex flex-wrap gap-3">
        {kind.choices.map((c) => (
          <label key={c} className="flex items-center gap-1.5 text-sm">
            <Checkbox
              checked={selected.includes(c)}
              onCheckedChange={(v) => onChange(v === true ? kind.choices.filter((x) => x === c || selected.includes(x)) : selected.filter((x) => x !== c))}
              aria-label={`${def.key} ${c}`}
            />
            {c}
          </label>
        ))}
      </div>
    )
  }
  if (kind.type === "integer") {
    return (
      <Input
        type="number"
        className="h-8 w-40 text-right tabular-nums"
        min={kind.min}
        max={kind.max}
        value={String(current ?? "")}
        onChange={(e) => onChange(e.target.value === "" ? undefined : Number(e.target.value))}
        aria-label={def.key}
      />
    )
  }
  return (
    <Input
      className="h-8 w-64"
      type={kind.type === "url" ? "url" : "text"}
      placeholder={kind.type === "url" ? "https://…" : ""}
      value={String(current ?? "")}
      onChange={(e) => onChange(e.target.value)}
      aria-label={def.key}
    />
  )
}

function HexesCard({ config }: { config: LatticeConfig }) {
  return (
    <Card className="gap-2">
      <CardHeader>
        <CardTitle>Hexes</CardTitle>
        <CardDescription>Each hex checks a new version's checksum and settings before applying it. A hex that can't keeps its previous version.</CardDescription>
      </CardHeader>
      <CardContent>
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Hex</TableHead>
              <TableHead>Role</TableHead>
              <TableHead>Version</TableHead>
              <TableHead>State</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {config.hexes.map((h) => {
              const current = config.version === 0 || h.config.sha256 === config.sha256
              const lost = h.status !== "active"
              return (
                <TableRow key={h.id}>
                  <TableCell className="font-medium">
                    {h.name} {h.is_self && <span className="text-muted-foreground text-xs">(this hex)</span>}
                  </TableCell>
                  <TableCell>{h.role}</TableCell>
                  <TableCell className="tabular-nums">{h.config.version}</TableCell>
                  <TableCell>
                    {lost ? (
                      <Badge variant="outline">{h.status}</Badge>
                    ) : h.config.state === "rejected" ? (
                      <Badge variant="destructive" title={h.config.error ?? undefined}>
                        rejected version {h.config.rejected_version}
                      </Badge>
                    ) : current ? (
                      <Badge variant="outline" className="text-status-good gap-1">
                        <IconCircleCheck className="size-3" /> {h.config.state}
                      </Badge>
                    ) : (
                      <Badge variant="secondary">behind</Badge>
                    )}
                    {h.config.overridden && h.config.overridden.length > 0 && (
                      <span className="text-muted-foreground ml-2 text-xs">storage overridden</span>
                    )}
                  </TableCell>
                </TableRow>
              )
            })}
          </TableBody>
        </Table>
      </CardContent>
    </Card>
  )
}

function HistoryCard({ refreshKey, onRolledBack }: { refreshKey: number; onRolledBack: () => void }) {
  const history = usePoll(() => api.configHistory(50), 0, [refreshKey])
  const [target, setTarget] = useState<ConfigVersionSummary | null>(null)
  const newest = history.data?.versions[0]?.version
  return (
    <Card className="gap-2">
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <IconHistory className="size-4" /> History
        </CardTitle>
        <CardDescription>Every change is a new version. Rolling back saves an earlier version's settings (and secrets) as a new version.</CardDescription>
      </CardHeader>
      <CardContent>
        {history.error && <p className="text-destructive text-sm">{history.error.message}</p>}
        {history.data?.versions.length === 0 && <p className="text-muted-foreground text-sm">No changes yet; defaults apply.</p>}
        {history.data && history.data.versions.length > 0 && (
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="w-16">Version</TableHead>
                <TableHead>Saved</TableHead>
                <TableHead>Changes</TableHead>
                <TableHead className="w-24" />
              </TableRow>
            </TableHeader>
            <TableBody>
              {history.data.versions.map((v) => (
                <TableRow key={v.version}>
                  <TableCell className="tabular-nums">{v.version}</TableCell>
                  <TableCell className="text-xs">
                    {v.valid ? (
                      <>
                        {when(v.time)} by {v.author}
                        {v.rollback_of ? <span className="text-muted-foreground"> · rollback to {v.rollback_of}</span> : null}
                        {v.comment ? <div className="text-muted-foreground">{v.comment}</div> : null}
                      </>
                    ) : (
                      <span className="text-destructive">Failed verification: {v.problem}</span>
                    )}
                  </TableCell>
                  <TableCell className="font-mono text-xs">
                    {v.diff?.changed.map((k) => <div key={`c${k}`}>~ {k}</div>)}
                    {v.diff?.removed.map((k) => <div key={`r${k}`}>- {k}</div>)}
                  </TableCell>
                  <TableCell>
                    {v.valid && v.version !== newest && (
                      <Button variant="outline" size="sm" onClick={() => setTarget(v)}>
                        <IconRestore /> Roll back
                      </Button>
                    )}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        )}
      </CardContent>
      <ConfirmDialog
        open={target !== null}
        onOpenChange={(open) => !open && setTarget(null)}
        title={`Roll back to version ${target?.version}?`}
        description="Its settings and secrets are saved as a new version and sent to every hex. Later versions stay in the history."
        confirmLabel="Roll back"
        onConfirm={async () => {
          if (!target) return
          const result = await api.rollbackConfig(target.version, `Rolled back to version ${target.version}`)
          toast.success(`Saved as version ${result.version}.`)
          onRolledBack()
        }}
      />
    </Card>
  )
}

/** The lattice configuration: file storage and AI, shared by every hex. */
export function ConfigurationPage() {
  const config = usePoll(api.config, 5000)
  const [scope, setScope] = useState(LATTICE)
  const [draft, setDraft] = useState<Draft>({})
  const [comment, setComment] = useState("")
  const [busy, setBusy] = useState(false)
  const [historyKey, setHistoryKey] = useState(0)

  useEffect(() => setDraft({}), [scope])

  const data = config.data
  const values: Record<string, unknown> = useMemo(
    () => (scope === LATTICE ? (data?.lattice ?? {}) : (data?.overrides[scope] ?? {})),
    [data, scope],
  )
  const hexNames = useMemo(() => {
    const names = new Set<string>([...(data?.hexes.map((h) => h.name) ?? []), ...Object.keys(data?.overrides ?? {})])
    return [...names].sort()
  }, [data])

  const changes = useMemo(() => {
    const out: Record<string, unknown> = {}
    for (const [key, v] of Object.entries(draft)) {
      if (v === undefined) continue
      const def = data?.schema.find((d) => d.key === key)
      if (!def) continue
      if (!def.secret && JSON.stringify(v) === JSON.stringify(values[key] ?? def.default)) continue
      out[key] = v
    }
    return out
  }, [draft, values, data])

  const save = async (payload: Record<string, unknown>) => {
    if (!data) return
    setBusy(true)
    try {
      const result = await api.saveConfig(payload, {
        hex: scope === LATTICE ? undefined : scope,
        comment: comment.trim() || undefined,
        base_version: data.version,
      })
      setDraft({})
      setComment("")
      await config.refresh()
      setHistoryKey((k) => k + 1)
      toast.success(result.changes && result.changes.version !== data.version ? `Saved version ${result.changes.version}. Hexes apply it within seconds.` : "Nothing changed.")
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  const effective = (key: string, def: ConfigDef) => (draft[key] !== undefined ? draft[key] : values[key] !== undefined ? values[key] : def.default)
  const backendDef = data?.schema.find((d) => d.key === "file_storage.backend")
  const backend = backendDef ? String(effective("file_storage.backend", backendDef)) : "local"

  const section = (title: string, description: string, prefix: string) => {
    const defs = (data?.schema ?? []).filter(
      (d) => d.key.startsWith(prefix) && (scope === LATTICE || d.per_hex) && (prefix !== "file_storage." || storageVisible(d.key, backend)),
    )
    if (defs.length === 0) return null
    return (
      <Card className="gap-2">
        <CardHeader>
          <CardTitle>{title}</CardTitle>
          <CardDescription>{description}</CardDescription>
        </CardHeader>
        <CardContent className="divide-y">
          {defs.map((def) => (
            <div key={def.key} className="grid gap-2 py-3 sm:grid-cols-[1fr_auto] sm:items-center">
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-1.5 text-sm font-medium">
                  {label(def.key)}
                  {values[def.key] !== undefined && !def.secret && (
                    <Button variant="link" size="sm" className="text-muted-foreground h-5 px-1 text-xs" onClick={() => void save({ [def.key]: null })} disabled={busy}>
                      reset to default
                    </Button>
                  )}
                </div>
                <p className="text-muted-foreground text-xs">
                  {def.description} <span className="font-mono">{def.key}</span>
                </p>
              </div>
              <div className="sm:justify-self-end">
                <SettingInput def={def} value={values[def.key]} draft={draft[def.key]} onChange={(v) => setDraft((d) => ({ ...d, [def.key]: v }))} />
              </div>
            </div>
          ))}
        </CardContent>
      </Card>
    )
  }

  const count = Object.keys(changes).length
  return (
    <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
      <p className="text-muted-foreground max-w-3xl text-sm">
        Settings for the whole lattice, saved as encrypted, versioned configuration and applied by every hex, including hexes that join later and a new
        Overseer after a failover. Secrets are write-only: they can be replaced or cleared, never shown. Settings for one machine (addresses, data
        directory, keys) stay in hexdb.toml; this hex's runtime limits are on the Settings page.
      </p>
      {config.error && <p className="text-destructive text-sm">{config.error.message}</p>}
      {data && !data.valid && (
        <div className="border-destructive/40 bg-destructive/10 flex items-center gap-2 rounded-md border px-3 py-2 text-sm">
          <IconAlertTriangle className="text-destructive size-4" /> Version {data.version} failed verification ({data.problem}). Roll back to an earlier version.
        </div>
      )}
      {data && (
        <p className="text-sm">
          Version <span className="font-semibold tabular-nums">{data.version}</span>
          {data.version > 0 && (
            <span className="text-muted-foreground">
              {" "}
              · saved by {data.author} at {when(data.time)}
              {data.comment ? ` · ${data.comment}` : ""} · <span className="font-mono text-xs" title={data.sha256}>{data.sha256.slice(0, 12)}</span>
            </span>
          )}
        </p>
      )}
      <div className="flex flex-wrap items-center gap-2">
        <Select value={scope} onValueChange={setScope}>
          <SelectTrigger className="w-64" aria-label="Scope">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={LATTICE}>Whole lattice</SelectItem>
            {hexNames.map((n) => (
              <SelectItem key={n} value={n}>
                Storage override: {n}
                {data?.overrides[n] ? " (set)" : ""}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Input className="h-9 w-72" placeholder="Why (saved with the version)" value={comment} onChange={(e) => setComment(e.target.value)} maxLength={500} />
        <Button disabled={busy || count === 0} onClick={() => void save(changes)}>
          {busy && <IconLoader2 className="animate-spin" />}
          Save {count > 0 ? `${count} change${count === 1 ? "" : "s"}` : "changes"}
        </Button>
        <Button variant="ghost" disabled={Object.keys(draft).length === 0} onClick={() => setDraft({})}>
          Discard
        </Button>
      </div>
      {scope !== LATTICE && (
        <p className="text-muted-foreground max-w-3xl text-sm">
          A storage override replaces the lattice's whole file storage configuration on hex {scope}; other settings always come from the lattice.
          {data?.overrides[scope] && (
            <Button
              variant="link"
              size="sm"
              className="h-5 px-1"
              disabled={busy}
              onClick={() => void save(Object.fromEntries(Object.keys(data.overrides[scope]).map((k) => [k, null])))}
            >
              Remove this override
            </Button>
          )}
        </p>
      )}
      <div className="grid gap-4 xl:grid-cols-2">
        {section("File storage", "Where file content is kept. It is encrypted and split into shards before it leaves the hex, so the provider only sees ciphertext.", "file_storage.")}
        {scope === LATTICE && section("AI", "AI features use this lattice's own provider and key, never anyone else's. Everything is off until you turn it on.", "ai.")}
      </div>
      {data && <HexesCard config={data} />}
      <HistoryCard
        refreshKey={historyKey}
        onRolledBack={() => {
          void config.refresh()
          setHistoryKey((k) => k + 1)
        }}
      />
    </div>
  )
}
