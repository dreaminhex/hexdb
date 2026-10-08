import { useEffect, useState } from "react"
import { IconArrowBackUp, IconCheck, IconLoader2, IconTrash } from "@tabler/icons-react"
import { toast } from "sonner"

import { JsonEditor, parseJson } from "@/components/json-editor"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { usePoll } from "@/hooks/use-poll"
import { api, errorMessage, type SchemaCheck, type SchemaVersion } from "@/lib/api"
import { formatNumber } from "@/lib/format"

const EXAMPLE = `{
  "fields": {
    "email": { "type": "string", "required": true, "max_length": 320 },
    "status": { "type": "string", "enum": ["active", "closed"], "default": "active" },
    "age": { "type": "integer", "min": 0, "nullable": true }
  },
  "additional_fields": true
}`

/** The next version's starting point: the current fields, with an example migration step to edit. */
function draftFrom(current: SchemaVersion | undefined): string {
  if (!current) return EXAMPLE
  return JSON.stringify(
    { fields: current.fields, additional_fields: current.additional_fields, migration: [{ rename: { from: "old_name", to: "new_name" } }] },
    null,
    2,
  )
}

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

function SchemaManager({ tessellation }: { tessellation: string }) {
  const schemas = usePoll(() => api.schemas(tessellation), 3000, [tessellation])
  const [draft, setDraft] = useState<string>()
  const [check, setCheck] = useState<SchemaCheck>()
  const [busy, setBusy] = useState<"check" | "save" | "drop" | "rollback">()
  const [rollbackTo, setRollbackTo] = useState<string>("")
  const [rollbackSteps, setRollbackSteps] = useState("")
  const [error, setError] = useState<string>()

  const versions = schemas.data?.versions ?? []
  const current = versions[versions.length - 1]
  useEffect(() => {
    if (schemas.data && draft === undefined) setDraft(draftFrom(current))
  }, [schemas.data, current, draft])

  const run = async (what: "check" | "save" | "drop" | "rollback") => {
    setBusy(what)
    setError(undefined)
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
        setDraft(EXAMPLE)
      } else {
        const body = parseJson(draft ?? "{}", "The schema")
        if (what === "check") {
          setCheck(await api.checkSchema(tessellation, body))
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
          className="h-72"
        />
        <p className="text-muted-foreground text-xs">
          Field rules: type (string, number, integer, boolean, object, array, any), required, nullable, default, enum, min, max, min_length,
          max_length, description. Migration steps: rename and copy ({"{from, to}"}), remove ("field"), set_default ({"{field, value}"}), convert (
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
      {error && <p className="text-destructive text-sm whitespace-pre-wrap">{error}</p>}
      <div className="flex justify-end gap-2">
        <Button variant="outline" disabled={!!busy || !draft} onClick={() => void run("check")}>
          {busy === "check" ? <IconLoader2 className="animate-spin" /> : <IconCheck />} Check
        </Button>
        <Button disabled={!!busy || !draft} onClick={() => void run("save")}>
          {busy === "save" && <IconLoader2 className="animate-spin" />}
          {current ? `Save version ${current.version + 1}` : "Save schema"}
        </Button>
      </div>
    </div>
  )
}
