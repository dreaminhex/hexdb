import { useEffect, useState } from "react"
import { type Completion } from "@codemirror/autocomplete"
import { IconArrowBackUp, IconCheck, IconClockPause, IconEdit, IconLoader2, IconPlayerPlay, IconTrash } from "@tabler/icons-react"
import { toast } from "sonner"

import { JsonEditor, parseJson } from "@/components/json-editor"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { usePoll } from "@/hooks/use-poll"
import { api, errorMessage, type InferredSchema, type PendingSchema, type SchemaCheck, type SchemaVersion } from "@/lib/api"
import { formatNumber } from "@/lib/format"
import { cn } from "@/lib/utils"

/** The next version's starting point: exactly the current version, which every document already fits. */
function draftFrom(current: SchemaVersion): string {
  return JSON.stringify({ fields: current.fields, additional_fields: current.additional_fields }, null, 2)
}

/** A first version read from the documents: their fields and types, nothing required. */
function draftFromInferred(inferred: InferredSchema): string {
  // Only what a field rule needs: false and empty settings are the defaults.
  const fields = Object.fromEntries(
    Object.entries(inferred.fields).map(([name, rule]) => [name, Object.fromEntries(Object.entries(rule).filter(([, v]) => v !== false && v !== ""))]),
  )
  return JSON.stringify({ fields, additional_fields: inferred.additional_fields }, null, 2)
}

type Action = "check" | "save" | "later" | "apply" | "discard" | "drop" | "rollback"

/** A saved version, as editor text. */
function draftFromPending(pending: PendingSchema): string {
  return JSON.stringify(pending.schema, null, 2)
}

/** What the schema editor offers while typing inside quotes. */
const SCHEMA_KEYWORDS: Completion[] = [
  ...["fields", "additional_fields", "migration"].map((label) => ({ label, type: "property", detail: "schema" })),
  ...["type", "required", "nullable", "default", "enum", "min", "max", "min_length", "max_length", "description", "tokenize"].map((label) => ({
    label,
    type: "property",
    detail: "field rule",
  })),
  ...["string", "number", "integer", "boolean", "object", "array", "any"].map((label) => ({ label, type: "enum", detail: "type" })),
  ...["one_way", "vault"].map((label) => ({ label, type: "enum", detail: "tokenize" })),
  ...["rename", "copy", "remove", "set_default", "convert", "function"].map((label) => ({ label, type: "keyword", detail: "migration step" })),
  ...["from", "to", "field", "value", "name", "undo"].map((label) => ({ label, type: "property", detail: "step option" })),
]

/** A tessellation's schema versions, a checked editor for the next one, and migration progress. */
export function SchemasDialog({ tessellation, onOpenChange }: { tessellation: string | null; onOpenChange: (open: boolean) => void }) {
  return (
    <Dialog open={tessellation !== null} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[94svh] overflow-y-auto sm:max-w-3xl">
        <DialogHeader>
          <DialogTitle>Schema for '{tessellation}'</DialogTitle>
          <DialogDescription>
            Optional. Writes that don't fit the current version are rejected. A new version may rename, copy, remove, default, or convert fields;
            existing documents are migrated in the background (until then, reads can return either version), and any document is upgraded when it's written.
          </DialogDescription>
        </DialogHeader>
        {tessellation && <SchemaManager tessellation={tessellation} />}
      </DialogContent>
    </Dialog>
  )
}

/**
 * Versions, the saved (pending) version, rollback, migration progress and an editor for the next
 * version, saved to apply now or later. `roomy` gives the editor more height (the Schemas page).
 */
export function SchemaManager({ tessellation, roomy = false }: { tessellation: string; roomy?: boolean }) {
  const schemas = usePoll(() => api.schemas(tessellation), 3000, [tessellation])
  const [draft, setDraft] = useState<string>()
  const [check, setCheck] = useState<SchemaCheck>()
  const [busy, setBusy] = useState<Action>()
  const [rollbackTo, setRollbackTo] = useState<string>("")
  const [rollbackSteps, setRollbackSteps] = useState("")
  const [error, setError] = useState<string>()
  const [inferred, setInferred] = useState<InferredSchema>()
  // Saving when some existing documents wouldn't fit waits for a second click.
  const [unfit, setUnfit] = useState<{ what: "save" | "later"; check: SchemaCheck }>()

  const versions = schemas.data?.versions ?? []
  const current = versions[versions.length - 1]
  const pending = schemas.data?.pending ?? null
  // Start from the saved version if there is one, so editing it carries on where it was left; else
  // from the current version; with no schema, from what the documents already look like.
  const loaded = !!schemas.data
  const savedPending = schemas.data?.pending
  useEffect(() => {
    if (!loaded || draft !== undefined) return
    if (savedPending) setDraft(draftFromPending(savedPending))
    else if (current) setDraft(draftFrom(current))
    else {
      let cancelled = false
      api
        .inferSchema(tessellation)
        .then((i) => {
          if (cancelled) return
          setInferred(i)
          setDraft(draftFromInferred(i))
        })
        .catch((e) => !cancelled && setError(errorMessage(e)))
      return () => {
        cancelled = true
      }
    }
  }, [loaded, savedPending, current, draft, tessellation])

  const run = async (what: Action, anyway = false) => {
    setBusy(what)
    setError(undefined)
    setUnfit(undefined)
    try {
      if (what === "rollback") {
        const steps = rollbackSteps.trim() ? parseJson<unknown[]>(rollbackSteps, "The extra steps") : []
        const version = await api.rollbackSchema(tessellation, Number(rollbackTo), steps)
        toast.success(`Version ${version.version} restores version ${rollbackTo}. Existing documents are being migrated.`)
        setRollbackTo("")
        setRollbackSteps("")
        setDraft(draftFrom(version))
      } else if (what === "drop") {
        await api.dropSchemas(tessellation)
        toast.success(`'${tessellation}' is schemaless again.`)
        setInferred(undefined)
        setDraft(undefined)
      } else if (what === "apply") {
        const version = await api.applyPendingSchema(tessellation)
        toast.success(`Version ${version.version} is current. Existing documents are being migrated.`)
        setDraft(draftFrom(version))
      } else if (what === "discard") {
        await api.discardPendingSchema(tessellation)
        toast.success("The saved version was discarded.")
        setDraft(current ? draftFrom(current) : undefined)
      } else {
        const body = parseJson(draft ?? "{}", "The schema")
        if (what === "check") {
          setCheck(await api.checkSchema(tessellation, body))
          return
        }
        // Check before saving: refuse what can't be registered, and ask before leaving documents behind.
        const checked = await api.checkSchema(tessellation, body)
        setCheck(checked)
        if (!checked.compatible) throw new Error(`Not saved. This version isn't compatible: ${checked.error}`)
        if (checked.would_not_fit && !anyway) {
          setUnfit({ what, check: checked })
          return
        }
        if (what === "later") {
          const saved = await api.savePendingSchema(tessellation, body)
          toast.success(`Version ${saved.pending?.version ?? ""} is saved. Writes follow ${current ? `version ${current.version}` : "no schema"} until you apply it.`)
          setCheck(undefined)
        } else {
          const version = await api.addSchema(tessellation, body)
          toast.success(`Version ${version.version} is current. Existing documents are being migrated.`)
          setCheck(undefined)
          setDraft(draftFrom(version))
        }
      }
      await schemas.refresh()
    } catch (e) {
      setError(errorMessage(e))
    } finally {
      setBusy(undefined)
    }
  }

  const migration = schemas.data?.migration
  return (
    <div className="grid gap-4">
      {schemas.error && <p className="text-destructive text-sm">{schemas.error.message}</p>}
      <div className="flex flex-wrap items-center gap-2 text-sm">
        {current ? (
          <>
            <span>
              Version <span className="font-medium">{current.version}</span> of {versions.length}, since {new Date(current.created).toLocaleString()}
            </span>
            <Badge variant="outline">{Object.keys(current.fields).length} fields</Badge>
            <Badge variant="outline">{current.additional_fields ? "other fields allowed" : "no other fields"}</Badge>
            <Button variant="ghost" size="sm" className="text-muted-foreground hover:text-destructive ml-auto" disabled={!!busy} onClick={() => void run("drop")}>
              <IconTrash /> Remove schema
            </Button>
          </>
        ) : (
          schemas.data && <span className="text-muted-foreground">No schema: any JSON object is accepted.</span>
        )}
      </div>
      {versions.length > 0 && (
        <div className="divide-y rounded-md border text-sm">
          {[...versions].reverse().map((v) => (
            <div key={v.version} className="flex flex-wrap items-center gap-2 px-3 py-1.5">
              <span className="font-medium">Version {v.version}</span>
              {v.restores && <Badge variant="outline">restores {v.restores}</Badge>}
              <span className="text-muted-foreground text-xs">
                {Object.keys(v.fields).length} fields · {new Date(v.created).toLocaleString()}
                {v.created_by_login ? ` · ${v.created_by_login}` : ""}
                {v.migration?.length ? ` · ${v.migration.length} migration step${v.migration.length === 1 ? "" : "s"}` : ""}
              </span>
            </div>
          ))}
        </div>
      )}
      {pending && (
        <div className={cn("grid gap-2 rounded-md border p-3 text-sm", pending.error ? "border-status-warning/50 bg-status-warning/10" : "border-primary/40 bg-primary/5")}>
          <div className="flex flex-wrap items-center gap-2">
            <IconClockPause className="text-primary size-4" aria-hidden />
            <span className="font-medium">{pending.version ? `Version ${pending.version}` : "A version"} is saved, waiting to be applied</span>
            <span className="text-muted-foreground text-xs">
              {new Date(pending.saved).toLocaleString()}
              {pending.saved_by_login ? ` · ${pending.saved_by_login}` : ""} · based on {pending.based_on ? `version ${pending.based_on}` : "no schema"}
            </span>
          </div>
          {pending.error && <p className="text-xs">It no longer fits the current version: {pending.error}. Edit it and save it again, or discard it.</p>}
          <div className="flex flex-wrap gap-2">
            <Button size="sm" disabled={!!busy || !!pending.error} onClick={() => void run("apply")}>
              {busy === "apply" ? <IconLoader2 className="animate-spin" /> : <IconPlayerPlay />} Apply now
            </Button>
            <Button
              size="sm"
              variant="outline"
              disabled={!!busy}
              onClick={() => {
                setDraft(draftFromPending(pending))
                setCheck(undefined)
              }}
            >
              <IconEdit /> Edit
            </Button>
            <Button size="sm" variant="ghost" className="text-muted-foreground hover:text-destructive" disabled={!!busy} onClick={() => void run("discard")}>
              {busy === "discard" ? <IconLoader2 className="animate-spin" /> : <IconTrash />} Discard
            </Button>
          </div>
        </div>
      )}
      {versions.length > 1 && (
        <div className="grid gap-2 rounded-md border p-3">
          <div className="flex flex-wrap items-end gap-2">
            <div className="grid gap-1.5">
              <Label>Roll back to</Label>
              <Select value={rollbackTo} onValueChange={setRollbackTo}>
                <SelectTrigger className="w-40">
                  <SelectValue placeholder="a version" />
                </SelectTrigger>
                <SelectContent>
                  {versions.slice(0, -1).map((v) => (
                    <SelectItem key={v.version} value={String(v.version)}>
                      Version {v.version}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <Button variant="outline" disabled={!!busy || !rollbackTo} onClick={() => void run("rollback")}>
              {busy === "rollback" ? <IconLoader2 className="animate-spin" /> : <IconArrowBackUp />} Roll back
            </Button>
          </div>
          <p className="text-muted-foreground text-xs">
            Registers a new version with that version's fields and the inverse of every migration since: renames reversed, copies removed, conversions
            converted back, function steps run their undo function. Removed fields can't come back by themselves; add steps for them here, e.g.{" "}
            <code>{'[{"set_default": {"field": "name", "value": "unknown"}}]'}</code>.
          </p>
          {rollbackTo && <JsonEditor value={rollbackSteps} onChange={setRollbackSteps} label="Extra rollback steps" className="h-16" />}
        </div>
      )}
      {migration && (
        <div className="bg-muted/40 rounded-md border px-3 py-2 text-sm">
          <div className="flex flex-wrap items-center gap-2">
            Migration to version {migration.version}:
            <Badge variant={migration.state === "failed" ? "destructive" : migration.state === "running" ? "secondary" : "outline"}>{migration.state}</Badge>
            <span className="text-muted-foreground text-xs">
              {formatNumber(migration.migrated)} migrated · {formatNumber(migration.unchanged)} already current · {formatNumber(migration.failed)} don't fit
            </span>
          </div>
          {migration.errors.slice(0, 5).map((e) => (
            <div key={e.id} className="text-muted-foreground mt-1 font-mono text-xs">
              {e.id}: {e.problems.map((p) => `${p.field} ${p.message}`).join("; ")}
            </div>
          ))}
        </div>
      )}
      <div className="grid gap-1.5">
        <Label>{current ? `Version ${current.version + 1}` : "First version"}</Label>
        <JsonEditor
          value={draft ?? ""}
          onChange={(v) => {
            setDraft(v)
            setCheck(undefined)
          }}
          label="Schema"
          className={roomy ? "h-[28rem]" : "h-72"}
          keywords={SCHEMA_KEYWORDS}
        />
        <p className="text-muted-foreground text-xs">
          Field rules: type (string, number, integer, boolean, object, array, any), required, nullable, default, enum, min, max, min_length,
          max_length, description, and tokenize (one_way or vault, string fields only: the value is replaced by a token on write; vault tokens can be
          read back with the detokenize permission). Migration steps: rename and copy ({"{from, to}"}), remove ("field"), set_default ({"{field, value}"}), convert (
          {"{field, to}"}), and function ({"{name, undo}"}: a function that gets batches of documents and returns them migrated). A new version must stay
          compatible: no type change without a convert step, no new required field without a default. With a function step, use Check to run it on
          the existing documents first.
        </p>
      </div>
      {check && (
        <div className={`rounded-md border px-3 py-2 text-sm ${check.compatible && !check.would_not_fit ? "" : "border-status-warning/50 bg-status-warning/10"}`}>
          {!check.compatible ? (
            <span className="text-destructive">Not compatible: {check.error}</span>
          ) : (
            <>
              Compatible as version {check.version}. Checked {formatNumber(check.checked ?? 0)} documents;{" "}
              {check.would_not_fit ? `${formatNumber(check.would_not_fit)} wouldn't fit after migrating.` : "all fit."}
              {check.errors?.slice(0, 5).map((e) => (
                <div key={e.id} className="text-muted-foreground mt-1 font-mono text-xs">
                  {e.id}: {e.problems.map((p) => `${p.field} ${p.message}`).join("; ")}
                </div>
              ))}
            </>
          )}
        </div>
      )}
      {inferred && !current && draft !== undefined && (
        <p className="text-muted-foreground rounded-md border px-3 py-2 text-xs">
          {inferred.sampled === 0 ? (
            <>No documents yet, so there are no fields to start from. Describe the fields documents should have.</>
          ) : (
            <>
              Read from {formatNumber(inferred.sampled)} existing document{inferred.sampled === 1 ? "" : "s"}: each field with its type, nothing
              required, other fields allowed, so it accepts every document there today.
              {(() => {
                const everywhere = Object.entries(inferred.present).filter(([, n]) => n === inferred.sampled).map(([f]) => f)
                return everywhere.length > 0 ? (
                  <>
                    {" "}
                    In every one: <span className="text-foreground font-mono">{everywhere.join(", ")}</span>. Add{" "}
                    <code>"required": true</code> to those every document must have.
                  </>
                ) : null
              })()}
            </>
          )}
        </p>
      )}
      {unfit && (
        <div className="border-status-warning/50 bg-status-warning/10 flex flex-wrap items-center gap-2 rounded-md border px-3 py-2 text-sm">
          <span className="flex-1">
            {formatNumber(unfit.check.would_not_fit ?? 0)} of {formatNumber(unfit.check.checked ?? 0)} existing documents wouldn't fit this version.
            {unfit.what === "save"
              ? " Their migration would fail and they'd stay as they are, and writes like them would be refused."
              : " Saved, it changes nothing yet; applied, they'd fail to migrate and writes like them would be refused."}
          </span>
          <Button size="sm" variant="outline" onClick={() => setUnfit(undefined)}>
            Cancel
          </Button>
          <Button size="sm" variant="destructive" onClick={() => void run(unfit.what, true)}>
            {unfit.what === "save" ? "Save and apply anyway" : "Save anyway"}
          </Button>
        </div>
      )}
      {error && <p className="text-destructive text-sm whitespace-pre-wrap">{error}</p>}
      <div className="flex justify-end gap-2">
        <Button variant="outline" disabled={!!busy || !draft} onClick={() => void run("check")}>
          {busy === "check" ? <IconLoader2 className="animate-spin" /> : <IconCheck />} Check
        </Button>
        <Button variant="outline" disabled={!!busy || !draft} onClick={() => void run("later")}>
          {busy === "later" ? <IconLoader2 className="animate-spin" /> : <IconClockPause />} Save, apply later
        </Button>
        <Button disabled={!!busy || !draft} onClick={() => void run("save")}>
          {busy === "save" && <IconLoader2 className="animate-spin" />}
          {current ? `Save version ${current.version + 1} and apply now` : "Save schema and apply now"}
        </Button>
      </div>
    </div>
  )
}
