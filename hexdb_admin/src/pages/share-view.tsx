import { useCallback, useEffect, useState } from "react"
import { IconDownload, IconFile, IconLoader2, IconLock } from "@tabler/icons-react"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { formatBytes } from "@/lib/format"

/** What a link says about itself before it's unlocked. */
interface LinkInfo {
  code: string
  kind: "file" | "folder"
  name: string
  expires_at: number | null
  password: boolean
  downloads_left: number | null
  file?: { id: string; name: string; size: number; content_type: string }
}

interface SharedFile {
  id: string
  name: string
  folder: string
  size: number
  content_type: string
  thumbnail: boolean
}

/** The public page of a sharing link, for people without an account (no sign-in, no app chrome). */
export function ShareView({ code }: { code: string }) {
  const [info, setInfo] = useState<LinkInfo | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [token, setToken] = useState<string | null>(null)
  const [password, setPassword] = useState("")
  const [unlockError, setUnlockError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [files, setFiles] = useState<SharedFile[] | null>(null)
  const base = `/_s/${encodeURIComponent(code)}`
  const query = (extra: Record<string, string> = {}) => {
    const params = new URLSearchParams({ ...extra, ...(token ? { t: token } : {}) })
    const text = params.toString()
    return text ? `?${text}` : ""
  }

  useEffect(() => {
    fetch(base, { headers: { Accept: "application/json" } })
      .then(async (r) => {
        const body = await r.json().catch(() => ({}))
        if (!r.ok) throw new Error(body?.error?.message ?? "This link doesn't exist or has expired.")
        setInfo(body)
      })
      .catch((e) => setError(e.message))
  }, [base])

  const loadFiles = useCallback(async () => {
    const r = await fetch(`${base}/files${token ? `?t=${encodeURIComponent(token)}` : ""}`)
    const body = await r.json().catch(() => ({}))
    if (r.ok) setFiles(body.files)
    else setError(body?.error?.message ?? "This link doesn't exist or has expired.")
  }, [base, token])

  const ready = info && (!info.password || token)
  useEffect(() => {
    if (ready && info?.kind === "folder") void loadFiles()
  }, [ready, info?.kind, loadFiles])

  const unlock = async () => {
    setBusy(true)
    setUnlockError(null)
    try {
      const r = await fetch(`${base}/unlock`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ password }) })
      const body = await r.json().catch(() => ({}))
      if (!r.ok) throw new Error(body?.error?.message ?? "That password isn't right.")
      setToken(body.token)
    } catch (e) {
      setUnlockError((e as Error).message)
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="bg-background text-foreground flex min-h-svh items-start justify-center p-4 pt-16">
      <Card className="w-full max-w-xl">
        <CardHeader>
          <div className="text-muted-foreground mb-2 flex items-center gap-2 text-xs">
            <img src={`${import.meta.env.BASE_URL}hexdb_lg.png`} alt="HexDB" className="h-5 w-auto" /> Shared with you
          </div>
          <CardTitle className="flex items-center gap-2 break-all">
            <IconFile className="size-5 shrink-0" /> {info?.file?.name ?? info?.name ?? (error ? "Link unavailable" : "Loading…")}
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-4">
          {error && <p className="text-muted-foreground text-sm">{error}</p>}
          {info && info.password && !token && (
            <form
              className="flex gap-2"
              onSubmit={(e) => {
                e.preventDefault()
                void unlock()
              }}
            >
              <Input type="password" placeholder="Password" value={password} onChange={(e) => setPassword(e.target.value)} aria-label="Password" autoFocus />
              <Button type="submit" disabled={busy || !password}>
                {busy ? <IconLoader2 className="animate-spin" /> : <IconLock />} Unlock
              </Button>
            </form>
          )}
          {unlockError && <p className="text-destructive text-sm">{unlockError}</p>}
          {ready && info.kind === "file" && (
            <div className="space-y-3">
              {info.file && <p className="text-muted-foreground text-sm">{formatBytes(info.file.size)} · {info.file.content_type}</p>}
              {/* Previews use the thumbnail, which isn't a download; players stream the file, so only on links without a download limit. */}
              {info.file?.content_type.startsWith("image/") && (
                <img
                  src={`${base}/thumbnail${query()}`}
                  alt={info.file.name}
                  className="max-h-96 rounded-md border object-contain"
                  onError={(e) => (e.currentTarget.style.display = "none")}
                />
              )}
              {info.downloads_left === null && info.file?.content_type.startsWith("audio/") && (
                <audio controls preload="none" className="w-full" src={`${base}/content${query()}`} />
              )}
              {info.downloads_left === null && info.file?.content_type.startsWith("video/") && (
                <video controls preload="none" className="w-full rounded-md" poster={`${base}/thumbnail${query()}`} src={`${base}/content${query()}`} />
              )}
              <Button asChild>
                <a href={`${base}/content${query({ download: "true" })}`}>
                  <IconDownload /> Download
                </a>
              </Button>
            </div>
          )}
          {ready && info.kind === "folder" && (
            <ul className="divide-y">
              {files?.length === 0 && <li className="text-muted-foreground py-2 text-sm">This folder is empty.</li>}
              {files?.map((f) => (
                <li key={f.id} className="flex items-center gap-3 py-2">
                  {f.thumbnail ? (
                    <img src={`${base}/thumbnail${query({ file: f.id })}`} alt="" className="size-10 rounded object-cover" />
                  ) : (
                    <IconFile className="text-muted-foreground size-10 p-2" />
                  )}
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-sm font-medium">{f.name}</div>
                    <div className="text-muted-foreground text-xs">
                      {f.folder !== "/" ? `${f.folder} · ` : ""}
                      {formatBytes(f.size)}
                    </div>
                  </div>
                  <Button asChild size="icon" variant="ghost" aria-label={`Download ${f.name}`}>
                    <a href={`${base}/content${query({ file: f.id, download: "true" })}`}>
                      <IconDownload className="size-4" />
                    </a>
                  </Button>
                </li>
              ))}
            </ul>
          )}
          {info && (
            <p className="text-muted-foreground text-xs">
              {info.expires_at ? `Available until ${new Date(info.expires_at).toLocaleString()}.` : ""}
              {info.downloads_left !== null ? ` ${info.downloads_left} download${info.downloads_left === 1 ? "" : "s"} left.` : ""}
            </p>
          )}
        </CardContent>
      </Card>
    </div>
  )
}
