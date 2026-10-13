import { useEffect, useMemo, useRef, useState } from "react"
import {
  IconArrowBackUp,
  IconDownload,
  IconFile,
  IconFileText,
  IconFileTypePdf,
  IconFileZip,
  IconFolder,
  IconFolderPlus,
  IconHistory,
  IconLayoutGrid,
  IconList,
  IconLoader2,
  IconMovie,
  IconMusic,
  IconPencil,
  IconPhoto,
  IconRefresh,
  IconSearch,
  IconTrash,
  IconUpload,
  IconX,
} from "@tabler/icons-react"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/confirm-dialog"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group"
import { usePoll } from "@/hooks/use-poll"
import { api, ApiError, errorMessage, type FileDoc, filePathUrl, fileUrl, uploadFile } from "@/lib/api"
import { can, useAuth } from "@/lib/auth"
import { formatBytes, formatTimestamp } from "@/lib/format"
import { setQueryParam, useQueryParam } from "@/lib/router"

/** Fields the listing needs (meta.text can be large). */
const LIST_FIELDS = ["name", "path", "size", "content_type", "modified_at", "version", "thumbnail", "extract.status", "meta.title", "meta.artist", "meta.duration_ms", "meta.width", "meta.height"]

function iconFor(type: string) {
  if (type.startsWith("image/")) return IconPhoto
  if (type.startsWith("audio/")) return IconMusic
  if (type.startsWith("video/")) return IconMovie
  if (type === "application/pdf") return IconFileTypePdf
  if (type.includes("zip") || type.includes("tar") || type.includes("compressed")) return IconFileZip
  if (type.startsWith("text/") || type.includes("json") || type.includes("xml") || type.includes("document")) return IconFileText
  return IconFile
}

function formatDuration(ms: number): string {
  const total = Math.round(ms / 1000)
  const h = Math.floor(total / 3600)
  const m = Math.floor((total % 3600) / 60)
  const s = total % 60
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}` : `${m}:${String(s).padStart(2, "0")}`
}

/** "/a/b/" -> ["a", "b"]. */
function segments(folder: string): string[] {
  return folder.split("/").filter(Boolean)
}

function folderOf(parts: string[]): string {
  return parts.length ? `/${parts.join("/")}/` : "/"
}

// ---------------------------------------------------------------------------
// Uploads
// ---------------------------------------------------------------------------

interface UploadItem {
  key: string
  file: File
  folder: string
  sent: number
  state: "waiting" | "uploading" | "done" | "failed"
  error?: string
}

function UploadPanel({ items, onClear }: { items: UploadItem[]; onClear: () => void }) {
  if (items.length === 0) return null
  const active = items.filter((i) => i.state === "waiting" || i.state === "uploading").length
  return (
    <Card className="fixed right-4 bottom-4 z-40 w-96 gap-2 p-3 shadow-lg">
      <div className="flex items-center justify-between">
        <span className="text-sm font-medium">{active ? `Uploading ${active} of ${items.length}` : `Uploaded ${items.filter((i) => i.state === "done").length} of ${items.length}`}</span>
        {!active && (
          <Button variant="ghost" size="icon" className="size-6" onClick={onClear} aria-label="Close">
            <IconX className="size-4" />
          </Button>
        )}
      </div>
      <div className="max-h-60 space-y-2 overflow-y-auto">
        {items.map((item) => {
          const pct = item.file.size ? Math.min(100, Math.round((item.sent / item.file.size) * 100)) : 100
          return (
            <div key={item.key} className="text-xs">
              <div className="flex justify-between gap-2">
                <span className="truncate">{item.file.name}</span>
                <span className={item.state === "failed" ? "text-destructive" : "text-muted-foreground"}>
                  {item.state === "failed" ? "failed" : item.state === "done" ? formatBytes(item.file.size) : `${pct}%`}
                </span>
              </div>
              <div className="bg-muted mt-1 h-1 overflow-hidden rounded">
                <div className={item.state === "failed" ? "bg-destructive h-1" : "bg-primary h-1"} style={{ width: `${item.state === "done" ? 100 : pct}%` }} />
              </div>
              {item.error && <p className="text-destructive mt-1">{item.error}</p>}
            </div>
          )
        })}
      </div>
    </Card>
  )
}

// ---------------------------------------------------------------------------
// Details
// ---------------------------------------------------------------------------

/** Normalized metadata fields, in display order. */
const META_LABELS: [string, string][] = [
  ["title", "Title"],
  ["artist", "Artist"],
  ["album", "Album"],
  ["album_artist", "Album artist"],
  ["composer", "Composer"],
  ["genre", "Genre"],
  ["year", "Year"],
  ["track", "Track"],
  ["author", "Author"],
  ["subject", "Subject"],
  ["description", "Description"],
  ["taken_at", "Taken"],
  ["recorded_at", "Recorded"],
  ["duration_ms", "Duration"],
  ["dimensions", "Dimensions"],
  ["camera", "Camera"],
  ["exposure", "Exposure"],
  ["gps", "Location"],
  ["location", "Place"],
  ["pages", "Pages"],
  ["words", "Words"],
  ["codec", "Codec"],
  ["sample_rate", "Sample rate"],
  ["channels", "Channels"],
  ["bit_depth", "Bit depth"],
  ["bitrate_kbps", "Bitrate"],
  ["color_mode", "Color mode"],
  ["layers", "Layers"],
  ["keywords", "Keywords"],
  ["language", "Language"],
  ["copyright", "Copyright"],
  ["creator_tool", "Created with"],
]

function metaValue(key: string, meta: Record<string, unknown>): React.ReactNode {
  const value = meta[key]
  switch (key) {
    case "duration_ms":
      return typeof value === "number" ? formatDuration(value) : null
    case "dimensions":
      return typeof meta.width === "number" && typeof meta.height === "number" ? `${meta.width} × ${meta.height}` : null
    case "sample_rate":
      return typeof value === "number" ? `${(value / 1000).toFixed(1)} kHz` : null
    case "bitrate_kbps":
      return typeof value === "number" ? `${value} kbps` : null
    case "gps": {
      const g = value as { lat?: number; lon?: number; alt?: number } | undefined
      if (typeof g?.lat !== "number" || typeof g?.lon !== "number") return null
      return `${g.lat.toFixed(5)}, ${g.lon.toFixed(5)}${typeof g.alt === "number" ? ` (${g.alt} m)` : ""}`
    }
    case "camera":
    case "exposure":
    case "location": {
      if (!value || typeof value !== "object") return null
      return Object.entries(value as Record<string, unknown>)
        .map(([k, v]) => (key === "camera" && (k === "make" || k === "model") ? String(v) : `${k.replace(/_/g, " ")} ${v}`))
        .join(" · ")
    }
    case "keywords":
      return Array.isArray(value) && value.length ? (
        <div className="flex flex-wrap gap-1">
          {value.map((k) => (
            <Badge key={String(k)} variant="secondary">
              {String(k)}
            </Badge>
          ))}
        </div>
      ) : null
    default:
      return value === undefined || value === null || typeof value === "object" ? null : String(value)
  }
}

function Preview({ tess, file }: { tess: string; file: FileDoc }) {
  const url = fileUrl(tess, file.id)
  const type = file.content_type
  if (type.startsWith("image/") && type !== "image/svg+xml") {
    return <img src={file.thumbnail && file.size > 20 * 1024 * 1024 ? fileUrl(tess, file.id, "thumbnail") : url} alt={file.name} className="bg-muted max-h-80 w-full rounded object-contain" />
  }
  if (type === "image/svg+xml") return <img src={url} alt={file.name} className="bg-muted max-h-80 w-full rounded object-contain" />
  if (type.startsWith("audio/")) return <audio controls preload="metadata" src={url} className="w-full" />
  if (type.startsWith("video/")) return <video controls preload="metadata" src={url} className="bg-muted max-h-80 w-full rounded" />
  if (file.thumbnail) return <img src={fileUrl(tess, file.id, "thumbnail")} alt={file.name} className="bg-muted max-h-80 w-full rounded object-contain" />
  const Icon = iconFor(type)
  return (
    <div className="bg-muted text-muted-foreground flex h-32 items-center justify-center rounded">
      <Icon className="size-12" stroke={1.2} />
    </div>
  )
}

function FileDetails({
  tess,
  id,
  open,
  onOpenChange,
  onChanged,
  writable,
}: {
  tess: string
  id: string | null
  open: boolean
  onOpenChange: (open: boolean) => void
  onChanged: () => void
  writable: boolean
}) {
  const [file, setFile] = useState<FileDoc | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [editing, setEditing] = useState(false)
  const [name, setName] = useState("")
  const [folder, setFolder] = useState("")
  const [tags, setTags] = useState("")
  const [description, setDescription] = useState("")
  const [busy, setBusy] = useState(false)
  const [deleting, setDeleting] = useState(false)

  const load = async () => {
    if (!id) return
    try {
      const doc = await api.file(tess, id)
      setFile(doc)
      setName(doc.name)
      setFolder(doc.path)
      setTags((doc.tags ?? []).join(", "))
      setDescription(typeof doc.description === "string" ? doc.description : "")
      setError(null)
    } catch (e) {
      setError(errorMessage(e))
    }
  }

  useEffect(() => {
    setFile(null)
    setEditing(false)
    if (open) void load()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, id, tess])

  // Metadata arrives a moment after an upload.
  useEffect(() => {
    if (!open || file?.extract?.status !== "pending") return
    const timer = window.setTimeout(() => void load(), 1500)
    return () => window.clearTimeout(timer)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, file?.extract?.status, file?.version])

  const save = async () => {
    if (!file) return
    setBusy(true)
    try {
      const doc = await api.patchFile(tess, file.id, {
        name: name.trim(),
        path: folder.trim() || "/",
        tags: tags.split(",").map((t) => t.trim()).filter(Boolean),
        description: description.trim(),
      })
      setFile({ ...file, ...doc })
      setEditing(false)
      toast.success("Saved.")
      onChanged()
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  const meta = (file?.meta ?? {}) as Record<string, unknown>
  const rows = META_LABELS.map(([key, label]) => [label, metaValue(key, meta)] as const).filter(([, v]) => v !== null && v !== "")
  const raw = Object.fromEntries(Object.entries(meta).filter(([k, v]) => k !== "text" && typeof v === "object" && !["camera", "exposure", "gps", "location"].includes(k)))

  return (
    <>
      <Sheet open={open} onOpenChange={onOpenChange}>
        <SheetContent className="flex w-full flex-col gap-0 overflow-y-auto sm:max-w-xl">
          <SheetHeader className="border-b">
            <SheetTitle className="truncate pr-6">{file?.name ?? "File"}</SheetTitle>
            <SheetDescription>{file ? `${file.path} · ${formatBytes(file.size)} · ${file.content_type}` : error ?? "Loading…"}</SheetDescription>
          </SheetHeader>
          {file && (
            <div className="space-y-5 p-4">
              <Preview tess={tess} file={file} />
              <div className="flex flex-wrap gap-2">
                <Button asChild size="sm" variant="outline">
                  <a href={fileUrl(tess, file.id, "content", "?download=true")}>
                    <IconDownload /> Download
                  </a>
                </Button>
                {file.content_type === "application/pdf" && (
                  <Button asChild size="sm" variant="outline">
                    <a href={fileUrl(tess, file.id)} target="_blank" rel="noreferrer">
                      Open
                    </a>
                  </Button>
                )}
                {writable && (
                  <>
                    <Button size="sm" variant="outline" onClick={() => setEditing((e) => !e)}>
                      <IconPencil /> Edit
                    </Button>
                    <Button
                      size="sm"
                      variant="outline"
                      onClick={async () => {
                        try {
                          await api.reextract(tess, file.id)
                          setFile({ ...file, extract: { status: "pending" } })
                          toast.success("Reading the metadata again.")
                        } catch (e) {
                          toast.error(errorMessage(e))
                        }
                      }}
                    >
                      <IconRefresh /> Re-read metadata
                    </Button>
                    <Button size="sm" variant="outline" className="text-destructive" onClick={() => setDeleting(true)}>
                      <IconTrash /> Delete
                    </Button>
                  </>
                )}
              </div>

              {editing && (
                <div className="grid gap-3 rounded border p-3">
                  <div className="grid gap-1">
                    <Label htmlFor="file-name">Name</Label>
                    <Input id="file-name" value={name} onChange={(e) => setName(e.target.value)} />
                  </div>
                  <div className="grid gap-1">
                    <Label htmlFor="file-folder">Folder</Label>
                    <Input id="file-folder" value={folder} onChange={(e) => setFolder(e.target.value)} placeholder="/photos/2024/" />
                  </div>
                  <div className="grid gap-1">
                    <Label htmlFor="file-tags">Tags</Label>
                    <Input id="file-tags" value={tags} onChange={(e) => setTags(e.target.value)} placeholder="holiday, family" />
                  </div>
                  <div className="grid gap-1">
                    <Label htmlFor="file-description">Description</Label>
                    <Input id="file-description" value={description} onChange={(e) => setDescription(e.target.value)} />
                  </div>
                  <div className="flex justify-end gap-2">
                    <Button size="sm" variant="outline" onClick={() => setEditing(false)} disabled={busy}>
                      Cancel
                    </Button>
                    <Button size="sm" onClick={save} disabled={busy || !name.trim()}>
                      {busy && <IconLoader2 className="animate-spin" />} Save
                    </Button>
                  </div>
                </div>
              )}

              <section className="space-y-2">
                <h3 className="text-sm font-medium">Details</h3>
                <dl className="grid grid-cols-[8rem_1fr] gap-x-3 gap-y-1 text-sm">
                  {rows.map(([label, value]) => (
                    <div key={label} className="contents">
                      <dt className="text-muted-foreground">{label}</dt>
                      <dd className="min-w-0 break-words">{value}</dd>
                    </div>
                  ))}
                  {(file.tags ?? []).length > 0 && (
                    <>
                      <dt className="text-muted-foreground">Tags</dt>
                      <dd className="flex flex-wrap gap-1">
                        {(file.tags ?? []).map((t) => (
                          <Badge key={t} variant="outline">
                            {t}
                          </Badge>
                        ))}
                      </dd>
                    </>
                  )}
                  <dt className="text-muted-foreground">Modified</dt>
                  <dd>{formatTimestamp(file.modified_at)}</dd>
                  <dt className="text-muted-foreground">Uploaded</dt>
                  <dd>
                    {formatTimestamp(file.uploaded_at)} by {file.uploaded_by}
                  </dd>
                  <dt className="text-muted-foreground">SHA-256</dt>
                  <dd className="font-mono text-xs break-all">{file.sha256}</dd>
                </dl>
                {file.extract?.status === "pending" && (
                  <p className="text-muted-foreground flex items-center gap-2 text-xs">
                    <IconLoader2 className="size-3 animate-spin" /> Reading metadata…
                  </p>
                )}
                {(file.extract?.errors ?? []).length > 0 && (
                  <p className="text-muted-foreground text-xs">Some metadata couldn't be read: {(file.extract?.errors ?? []).join("; ")}</p>
                )}
              </section>

              <section className="space-y-2">
                <h3 className="flex items-center gap-2 text-sm font-medium">
                  <IconHistory className="size-4" /> Versions
                </h3>
                <Table>
                  <TableHeader>
                    <TableRow>
                      <TableHead>Version</TableHead>
                      <TableHead>Size</TableHead>
                      <TableHead>Uploaded</TableHead>
                      <TableHead />
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    <TableRow>
                      <TableCell>
                        {file.version} <Badge variant="secondary">current</Badge>
                      </TableCell>
                      <TableCell>{formatBytes(file.size)}</TableCell>
                      <TableCell>{formatTimestamp(file.uploaded_at)}</TableCell>
                      <TableCell />
                    </TableRow>
                    {(file.versions ?? []).map((v) => (
                      <TableRow key={v.version}>
                        <TableCell>{v.version}</TableCell>
                        <TableCell>{formatBytes(v.size)}</TableCell>
                        <TableCell>
                          {formatTimestamp(v.uploaded_at)} <span className="text-muted-foreground">by {v.uploaded_by}</span>
                        </TableCell>
                        <TableCell className="text-right whitespace-nowrap">
                          <Button asChild size="icon" variant="ghost" className="size-7" aria-label={`Download version ${v.version}`}>
                            <a href={fileUrl(tess, file.id, "content", `?version=${v.version}&download=true`)}>
                              <IconDownload className="size-4" />
                            </a>
                          </Button>
                          {writable && (
                            <Button
                              size="icon"
                              variant="ghost"
                              className="size-7"
                              aria-label={`Restore version ${v.version}`}
                              onClick={async () => {
                                try {
                                  const doc = await api.restoreVersion(tess, file.id, v.version)
                                  toast.success(`Version ${v.version} is current again (as version ${doc.version}).`)
                                  await load()
                                  onChanged()
                                } catch (e) {
                                  toast.error(errorMessage(e))
                                }
                              }}
                            >
                              <IconArrowBackUp className="size-4" />
                            </Button>
                          )}
                        </TableCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </Table>
              </section>

              {Object.keys(raw).length > 0 && (
                <details className="text-sm">
                  <summary className="cursor-pointer font-medium">All metadata ({Object.keys(raw).join(", ")})</summary>
                  <pre className="bg-muted mt-2 max-h-96 overflow-auto rounded p-2 text-xs">{JSON.stringify(raw, null, 2)}</pre>
                </details>
              )}
              {typeof meta.text === "string" && meta.text && (
                <details className="text-sm">
                  <summary className="cursor-pointer font-medium">Text</summary>
                  <pre className="bg-muted mt-2 max-h-96 overflow-auto rounded p-2 text-xs whitespace-pre-wrap">{meta.text}</pre>
                </details>
              )}
            </div>
          )}
        </SheetContent>
      </Sheet>
      {file && (
        <ConfirmDialog
          open={deleting}
          onOpenChange={setDeleting}
          title="Move to trash?"
          description={
            <>
              <span className="font-medium">{file.name}</span> goes to the trash, where it can be restored until it expires.
            </>
          }
          confirmLabel="Move to trash"
          onConfirm={async () => {
            await api.trashFile(tess, file.id)
            toast.success(`Moved ${file.name} to the trash.`)
            onOpenChange(false)
            onChanged()
          }}
        />
      )}
    </>
  )
}

// ---------------------------------------------------------------------------
// Trash
// ---------------------------------------------------------------------------

function TrashView({ tess, writable, onChanged }: { tess: string; writable: boolean; onChanged: () => void }) {
  const trash = usePoll(() => api.trash(tess), 0, [tess])
  const [emptying, setEmptying] = useState(false)
  const docs = trash.data?.documents ?? []

  const restore = async (file: FileDoc) => {
    try {
      await api.restoreFile(tess, file.id)
    } catch (e) {
      if (!(e instanceof ApiError) || e.status !== 409) {
        toast.error(errorMessage(e))
        return
      }
      // Another file took the name meanwhile: restore beside it.
      const dot = file.name.lastIndexOf(".")
      const renamed = dot > 0 ? `${file.name.slice(0, dot)} (restored)${file.name.slice(dot)}` : `${file.name} (restored)`
      try {
        await api.restoreFile(tess, file.id, renamed)
      } catch (e2) {
        toast.error(errorMessage(e2))
        return
      }
    }
    toast.success(`Restored ${file.name} to ${file.path}.`)
    await trash.refresh()
    onChanged()
  }

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between">
        <p className="text-muted-foreground text-sm">Deleted files stay here until they expire. Restoring puts a file back in its folder.</p>
        {writable && docs.length > 0 && (
          <Button size="sm" variant="outline" className="text-destructive" onClick={() => setEmptying(true)}>
            <IconTrash /> Empty trash
          </Button>
        )}
      </div>
      <Card className="py-0">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Name</TableHead>
              <TableHead>Folder</TableHead>
              <TableHead>Size</TableHead>
              <TableHead>Deleted</TableHead>
              <TableHead>Expires</TableHead>
              <TableHead />
            </TableRow>
          </TableHeader>
          <TableBody>
            {docs.length === 0 && (
              <TableRow>
                <TableCell colSpan={6} className="text-muted-foreground py-8 text-center">
                  {trash.loading ? "Loading…" : "The trash is empty."}
                </TableCell>
              </TableRow>
            )}
            {docs.map((file) => (
              <TableRow key={file.id}>
                <TableCell className="font-medium">{file.name}</TableCell>
                <TableCell className="text-muted-foreground">{file.path}</TableCell>
                <TableCell>{formatBytes(file.size)}</TableCell>
                <TableCell>
                  {formatTimestamp(file.trashed_at)} <span className="text-muted-foreground">by {file.trashed_by}</span>
                </TableCell>
                <TableCell>{formatTimestamp(file._expires_at)}</TableCell>
                <TableCell className="text-right whitespace-nowrap">
                  {writable && (
                    <>
                      <Button size="sm" variant="ghost" onClick={() => void restore(file)}>
                        <IconArrowBackUp /> Restore
                      </Button>
                      <Button
                        size="sm"
                        variant="ghost"
                        className="text-destructive"
                        onClick={async () => {
                          try {
                            await api.purgeFile(tess, file.id)
                            await trash.refresh()
                          } catch (e) {
                            toast.error(errorMessage(e))
                          }
                        }}
                      >
                        Delete forever
                      </Button>
                    </>
                  )}
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </Card>
      <ConfirmDialog
        open={emptying}
        onOpenChange={setEmptying}
        title="Empty the trash?"
        description={`The ${docs.length} file(s) in the trash are deleted for good. Their content is freed at the next compaction.`}
        confirmLabel="Empty trash"
        onConfirm={async () => {
          const { purged } = await api.emptyTrash(tess)
          toast.success(`Deleted ${purged} file(s) for good.`)
          await trash.refresh()
        }}
      />
    </div>
  )
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

function NewFilesTessellation({ onCreated }: { onCreated: (name: string) => void }) {
  const [name, setName] = useState("")
  const [busy, setBusy] = useState(false)
  return (
    <form
      className="flex gap-2"
      onSubmit={async (e) => {
        e.preventDefault()
        setBusy(true)
        try {
          await api.createTessellation(name.trim(), "files")
          toast.success(`Created '${name.trim()}' for files.`)
          onCreated(name.trim())
          setName("")
        } catch (err) {
          toast.error(errorMessage(err))
        } finally {
          setBusy(false)
        }
      }}
    >
      <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="media" className="w-48" />
      <Button type="submit" disabled={busy || !name.trim()}>
        {busy && <IconLoader2 className="animate-spin" />} Create files tessellation
      </Button>
    </form>
  )
}

export function FilesPage() {
  const { me } = useAuth()
  const tessellations = usePoll(api.tessellations, 30_000)
  const filesTess = (tessellations.data ?? []).filter((t) => t.kind === "files")
  const chosen = useQueryParam("tessellation")
  const tess = chosen && filesTess.some((t) => t.name === chosen) ? chosen : filesTess[0]?.name
  const folder = useQueryParam("path") || "/"
  const [view, setView] = useState<"grid" | "list">("grid")
  const [mode, setMode] = useState<"files" | "trash">("files")
  const [searchText, setSearchText] = useState("")
  const [query, setQuery] = useState("")
  const [selected, setSelected] = useState<string | null>(null)
  const [uploads, setUploads] = useState<UploadItem[]>([])
  const [dragging, setDragging] = useState(false)
  const [newFolder, setNewFolder] = useState("")
  const input = useRef<HTMLInputElement>(null)
  const writable = !!tess && can(me, "write", tess)

  useEffect(() => {
    const timer = window.setTimeout(() => setQuery(searchText.trim()), 300)
    return () => window.clearTimeout(timer)
  }, [searchText])

  const listing = usePoll(
    async () => {
      if (!tess) return undefined
      const filter = query ? { $text: query } : { path: folder }
      const files = await api.files(tess, { filter, sort: query ? undefined : "name", limit: 500, fields: LIST_FIELDS, total: true })
      let folders: [string, number][] = []
      if (!query) {
        const groups = await api.aggregate(tess, { filter: { path: { $startsWith: folder } }, group_by: ["path"], aggregates: { files: { $count: "*" } }, limit: 10000 })
        const depth = segments(folder).length
        const counts = new Map<string, number>()
        for (const row of groups.rows) {
          const parts = segments(String(row.path ?? ""))
          if (parts.length > depth) counts.set(parts[depth], (counts.get(parts[depth]) ?? 0) + Number(row.files ?? 0))
        }
        folders = [...counts.entries()].sort((a, b) => a[0].localeCompare(b[0]))
      }
      return { files: files.documents, total: files.total ?? files.documents.length, folders }
    },
    0,
    [tess, folder, query],
  )

  const go = (path: string) => {
    setQueryParam("path", path === "/" ? null : path)
    setSearchText("")
  }

  const startUploads = (files: FileList | File[]) => {
    if (!tess || !writable) return
    const added: UploadItem[] = Array.from(files).map((file) => ({ key: `${Date.now()}-${Math.random()}`, file, folder, sent: 0, state: "waiting" }))
    setUploads((list) => [...list.filter((i) => i.state !== "done"), ...added])
    void (async () => {
      for (const item of added) {
        const update = (patch: Partial<UploadItem>) => setUploads((list) => list.map((i) => (i.key === item.key ? { ...i, ...patch } : i)))
        update({ state: "uploading" })
        try {
          await uploadFile(tess, item.folder, item.file, (sent) => update({ sent }))
          update({ state: "done", sent: item.file.size })
        } catch (e) {
          update({ state: "failed", error: errorMessage(e) })
        }
      }
      await listing.refresh()
    })()
  }

  const crumbs = segments(folder)
  const files = listing.data?.files ?? []
  const folders = listing.data?.folders ?? []
  const empty = !listing.loading && files.length === 0 && folders.length === 0

  const fileCard = useMemo(
    () => (file: FileDoc) => {
      const Icon = iconFor(file.content_type)
      const meta = (file.meta ?? {}) as Record<string, unknown>
      return (
        <button key={file.id} type="button" onClick={() => setSelected(file.id)} className="hover:bg-accent group flex flex-col overflow-hidden rounded-lg border text-left">
          <div className="bg-muted flex aspect-[4/3] items-center justify-center overflow-hidden">
            {file.thumbnail && tess ? (
              <img src={fileUrl(tess, file.id, "thumbnail")} alt="" loading="lazy" className="h-full w-full object-cover" />
            ) : (
              <Icon className="text-muted-foreground size-10" stroke={1.2} />
            )}
          </div>
          <div className="min-w-0 p-2">
            <div className="truncate text-sm font-medium" title={file.name}>
              {file.name}
            </div>
            <div className="text-muted-foreground truncate text-xs">
              {typeof meta.duration_ms === "number" ? `${formatDuration(meta.duration_ms)} · ` : ""}
              {formatBytes(file.size)}
              {query ? ` · ${file.path}` : ""}
            </div>
          </div>
        </button>
      )
    },
    [tess, query],
  )

  if (tessellations.loading && !tessellations.data) {
    return <div className="text-muted-foreground p-6 text-sm">Loading…</div>
  }
  if (!tess) {
    return (
      <div className="space-y-4 p-6">
        <p className="text-muted-foreground text-sm">
          No files tessellation yet. A files tessellation stores files of any kind (photos, audio, video, documents) with their metadata indexed for search.
        </p>
        {me?.is_admin && <NewFilesTessellation onCreated={(name) => setQueryParam("tessellation", name)} />}
      </div>
    )
  }

  return (
    <div
      className="relative flex flex-1 flex-col gap-4 p-4 lg:p-6"
      onDragOver={(e) => {
        if (writable && mode === "files" && e.dataTransfer.types.includes("Files")) {
          e.preventDefault()
          setDragging(true)
        }
      }}
      onDragLeave={(e) => {
        if (e.currentTarget === e.target) setDragging(false)
      }}
      onDrop={(e) => {
        e.preventDefault()
        setDragging(false)
        if (e.dataTransfer.files.length) startUploads(e.dataTransfer.files)
      }}
    >
      <div className="flex flex-wrap items-center gap-2">
        <Select value={tess} onValueChange={(v) => { setQueryParam("path", null); setQueryParam("tessellation", v) }}>
          <SelectTrigger className="w-44">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {filesTess.map((t) => (
              <SelectItem key={t.name} value={t.name}>
                {t.name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        <div className="relative max-w-sm flex-1">
          <IconSearch className="text-muted-foreground absolute top-2.5 left-2.5 size-4" />
          <Input value={searchText} onChange={(e) => setSearchText(e.target.value)} placeholder="Search names, tags and metadata" className="pl-8" disabled={mode === "trash"} />
        </div>
        <ToggleGroup type="single" value={mode} onValueChange={(v) => v && setMode(v as "files" | "trash")} variant="outline" size="sm">
          <ToggleGroupItem value="files">Files</ToggleGroupItem>
          <ToggleGroupItem value="trash">Trash</ToggleGroupItem>
        </ToggleGroup>
        {mode === "files" && (
          <ToggleGroup type="single" value={view} onValueChange={(v) => v && setView(v as "grid" | "list")} variant="outline" size="sm">
            <ToggleGroupItem value="grid" aria-label="Grid">
              <IconLayoutGrid className="size-4" />
            </ToggleGroupItem>
            <ToggleGroupItem value="list" aria-label="List">
              <IconList className="size-4" />
            </ToggleGroupItem>
          </ToggleGroup>
        )}
        <div className="ml-auto flex gap-2">
          {writable && mode === "files" && (
            <>
              <input ref={input} type="file" multiple className="hidden" onChange={(e) => e.target.files && startUploads(e.target.files)} />
              <Button size="sm" onClick={() => input.current?.click()}>
                <IconUpload /> Upload
              </Button>
            </>
          )}
        </div>
      </div>

      {mode === "trash" ? (
        <TrashView tess={tess} writable={writable} onChanged={() => void listing.refresh()} />
      ) : (
        <>
          <div className="flex flex-wrap items-center gap-1 text-sm">
            {query ? (
              <span className="text-muted-foreground">
                {listing.data ? `${listing.data.total} result(s) for “${query}”` : "Searching…"}
              </span>
            ) : (
              <>
                <button type="button" className="hover:underline" onClick={() => go("/")}>
                  {tess}
                </button>
                {crumbs.map((part, i) => (
                  <span key={i} className="flex items-center gap-1">
                    <span className="text-muted-foreground">/</span>
                    <button type="button" className="hover:underline" onClick={() => go(folderOf(crumbs.slice(0, i + 1)))}>
                      {part}
                    </button>
                  </span>
                ))}
                {writable && (
                  <form
                    className="ml-3 flex items-center gap-1"
                    onSubmit={(e) => {
                      e.preventDefault()
                      const name = newFolder.trim().replace(/[\\/]/g, "")
                      if (name) go(folderOf([...crumbs, name]))
                      setNewFolder("")
                    }}
                  >
                    <Input value={newFolder} onChange={(e) => setNewFolder(e.target.value)} placeholder="New folder" className="h-7 w-36 text-xs" />
                    <Button type="submit" size="icon" variant="ghost" className="size-7" aria-label="Open new folder" disabled={!newFolder.trim()}>
                      <IconFolderPlus className="size-4" />
                    </Button>
                  </form>
                )}
              </>
            )}
            {listing.error && <span className="text-destructive ml-3">{listing.error.message}</span>}
          </div>

          {empty && (
            <div className="text-muted-foreground flex flex-1 flex-col items-center justify-center gap-2 rounded-lg border border-dashed p-12 text-sm">
              <IconUpload className="size-8" stroke={1.2} />
              {query ? "Nothing matches." : writable ? "This folder is empty. Drop files here or use Upload." : "This folder is empty."}
            </div>
          )}

          {view === "grid" ? (
            <div className="grid grid-cols-[repeat(auto-fill,minmax(10rem,1fr))] gap-3">
              {folders.map(([name, count]) => (
                <button key={name} type="button" onClick={() => go(folderOf([...crumbs, name]))} className="hover:bg-accent flex items-center gap-2 rounded-lg border p-3 text-left">
                  <IconFolder className="text-muted-foreground size-6 shrink-0" />
                  <div className="min-w-0">
                    <div className="truncate text-sm font-medium">{name}</div>
                    <div className="text-muted-foreground text-xs">{count} file(s)</div>
                  </div>
                </button>
              ))}
              {files.map(fileCard)}
            </div>
          ) : (
            <Card className="py-0">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>Name</TableHead>
                    {query && <TableHead>Folder</TableHead>}
                    <TableHead>Type</TableHead>
                    <TableHead>Size</TableHead>
                    <TableHead>Modified</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {folders.map(([name, count]) => (
                    <TableRow key={name} className="cursor-pointer" onClick={() => go(folderOf([...crumbs, name]))}>
                      <TableCell className="flex items-center gap-2 font-medium">
                        <IconFolder className="text-muted-foreground size-4" /> {name}
                      </TableCell>
                      {query && <TableCell />}
                      <TableCell className="text-muted-foreground">Folder</TableCell>
                      <TableCell className="text-muted-foreground">{count} file(s)</TableCell>
                      <TableCell />
                    </TableRow>
                  ))}
                  {files.map((file) => {
                    const Icon = iconFor(file.content_type)
                    return (
                      <TableRow key={file.id} className="cursor-pointer" onClick={() => setSelected(file.id)}>
                        <TableCell className="flex items-center gap-2 font-medium">
                          <Icon className="text-muted-foreground size-4" /> {file.name}
                        </TableCell>
                        {query && <TableCell className="text-muted-foreground">{file.path}</TableCell>}
                        <TableCell className="text-muted-foreground">{file.content_type}</TableCell>
                        <TableCell>{formatBytes(file.size)}</TableCell>
                        <TableCell>{formatTimestamp(file.modified_at)}</TableCell>
                      </TableRow>
                    )
                  })}
                </TableBody>
              </Table>
            </Card>
          )}
        </>
      )}

      {dragging && (
        <div className="border-primary bg-background/80 pointer-events-none absolute inset-2 flex items-center justify-center rounded-lg border-2 border-dashed text-sm font-medium">
          Drop to upload to {filePathUrl(folder, "") ? `/${decodeURIComponent(filePathUrl(folder, ""))}/` : "/"}
        </div>
      )}

      <FileDetails tess={tess} id={selected} open={!!selected} onOpenChange={(open) => !open && setSelected(null)} onChanged={() => void listing.refresh()} writable={writable} />
      <UploadPanel items={uploads} onClear={() => setUploads([])} />
    </div>
  )
}
