import { useMemo, useState } from "react"
import { IconAlertTriangle, IconCircleCheck, IconHistory, IconKey, IconLoader2, IconPlugConnected, IconRestore } from "@tabler/icons-react"
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
import { useAuth } from "@/lib/auth"
import { api, errorMessage, type AiTestResult, type ConfigDef, type ConfigVersionSummary, type LatticeConfig, type SecretInfo } from "@/lib/api"
import { formatNumber } from "@/lib/format"
import { cn } from "@/lib/utils"

const PROVIDER_LABELS: Record<string, string> = {
  none: "None (AI features off)",
  anthropic: "Anthropic (Claude)",
  openai: "OpenAI",
  github: "GitHub Models",
  azure_openai: "Azure OpenAI",
  openai_compatible: "OpenAI-compatible (Ollama, LM Studio, vLLM...)",
}

/** What tagging can send, with what each covers. */
const SEND_LABELS: Record<string, [string, string]> = {
  metadata: ["Metadata", "Name, folder, type, size and the metadata read from the file"],
  text: ["Text", "Text read from the file: Word, PowerPoint, Excel, OpenDocument, plain text"],
  thumbnail: ["Thumbnail", "The file's small preview image"],
  image: ["Full image", "JPEG, PNG, GIF and WebP images up to 4.5 MB, whole"],
  video: ["Video frames", "Frames spread through each H.264 video (how many: ai.tagging.video_frames)"],
  pdf: ["Whole PDF", "PDFs up to 20 MB and 100 pages, read page by page; Anthropic and OpenAI only"],
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

type Draft = Record<string, unknown>

function SettingInput({
  def,
  value,
  draft,
  onChange,
  readOnly,
}: {
  def: ConfigDef
  value: unknown
  draft: unknown
  onChange: (v: unknown) => void
  readOnly: boolean
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
        {!readOnly && (
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
        )}
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
    const labels: Record<string, string> = def.key === "ai.provider" ? PROVIDER_LABELS : {}
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
          <label key={c} className="flex items-center gap-1.5 text-sm" title={def.key === "ai.tagging.send" ? SEND_LABELS[c]?.[1] : undefined}>
            <Checkbox
              checked={selected.includes(c)}
              onCheckedChange={(v) => onChange(v === true ? kind.choices.filter((x) => x === c || selected.includes(x)) : selected.filter((x) => x !== c))}
              aria-label={`${def.key} ${c}`}
            />
            {def.key === "ai.tagging.send" ? (SEND_LABELS[c]?.[0] ?? c) : c}
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
      placeholder={kind.type === "url" ? "https://…" : kind.type === "origins" ? "https://app.example.com, …" : ""}
      value={String(current ?? "")}
      onChange={(e) => onChange(e.target.value)}
      aria-label={def.key}
    />
  )
}

/** The AI provider in use, a test call, and this month's token usage. */
function AiCard({ refreshKey, readOnly }: { refreshKey: number; readOnly: boolean }) {
  const status = usePoll(api.aiStatus, 15000, [refreshKey])
  const [testing, setTesting] = useState(false)
  const [result, setResult] = useState<AiTestResult | null>(null)
  const data = status.data
  const usage = data?.usage
  const test = async () => {
    setTesting(true)
    setResult(null)
    try {
      setResult(await api.aiTest())
      void status.refresh()
    } catch (e) {
      setResult({ ok: false, error: errorMessage(e) })
    } finally {
      setTesting(false)
    }
  }
  return (
    <Card className="gap-2">
      <CardHeader className="flex flex-row items-start justify-between gap-2">
        <div>
          <CardTitle>AI usage</CardTitle>
          <CardDescription>
            {data?.ai.configured ? `${data.ai.provider} · ${data.ai.model}` : (data?.ai.problem ?? "No AI provider configured.")}
          </CardDescription>
        </div>
        {!readOnly && (
          <Button size="sm" variant="outline" disabled={testing || !data?.ai.configured} onClick={() => void test()}>
            {testing ? <IconLoader2 className="animate-spin" /> : <IconPlugConnected />} Test
          </Button>
        )}
      </CardHeader>
      <CardContent className="grid gap-2 text-sm">
        {result &&
          (result.ok ? (
            <p className="text-status-good">
              Works: answered "{result.reply}" in {result.millis} ms ({result.input_tokens} input, {result.output_tokens} output tokens).
            </p>
          ) : (
            <p className="text-destructive">Failed: {result.error}</p>
          ))}
        {usage && (
          <>
            <p>
              {usage.month}: <span className="font-semibold tabular-nums">{formatNumber(usage.total_tokens)}</span> tokens (
              {formatNumber(usage.input_tokens)} input, {formatNumber(usage.output_tokens)} output) in {usage.calls} call
              {usage.calls === 1 ? "" : "s"}
              {usage.failures > 0 ? `, ${usage.failures} failed` : ""}.{" "}
              {usage.budget > 0 ? `Budget ${formatNumber(usage.budget)}, ${formatNumber(usage.remaining ?? 0)} left.` : "No monthly budget."}
            </p>
            {Object.keys(usage.features).length > 0 && (
              <ul className="text-muted-foreground text-xs">
                {Object.entries(usage.features).map(([name, f]) => (
                  <li key={name}>
                    <span className="font-mono">{name}</span>: {formatNumber((f.input_tokens ?? 0) + (f.output_tokens ?? 0))} tokens, {f.calls ?? 0} call
                    {f.calls === 1 ? "" : "s"}
                  </li>
                ))}
              </ul>
            )}
          </>
        )}
      </CardContent>
    </Card>
  )
}

/** What tagging and video thumbnails would cost in tokens, and buttons to process existing files. */
function AiEstimateCard({ refreshKey, configured, readOnly }: { refreshKey: number; configured: boolean; readOnly: boolean }) {
  const estimate = usePoll(api.aiEstimate, 0, [refreshKey])
  const [confirm, setConfirm] = useState<{ tess: string; kind: "tag" | "thumbnails"; files: number; tokens: number } | null>(null)
  const rows = estimate.data?.tessellations ?? []
  if (rows.length === 0) return null
  return (
    <Card className="gap-2">
      <CardHeader>
        <CardTitle>AI token estimates</CardTitle>
        <CardDescription>
          Rough figures from file sizes, metadata and image dimensions (providers count tokens differently). Turning a feature on only affects files
          uploaded afterwards; existing files are processed only when an administrator asks{readOnly ? "." : ", with the buttons below."}
        </CardDescription>
      </CardHeader>
      <CardContent>
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Tessellation</TableHead>
              <TableHead>Tagging</TableHead>
              <TableHead>Video thumbnails</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((r) => (
              <TableRow key={r.tessellation}>
                <TableCell className="font-medium">{r.tessellation}</TableCell>
                <TableCell className="text-xs">
                  about {formatNumber(r.tagging.tokens_per_file)} tokens per file
                  {r.tagging.untagged > 0 && !readOnly && (
                    <div className="mt-1 flex items-center gap-2">
                      {formatNumber(r.tagging.untagged)} untagged: about {formatNumber(r.tagging.tokens_for_untagged)}
                      <Button
                        size="sm"
                        variant="outline"
                        className="h-6 text-xs"
                        disabled={!configured}
                        onClick={() => setConfirm({ tess: r.tessellation, kind: "tag", files: r.tagging.untagged, tokens: r.tagging.tokens_for_untagged })}
                      >
                        Tag them
                      </Button>
                    </div>
                  )}
                </TableCell>
                <TableCell className="text-xs">
                  {r.video_thumbnails.videos === 0 ? (
                    <span className="text-muted-foreground">no videos</span>
                  ) : (
                    <>
                      about {formatNumber(r.video_thumbnails.tokens_per_video)} tokens per video
                      {r.video_thumbnails.without_ai_thumbnail > 0 && !readOnly && (
                        <div className="mt-1 flex items-center gap-2">
                          {r.video_thumbnails.without_ai_thumbnail} without: about {formatNumber(r.video_thumbnails.tokens_for_those)}
                          <Button
                            size="sm"
                            variant="outline"
                            className="h-6 text-xs"
                            disabled={!configured}
                            onClick={() =>
                              setConfirm({ tess: r.tessellation, kind: "thumbnails", files: r.video_thumbnails.without_ai_thumbnail, tokens: r.video_thumbnails.tokens_for_those })
                            }
                          >
                            Choose them
                          </Button>
                        </div>
                      )}
                    </>
                  )}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </CardContent>
      <ConfirmDialog
        open={confirm !== null}
        onOpenChange={(open) => !open && setConfirm(null)}
        title={confirm?.kind === "tag" ? `Tag ${confirm?.files} file(s) in ${confirm?.tess}?` : `Choose thumbnails for ${confirm?.files} video(s) in ${confirm?.tess}?`}
        description={`This sends them to the AI provider and uses about ${formatNumber(confirm?.tokens ?? 0)} tokens of this lattice's budget. It runs in the background, one file at a time, and stops if the monthly budget runs out.`}
        confirmLabel="Start"
        onConfirm={async () => {
          if (!confirm) return
          const result = confirm.kind === "tag" ? await api.aiTagFiles(confirm.tess) : await api.aiPickThumbnails(confirm.tess)
          toast.success(`Queued ${result.queued} file(s).`)
        }}
      />
    </Card>
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

function HistoryCard({ refreshKey, onRolledBack, readOnly }: { refreshKey: number; onRolledBack: () => void; readOnly: boolean }) {
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
                    {v.valid && v.version !== newest && !readOnly && (
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

/** The lattice configuration: AI settings shared by every hex. */
export function ConfigurationPage() {
  const { me } = useAuth()
  // Demo mode lets non-administrators look; only administrators change anything.
  const readOnly = !me?.is_admin
  const config = usePoll(api.config, 5000)
  const [draft, setDraft] = useState<Draft>({})
  const [comment, setComment] = useState("")
  const [busy, setBusy] = useState(false)
  const [historyKey, setHistoryKey] = useState(0)

  const data = config.data
  const values: Record<string, unknown> = useMemo(() => data?.lattice ?? {}, [data])

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

  const section = (title: string, description: string, prefix: string) => {
    const defs = (data?.schema ?? []).filter((d) => d.key.startsWith(prefix))
    if (defs.length === 0) return null
    return (
      <Card className="gap-2">
        <CardHeader>
          <CardTitle>{title}</CardTitle>
          <CardDescription>{description}</CardDescription>
        </CardHeader>
        <CardContent className="divide-y">
          {defs.map((def) => (
            <div key={def.key} className={cn("grid gap-2 py-3", def.kind.type !== "set" && "sm:grid-cols-[1fr_auto] sm:items-center")}>
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-1.5 text-sm font-medium">
                  {label(def.key)}
                  {values[def.key] !== undefined && !def.secret && !readOnly && (
                    <Button variant="link" size="sm" className="text-muted-foreground h-5 px-1 text-xs" onClick={() => void save({ [def.key]: null })} disabled={busy}>
                      reset to default
                    </Button>
                  )}
                </div>
                <p className="text-muted-foreground text-xs">
                  {def.description} <span className="font-mono">{def.key}</span>
                </p>
              </div>
              <div className={cn(def.kind.type !== "set" && "sm:justify-self-end")}>
                <SettingInput def={def} value={values[def.key]} draft={draft[def.key]} onChange={(v) => setDraft((d) => ({ ...d, [def.key]: v }))} readOnly={readOnly} />
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
      {readOnly && (
        <div className="border-primary/40 bg-primary/10 flex items-center gap-2 rounded-md border px-3 py-2 text-sm">
          <IconAlertTriangle className="text-primary size-4" /> Demo mode: you can look around, but only an administrator can change the
          configuration. Secret values are never shown to anyone.
        </div>
      )}
      <div className="flex flex-wrap items-center gap-2">
        {!readOnly && (
          <>
        <Input className="h-9 w-72" placeholder="Why (saved with the version)" value={comment} onChange={(e) => setComment(e.target.value)} maxLength={500} />
        <Button disabled={busy || count === 0} onClick={() => void save(changes)}>
          {busy && <IconLoader2 className="animate-spin" />}
          Save {count > 0 ? `${count} change${count === 1 ? "" : "s"}` : "changes"}
        </Button>
        <Button variant="ghost" disabled={Object.keys(draft).length === 0} onClick={() => setDraft({})}>
          Discard
        </Button>
          </>
        )}
      </div>
      <fieldset disabled={readOnly} className="grid min-w-0 gap-4">
        {section("AI", "AI features use this lattice's own provider and key, never anyone else's. Everything is off until you turn it on.", "ai.")}
        {section("Quotas", "How much each user may upload. A user's own quota (Users page) comes first; 0 means no limit.", "quotas.")}
        {section("Sharing", "Links that give people without an account a file or a folder (users with the share permission make them on the Files page).", "sharing.")}
        {section("A2A", "Other AI agents can ask this lattice questions over the Agent2Agent protocol, answered by the chat as the calling user.", "a2a.")}
        {section("Cross-origin access", "Web apps on other sites that may call the API from a browser, with an API key or bearer token of their own.", "cors.")}
      </fieldset>
      <AiCard refreshKey={historyKey} readOnly={readOnly} />
      <AiEstimateCard refreshKey={historyKey} configured={!!data && data.lattice["ai.provider"] !== undefined && data.lattice["ai.provider"] !== "none"} readOnly={readOnly} />
      {data && <HexesCard config={data} />}
      <HistoryCard
        readOnly={readOnly}
        refreshKey={historyKey}
        onRolledBack={() => {
          void config.refresh()
          setHistoryKey((k) => k + 1)
        }}
      />
    </div>
  )
}
