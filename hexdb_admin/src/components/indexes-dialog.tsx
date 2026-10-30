import { useState } from "react"
import { IconBulb, IconKey, IconLoader2, IconSparkles, IconTrash } from "@tabler/icons-react"
import { toast } from "sonner"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { usePoll } from "@/hooks/use-poll"
import { api, errorMessage, type Advice, type IndexKind, type IndexSuggestion } from "@/lib/api"
import { formatNumber } from "@/lib/format"

/** List, create, and drop a tessellation's secondary indexes. */
export function IndexesDialog({ tessellation, onOpenChange, onChanged }: { tessellation: string | null; onOpenChange: (open: boolean) => void; onChanged: () => void }) {
  return (
    <Dialog open={tessellation !== null} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[94svh] overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>Indexes on '{tessellation}'</DialogTitle>
          <DialogDescription>
            Indexes speed up filtered queries, counts, and aggregations. Results are the same with or without them. Every document is already
            indexed by its <code>id</code>.
          </DialogDescription>
        </DialogHeader>
        {tessellation && <IndexManager tessellation={tessellation} onChanged={onChanged} />}
        {tessellation && <IndexAdvice tessellation={tessellation} onChanged={onChanged} />}
      </DialogContent>
    </Dialog>
  )
}

function IndexManager({ tessellation, onChanged }: { tessellation: string; onChanged: () => void }) {
  const indexes = usePoll(() => api.indexes(tessellation), 0, [tessellation])
  const [fields, setFields] = useState("")
  const [name, setName] = useState("")
  const [kind, setKind] = useState<IndexKind>("field")
  const [source, setSource] = useState("")
  const [dimensions, setDimensions] = useState("")
  const [metric, setMetric] = useState<"cosine" | "dot">("cosine")
  const [compact, setCompact] = useState(false)
  const [unique, setUnique] = useState(false)
  const [analyzer, setAnalyzer] = useState("standard")
  const analyzers = usePoll(api.analyzers, 0)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()
  const [dropping, setDropping] = useState<string>()

  const fieldList = fields
    .split(",")
    .map((f) => f.trim())
    .filter(Boolean)

  const create = async (event: React.FormEvent) => {
    event.preventDefault()
    if (fieldList.length === 0) return
    setBusy(true)
    setError(undefined)
    try {
      const sourceList = source
        .split(",")
        .map((f) => f.trim())
        .filter(Boolean)
      const vector =
        kind === "vector" ? { metric, source: sourceList, ...(dimensions.trim() ? { dimensions: Number(dimensions) } : {}), ...(compact ? { quantization: "int8" as const } : {}) } : {}
      const created = await api.createIndex(tessellation, { fields: fieldList, name: name.trim() || undefined, kind, unique: kind === "field" && unique, ...(kind === "text" ? { analyzer } : {}), ...vector })
      toast.success(`Created index '${created.name}' over ${formatNumber(created.documents)} documents.`)
      setFields("")
      setName("")
      setUnique(false)
      setSource("")
      setDimensions("")
      await indexes.refresh()
      onChanged()
    } catch (e) {
      setError(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  const drop = async (index: string) => {
    setDropping(index)
    try {
      await api.dropIndex(tessellation, index)
      toast.success(`Dropped index '${index}'.`)
      await indexes.refresh()
      onChanged()
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setDropping(undefined)
    }
  }

  return (
    <div className="grid gap-5">
      <div className="divide-y rounded-md border">
        {indexes.error && <p className="text-destructive p-3 text-sm">{indexes.error.message}</p>}
        {indexes.data?.length === 0 && <p className="text-muted-foreground p-4 text-sm">No secondary indexes. Queries scan every document.</p>}
        {indexes.data?.map((index) => (
          <div key={index.name} className="flex items-center gap-3 px-3 py-2.5">
            <IconKey className="text-muted-foreground size-4 shrink-0" />
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-1.5 text-sm font-medium">
                {index.name}
                <Badge variant="outline">{index.kind}</Badge>
                {index.unique && <Badge variant="secondary">unique</Badge>}
                {index.kind === "text" && index.analyzer && <Badge variant="secondary">{index.analyzer}</Badge>}
                {index.kind === "vector" && <Badge variant="secondary">{index.metric ?? "cosine"}</Badge>}
                {index.kind === "vector" && index.quantization === "int8" && <Badge variant="secondary">int8</Badge>}
                {index.kind === "vector" && (index.source?.length ?? 0) > 0 && <Badge variant="secondary">embeds {index.source?.join(", ")}</Badge>}
                {!index.ready && <Badge variant="secondary">building</Badge>}
              </div>
              <div className="text-muted-foreground truncate font-mono text-xs">
                {index.fields.join(", ")} · {formatNumber(index.documents)} {index.kind === "vector" ? "vectors" : "docs"} · {formatNumber(index.keys)}{" "}
                {index.kind === "text" ? "words" : index.kind === "vector" ? "dimensions" : "keys"}
              </div>
            </div>
            <Button
              variant="ghost"
              size="icon"
              className="text-muted-foreground hover:text-destructive size-8"
              aria-label={`Drop ${index.name}`}
              disabled={dropping === index.name}
              onClick={() => void drop(index.name)}
            >
              {dropping === index.name ? <IconLoader2 className="animate-spin" /> : <IconTrash />}
            </Button>
          </div>
        ))}
      </div>

      <form onSubmit={create} className="grid gap-3">
        <div className="text-sm font-medium">New index</div>
        <div className="grid gap-3 sm:grid-cols-[1fr_9rem]">
          <div className="grid gap-1.5">
            <Label htmlFor="index-fields">Fields</Label>
            <Input
              id="index-fields"
              className="font-mono"
              placeholder={kind === "text" ? "title, body" : kind === "vector" ? "embedding" : "status, author.name"}
              value={fields}
              onChange={(e) => setFields(e.target.value)}
              autoComplete="off"
            />
          </div>
          <div className="grid gap-1.5">
            <Label>Kind</Label>
            <Select value={kind} onValueChange={(v) => v && setKind(v as IndexKind)}>
              <SelectTrigger className="w-full">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="field">Field</SelectItem>
                <SelectItem value="text">Full text</SelectItem>
                <SelectItem value="vector">Vector</SelectItem>
              </SelectContent>
            </Select>
          </div>
        </div>
        <p className="text-muted-foreground text-xs">
          {kind === "field"
            ? "Comma-separated paths. A field index answers equality, $in, ranges, and $startsWith on its first field, and equality on all of its fields."
            : kind === "text"
              ? "Comma-separated string fields. A text index answers $text searches; a tessellation has at most one."
              : "The field holding each document's vector (an array of numbers). A vector index answers similarity searches (near). With source fields, HexDB fills the field with embeddings of their text (ai.embeddings.* on the Configuration page)."}
        </p>
        <div className="flex flex-wrap items-end gap-3">
          <div className="grid min-w-48 flex-1 gap-1.5">
            <Label htmlFor="index-name">Name (optional)</Label>
            <Input
              id="index-name"
              placeholder={(fieldList.length ? fieldList.join("_").replace(/\./g, "_") : kind === "text" ? "title" : kind === "vector" ? "embedding" : "status") + (kind === "text" ? "_text" : kind === "vector" ? "_vector" : "")}
              value={name}
              onChange={(e) => setName(e.target.value)}
              autoComplete="off"
            />
          </div>
          {kind === "vector" && (
            <>
              <div className="grid min-w-48 flex-1 gap-1.5">
                <Label htmlFor="index-source">Embed from (optional)</Label>
                <Input id="index-source" className="font-mono" placeholder="title, body" value={source} onChange={(e) => setSource(e.target.value)} autoComplete="off" />
              </div>
              <div className="grid w-28 gap-1.5">
                <Label htmlFor="index-dimensions">Dimensions</Label>
                <Input id="index-dimensions" type="number" min={1} max={4096} placeholder="auto" value={dimensions} onChange={(e) => setDimensions(e.target.value)} />
              </div>
              <div className="grid w-32 gap-1.5">
                <Label>Metric</Label>
                <Select value={metric} onValueChange={(v) => v && setMetric(v as "cosine" | "dot")}>
                  <SelectTrigger className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="cosine">Cosine</SelectItem>
                    <SelectItem value="dot">Dot product</SelectItem>
                  </SelectContent>
                </Select>
              </div>
              <Label className="flex h-9 items-center gap-2 font-normal" title="One byte per dimension instead of four; results are rescored exactly">
                <Checkbox checked={compact} onCheckedChange={(v) => setCompact(v === true)} /> Compact (int8)
              </Label>
            </>
          )}
          {kind === "text" && (
            <div className="grid w-44 gap-1.5">
              <Label>Analyzer</Label>
              <Select value={analyzer} onValueChange={setAnalyzer}>
                <SelectTrigger className="w-full">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {(analyzers.data ?? [{ name: "standard", description: "", pipeline: "", builtin: true }]).map((a) => (
                    <SelectItem key={a.name} value={a.name} title={`${a.description} (${a.pipeline})`}>
                      {a.name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          )}
          {kind === "field" && (
            <Label className="flex h-9 items-center gap-2 font-normal">
              <Checkbox checked={unique} onCheckedChange={(v) => setUnique(v === true)} /> Unique
            </Label>
          )}
          <Button type="submit" disabled={busy || fieldList.length === 0}>
            {busy && <IconLoader2 className="animate-spin" />}
            Create index
          </Button>
        </div>
        {error && <p className="text-destructive text-sm">{error}</p>}
      </form>
    </div>
  )
}

const IMPACT: Record<IndexSuggestion["impact"], "default" | "secondary" | "outline"> = { high: "default", medium: "secondary", low: "outline" }

/** Index suggestions from the queries this hex has seen, optionally with Claude's. */
function IndexAdvice({ tessellation, onChanged }: { tessellation: string; onChanged: () => void }) {
  const [advice, setAdvice] = useState<Advice>()
  const [loading, setLoading] = useState<"rules" | "ai">()
  const [applying, setApplying] = useState<string>()
  const [error, setError] = useState<string>()

  const load = async (ai: boolean) => {
    setLoading(ai ? "ai" : "rules")
    setError(undefined)
    try {
      setAdvice(await api.advice(tessellation, ai))
    } catch (e) {
      setError(errorMessage(e))
    } finally {
      setLoading(undefined)
    }
  }

  const apply = async (s: IndexSuggestion) => {
    const key = s.index?.name ?? s.name ?? ""
    setApplying(key)
    try {
      if (s.action === "create_index" && s.index) {
        const created = await api.createIndex(tessellation, s.index)
        toast.success(`Created index '${created.name}'.`)
      } else if (s.name) {
        await api.dropIndex(tessellation, s.name)
        toast.success(`Dropped index '${s.name}'.`)
      }
      onChanged()
      await load(false)
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setApplying(undefined)
    }
  }

  const suggestions = [...(advice?.suggestions ?? []), ...(advice?.ai?.suggestions ?? [])]
  return (
    <div className="grid gap-3 border-t pt-4">
      <div className="flex flex-wrap items-center gap-2">
        <div className="mr-auto text-sm font-medium">Suggestions</div>
        <Button variant="outline" size="sm" disabled={!!loading} onClick={() => void load(false)}>
          {loading === "rules" ? <IconLoader2 className="animate-spin" /> : <IconBulb />} From recent queries
        </Button>
        <Button variant="outline" size="sm" disabled={!!loading} onClick={() => void load(true)} title="Sends the query shapes (field names, never values) to Claude">
          {loading === "ai" ? <IconLoader2 className="animate-spin" /> : <IconSparkles />} Ask Claude
        </Button>
      </div>
      {error && <p className="text-destructive text-sm">{error}</p>}
      {advice && (
        <>
          <p className="text-muted-foreground text-xs">
            Based on {formatNumber(advice.queries_observed)} queries since this hex started, over {formatNumber(advice.documents)} documents.
            {advice.ai && !advice.ai.available && " Claude isn't configured: set [ai] in hexdb.toml and its API key."}
            {advice.ai?.error && ` Claude: ${advice.ai.error}`}
          </p>
          {suggestions.length === 0 ? (
            <p className="text-muted-foreground text-sm">No suggestions. The indexes fit the queries seen so far.</p>
          ) : (
            <div className="divide-y rounded-md border">
              {suggestions.map((s, i) => {
                const key = s.index?.name ?? s.name ?? String(i)
                return (
                  <div key={`${s.source}-${key}-${i}`} className="flex items-start gap-3 px-3 py-2.5">
                    <div className="min-w-0 flex-1">
                      <div className="flex flex-wrap items-center gap-1.5 text-sm font-medium">
                        {s.action === "create_index" ? `Create ${s.index?.kind === "text" ? "text " : ""}index on ${s.index?.fields.join(", ")}` : `Drop ${s.name}`}
                        <Badge variant={IMPACT[s.impact]}>{s.impact}</Badge>
                        {s.source === "ai" && <Badge variant="outline">Claude</Badge>}
                      </div>
                      <p className="text-muted-foreground text-xs">{s.reason}</p>
                    </div>
                    <Button size="sm" variant={s.action === "drop_index" ? "outline" : "default"} disabled={applying !== undefined} onClick={() => void apply(s)}>
                      {applying === key && <IconLoader2 className="animate-spin" />}
                      {s.action === "create_index" ? "Create" : "Drop"}
                    </Button>
                  </div>
                )
              })}
            </div>
          )}
        </>
      )}
    </div>
  )
}
