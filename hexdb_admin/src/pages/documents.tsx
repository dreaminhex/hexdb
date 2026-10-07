import { useEffect, useMemo, useState } from "react"
import { json } from "@codemirror/lang-json"
import { type Extension } from "@codemirror/state"
import {
  IconAlertTriangle,
  IconChevronLeft,
  IconChevronRight,
  IconCopy,
  IconFilter,
  IconLoader2,
  IconPlus,
  IconTrash,
} from "@tabler/icons-react"
import { toast } from "sonner"

import { CodeEditor } from "@/components/code-editor"
import { ConfirmDialog } from "@/components/confirm-dialog"
import { DocumentTable } from "@/components/document-table"
import { columnsFor } from "@/lib/doc-table"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Sheet, SheetContent, SheetDescription, SheetFooter, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { usePoll } from "@/hooks/use-poll"
import { api, type ApiDocument, errorMessage } from "@/lib/api"
import { formatNumber, formatTimestamp } from "@/lib/format"
import { setQueryParam, useQueryParam } from "@/lib/router"

const PAGE_SIZES = ["25", "50", "100"]

// ---------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------

/** The editable part of a document: everything except `id` and `_expires_at`. */
function editableJson(doc: ApiDocument | null): string {
  if (!doc) return "{\n  \n}"
  const fields = Object.fromEntries(Object.entries(doc).filter(([key]) => key !== "id" && key !== "_expires_at"))
  return JSON.stringify(fields, null, 2)
}

function DocumentEditor({
  tessellation,
  doc,
  open,
  onOpenChange,
  onSaved,
}: {
  tessellation: string
  /** null = new document. */
  doc: ApiDocument | null
  open: boolean
  onOpenChange: (open: boolean) => void
  onSaved: () => void
}) {
  const [text, setText] = useState("")
  const [ttl, setTtl] = useState("")
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [deleting, setDeleting] = useState(false)
  const extensions = useMemo<Extension[]>(() => [json()], [])

  useEffect(() => {
    if (open) {
      setText(editableJson(doc))
      setTtl("")
      setError(null)
    }
  }, [open, doc])

  const save = async () => {
    let data: unknown
    try {
      data = JSON.parse(text)
    } catch (e) {
      setError(`Not valid JSON: ${errorMessage(e)}`)
      return
    }
    if (data === null || typeof data !== "object" || Array.isArray(data)) {
      setError("A document must be a JSON object.")
      return
    }
    const seconds = ttl.trim() ? Number(ttl) : undefined
    if (seconds !== undefined && (!Number.isInteger(seconds) || seconds <= 0)) {
      setError("TTL must be a whole number of seconds.")
      return
    }

    setBusy(true)
    setError(null)
    try {
      if (doc) {
        await api.replaceDocument(tessellation, doc.id, data, seconds)
        toast.success("Document saved.")
      } else {
        const created = await api.insertDocument(tessellation, data, seconds)
        toast.success(`Created document ${created.id}.`)
      }
      onSaved()
      onOpenChange(false)
    } catch (e) {
      setError(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <>
      <Sheet open={open} onOpenChange={(next) => !busy && onOpenChange(next)}>
        <SheetContent className="flex w-full flex-col gap-0 sm:max-w-xl">
          <SheetHeader className="border-b">
            <SheetTitle>{doc ? "Edit document" : "New document"}</SheetTitle>
            <SheetDescription asChild>
              <div className="space-y-1">
                {doc ? (
                  <div className="flex items-center gap-1">
                    <span className="font-mono text-xs">{doc.id}</span>
                    <Button
                      variant="ghost"
                      size="icon"
                      className="size-6"
                      aria-label="Copy document ID"
                      onClick={() => {
                        void navigator.clipboard?.writeText(doc.id)
                        toast.success("Copied the document ID.")
                      }}
                    >
                      <IconCopy className="size-3.5" />
                    </Button>
                  </div>
                ) : (
                  <span>
                    In <span className="font-medium">{tessellation}</span>. The server assigns the ID.
                  </span>
                )}
                {doc?._expires_at && <div className="text-xs">Expires {formatTimestamp(doc._expires_at)}</div>}
              </div>
            </SheetDescription>
          </SheetHeader>

          <div className="min-h-0 flex-1 border-b">
            <CodeEditor value={text} onChange={setText} extensions={extensions} onRun={save} aria-label="Document JSON" />
          </div>

          <div className="grid gap-2 px-4 pt-4">
            <Label htmlFor="doc-ttl">{doc ? "New TTL (seconds, optional)" : "TTL (seconds, optional)"}</Label>
            <Input id="doc-ttl" inputMode="numeric" value={ttl} onChange={(e) => setTtl(e.target.value)} placeholder={doc?._expires_at ? "Keep the current expiry" : "Never expires"} />
          </div>
          {error && (
            <p className="text-destructive flex items-start gap-2 px-4 pt-3 text-sm">
              <IconAlertTriangle className="mt-0.5 size-4 shrink-0" /> {error}
            </p>
          )}

          <SheetFooter className="flex-row">
            {doc && (
              <Button variant="ghost" className="text-destructive hover:text-destructive mr-auto" onClick={() => setDeleting(true)} disabled={busy}>
                <IconTrash /> Delete
              </Button>
            )}
            <Button variant="outline" className="ml-auto" onClick={() => onOpenChange(false)} disabled={busy}>
              Cancel
            </Button>
            <Button onClick={save} disabled={busy}>
              {busy && <IconLoader2 className="animate-spin" />}
              {doc ? "Save" : "Create"}
            </Button>
          </SheetFooter>
        </SheetContent>
      </Sheet>

      {doc && (
        <ConfirmDialog
          open={deleting}
          onOpenChange={setDeleting}
          title="Delete this document?"
          description={<span className="font-mono text-xs">{doc.id}</span>}
          confirmLabel="Delete document"
          onConfirm={async () => {
            await api.deleteDocument(tessellation, doc.id)
            toast.success("Document deleted.")
            onSaved()
            onOpenChange(false)
          }}
        />
      )}
    </>
  )
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

export function DocumentsPage() {
  const tessellations = usePoll(api.tessellations)
  const userTessellations = (tessellations.data ?? []).filter((t) => t.kind === "user").map((t) => t.name).sort()
  const selected = useQueryParam("tessellation")
  const tessellation = selected && userTessellations.includes(selected) ? selected : (userTessellations[0] ?? null)

  const [filterText, setFilterText] = useState("")
  const [sortText, setSortText] = useState("")
  const [applied, setApplied] = useState<{ filter: unknown; sort: string }>({ filter: undefined, sort: "" })
  const [inputError, setInputError] = useState<string | null>(null)
  const [pageSize, setPageSize] = useState("25")
  const [offset, setOffset] = useState(0)
  const [editing, setEditing] = useState<ApiDocument | null | undefined>(undefined)

  // Reset the query when switching tessellations.
  useEffect(() => {
    setFilterText("")
    setSortText("")
    setApplied({ filter: undefined, sort: "" })
    setOffset(0)
    setInputError(null)
  }, [tessellation])

  const page = usePoll(
    () =>
      tessellation
        ? api.queryDocuments(tessellation, { filter: applied.filter, sort: applied.sort || undefined, limit: Number(pageSize), offset })
        : Promise.resolve({ documents: [], total: 0, next: null }),
    0,
    [tessellation, applied, pageSize, offset],
  )

  const apply = (event?: React.FormEvent) => {
    event?.preventDefault()
    let filter: unknown = undefined
    if (filterText.trim()) {
      try {
        filter = JSON.parse(filterText)
      } catch (e) {
        setInputError(`Filter is not valid JSON: ${errorMessage(e)}`)
        return
      }
    }
    setInputError(null)
    setOffset(0)
    setApplied({ filter, sort: sortText.trim() })
  }

  const docs = useMemo(() => page.data?.documents ?? [], [page.data])
  const total = page.data?.total ?? 0
  const columns = useMemo(() => columnsFor(docs), [docs])
  const size = Number(pageSize)

  if (tessellations.data && userTessellations.length === 0) {
    return (
      <div className="text-muted-foreground flex flex-1 flex-col items-center justify-center gap-2 p-8 text-center text-sm">
        <p>No user tessellations yet.</p>
        <p>Create one on the Tessellations page, or insert a document through the API.</p>
      </div>
    )
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-4 p-4 lg:p-6">
      <form onSubmit={apply} className="flex flex-wrap items-end gap-2">
        <div className="grid gap-1.5">
          <Label className="text-muted-foreground text-xs">Tessellation</Label>
          {/* Radix can report an empty value while options load; ignore it so the URL keeps its tessellation. */}
          <Select value={tessellation ?? undefined} onValueChange={(name) => name && setQueryParam("tessellation", name)} disabled={!tessellations.data}>
            <SelectTrigger size="sm" className="w-48" aria-label="Tessellation">
              <SelectValue placeholder="Loading…" />
            </SelectTrigger>
            <SelectContent>
              {userTessellations.map((name) => (
                <SelectItem key={name} value={name}>
                  {name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        <div className="grid min-w-64 flex-1 gap-1.5">
          <Label htmlFor="doc-filter" className="text-muted-foreground text-xs">
            Filter (JSON)
          </Label>
          <Input
            id="doc-filter"
            value={filterText}
            onChange={(e) => setFilterText(e.target.value)}
            placeholder='{"published": true, "views": {"$gte": 100}}'
            className="h-8 font-mono text-xs"
            aria-invalid={!!inputError}
          />
        </div>
        <div className="grid w-44 gap-1.5">
          <Label htmlFor="doc-sort" className="text-muted-foreground text-xs">
            Sort
          </Label>
          <Input id="doc-sort" value={sortText} onChange={(e) => setSortText(e.target.value)} placeholder="-views,title" className="h-8 font-mono text-xs" />
        </div>
        <Button type="submit" variant="outline" size="sm">
          <IconFilter /> Apply
        </Button>
        <Button type="button" size="sm" onClick={() => setEditing(null)} disabled={!tessellation}>
          <IconPlus /> New document
        </Button>
      </form>

      {(inputError || page.error) && (
        <p className="text-destructive flex items-center gap-2 text-sm">
          <IconAlertTriangle className="size-4" /> {inputError ?? page.error?.message}
        </p>
      )}

      <Card className="flex min-h-[20rem] flex-1 flex-col gap-0 overflow-hidden py-0">
        <div className="min-h-0 flex-1">
          {page.loading && !page.data ? (
            <div className="text-muted-foreground flex h-full items-center justify-center gap-2 text-sm">
              <IconLoader2 className="size-4 animate-spin" /> Loading documents…
            </div>
          ) : docs.length === 0 ? (
            <div className="text-muted-foreground flex h-full items-center justify-center text-sm">
              {applied.filter ? "No documents match this filter." : "This tessellation is empty."}
            </div>
          ) : (
            <DocumentTable columns={columns} rows={docs} onRowClick={(row) => setEditing(row as ApiDocument)} selectedId={editing?.id} />
          )}
        </div>
        <div className="text-muted-foreground flex items-center gap-3 border-t px-4 py-2 text-sm">
          <span className="tabular-nums">
            {total === 0 ? "0 documents" : `${formatNumber(offset + 1)}–${formatNumber(offset + docs.length)} of ${formatNumber(total)}`}
          </span>
          {page.data?.plan && (
            <span className="hidden text-xs md:inline" title="How the query ran">
              {page.data.plan.indexes.length
                ? `via ${page.data.plan.indexes.join(", ")} · read ${formatNumber(page.data.plan.scanned)}`
                : `full scan · read ${formatNumber(page.data.plan.scanned)}`}
            </span>
          )}
          <div className="ml-auto flex items-center gap-2">
            <span className="hidden sm:inline">Rows per page</span>
            <Select
              value={pageSize}
              onValueChange={(value) => {
                setPageSize(value)
                setOffset(0)
              }}
            >
              <SelectTrigger size="sm" className="w-20" aria-label="Rows per page">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {PAGE_SIZES.map((s) => (
                  <SelectItem key={s} value={s}>
                    {s}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Button variant="outline" size="icon" className="size-8" aria-label="Previous page" disabled={offset === 0} onClick={() => setOffset(Math.max(0, offset - size))}>
              <IconChevronLeft />
            </Button>
            <Button variant="outline" size="icon" className="size-8" aria-label="Next page" disabled={offset + docs.length >= total} onClick={() => setOffset(offset + size)}>
              <IconChevronRight />
            </Button>
          </div>
        </div>
      </Card>

      {tessellation && (
        <DocumentEditor
          tessellation={tessellation}
          doc={editing ?? null}
          open={editing !== undefined}
          onOpenChange={(open) => !open && setEditing(undefined)}
          onSaved={() => void page.refresh()}
        />
      )}
    </div>
  )
}
