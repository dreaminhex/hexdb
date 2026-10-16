import { useEffect, useState } from "react"
import { IconCheck, IconCopy, IconLink, IconLoader2 } from "@tabler/icons-react"

import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { api, errorMessage, shareAddress, type ShareLink } from "@/lib/api"

const EXPIRY = [
  { value: "1", label: "1 day" },
  { value: "7", label: "7 days" },
  { value: "30", label: "30 days" },
]

/** Make a sharing link to one file or one folder, then show it to copy. */
export function ShareDialog({
  open,
  onOpenChange,
  tess,
  fileId,
  folder,
  name,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  tess: string
  fileId?: string
  folder?: string
  name: string
}) {
  const [days, setDays] = useState("7")
  const [password, setPassword] = useState("")
  const [maxDownloads, setMaxDownloads] = useState("")
  const [note, setNote] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [link, setLink] = useState<ShareLink | null>(null)
  const [copied, setCopied] = useState(false)

  useEffect(() => {
    if (open) {
      setDays("7")
      setPassword("")
      setMaxDownloads("")
      setNote("")
      setError(null)
      setLink(null)
      setCopied(false)
    }
  }, [open])

  const create = async () => {
    setBusy(true)
    setError(null)
    try {
      const made = await api.createShare({
        tessellation: tess,
        ...(fileId ? { file_id: fileId } : { folder: folder ?? "/" }),
        expires_in_days: Number(days),
        ...(password ? { password } : {}),
        ...(maxDownloads ? { max_downloads: Number(maxDownloads) } : {}),
        ...(note.trim() ? { note: note.trim() } : {}),
      })
      setLink(made)
    } catch (e) {
      setError(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  const copy = async () => {
    if (!link) return
    try {
      await navigator.clipboard.writeText(shareAddress(link))
      setCopied(true)
    } catch {
      setCopied(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <IconLink className="size-4" /> Share {fileId ? "file" : "folder"}
          </DialogTitle>
          <DialogDescription>
            Anyone with the link can {fileId ? "download" : "see and download the files in"} <strong className="text-foreground">{name}</strong>, without an
            account, until it expires or you delete it. It shows only what you can see.
          </DialogDescription>
        </DialogHeader>
        {link ? (
          <div className="space-y-3">
            <div className="flex gap-2">
              <Input readOnly value={shareAddress(link)} onFocus={(e) => e.target.select()} aria-label="Link" />
              <Button variant="outline" onClick={() => void copy()} aria-label="Copy the link">
                {copied ? <IconCheck /> : <IconCopy />}
              </Button>
            </div>
            <p className="text-muted-foreground text-xs">
              {link.expires_at ? `Expires ${new Date(link.expires_at).toLocaleString()}.` : "Doesn't expire."}
              {link.password ? " Needs the password you set; send it separately." : ""}
              {link.max_downloads ? ` Allows ${link.max_downloads} download${link.max_downloads === 1 ? "" : "s"}.` : ""} Your links are under Shared
              links.
            </p>
          </div>
        ) : (
          <div className="grid gap-3">
            <div className="grid gap-1.5">
              <Label>Expires after</Label>
              <Select value={days} onValueChange={setDays}>
                <SelectTrigger aria-label="Expires after">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {EXPIRY.map((e) => (
                    <SelectItem key={e.value} value={e.value}>
                      {e.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="grid gap-1.5">
              <Label htmlFor="share-password">Password (optional, 8+ characters)</Label>
              <Input id="share-password" type="password" autoComplete="new-password" value={password} onChange={(e) => setPassword(e.target.value)} />
            </div>
            <div className="grid gap-1.5">
              <Label htmlFor="share-downloads">Downloads allowed (optional)</Label>
              <Input id="share-downloads" type="number" min={1} value={maxDownloads} onChange={(e) => setMaxDownloads(e.target.value)} />
            </div>
            <div className="grid gap-1.5">
              <Label htmlFor="share-note">Note for yourself (optional)</Label>
              <Input id="share-note" maxLength={500} value={note} onChange={(e) => setNote(e.target.value)} />
            </div>
            {error && <p className="text-destructive text-sm">{error}</p>}
          </div>
        )}
        <DialogFooter>
          {link ? (
            <Button onClick={() => onOpenChange(false)}>Done</Button>
          ) : (
            <>
              <Button variant="ghost" onClick={() => onOpenChange(false)}>
                Cancel
              </Button>
              <Button disabled={busy || (!!password && password.length < 8)} onClick={() => void create()}>
                {busy && <IconLoader2 className="animate-spin" />} Create link
              </Button>
            </>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
