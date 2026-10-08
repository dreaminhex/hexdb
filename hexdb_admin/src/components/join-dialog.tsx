import { useEffect, useState } from "react"
import { IconCopy, IconLoader2 } from "@tabler/icons-react"
import { toast } from "sonner"

import { Button } from "@/components/ui/button"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { api, errorMessage, type JoinInfo } from "@/lib/api"

function Snippet({ title, text }: { title: string; text: string }) {
  return (
    <div className="grid gap-1.5">
      <div className="flex items-center justify-between">
        <Label>{title}</Label>
        <Button
          variant="ghost"
          size="sm"
          className="text-muted-foreground h-7"
          onClick={() => void navigator.clipboard.writeText(text).then(() => toast.success(`Copied ${title}.`))}
        >
          <IconCopy /> Copy
        </Button>
      </div>
      <pre className="bg-muted/50 max-h-56 overflow-auto rounded-md border p-3 text-xs">{text}</pre>
    </div>
  )
}

/** What a new hex needs to join this lattice. The snippets include the lattice secret, so they need the password again. */
export function JoinDialog({ open, onOpenChange }: { open: boolean; onOpenChange: (open: boolean) => void }) {
  const [password, setPassword] = useState("")
  const [info, setInfo] = useState<JoinInfo>()
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()

  useEffect(() => {
    if (!open) {
      setPassword("")
      setInfo(undefined)
      setError(undefined)
    }
  }, [open])

  const submit = async (event: React.FormEvent) => {
    event.preventDefault()
    setBusy(true)
    setError(undefined)
    try {
      setInfo(await api.joinInfo(password))
      setPassword("")
    } catch (e) {
      setError(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-h-[94svh] overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>Add a hex</DialogTitle>
          <DialogDescription>
            A new hex joins when it can reach a seed and shares the lattice secret and encryption key. On one machine,{" "}
            <code>hexdb lattice spawn</code> does all of this for you.
          </DialogDescription>
        </DialogHeader>
        {info ? (
          <div className="grid gap-4">
            <Snippet title="hexdb.toml" text={info.hexdb_toml} />
            <Snippet title="hexdb.local.toml (secret)" text={info.hexdb_local_toml} />
            <ul className="text-muted-foreground list-disc space-y-1 pl-5 text-sm">
              {info.notes.map((note) => (
                <li key={note}>{note}</li>
              ))}
            </ul>
          </div>
        ) : (
          <form onSubmit={submit} className="grid gap-3">
            <div className="grid gap-1.5">
              <Label htmlFor="join-password">Your password</Label>
              <Input id="join-password" type="password" autoComplete="current-password" value={password} onChange={(e) => setPassword(e.target.value)} />
              <p className="text-muted-foreground text-xs">The settings include the lattice secret. Keep them out of version control.</p>
            </div>
            {error && <p className="text-destructive text-sm">{error}</p>}
            <DialogFooter>
              <Button type="submit" disabled={busy || !password}>
                {busy && <IconLoader2 className="animate-spin" />}
                Show settings
              </Button>
            </DialogFooter>
          </form>
        )}
      </DialogContent>
    </Dialog>
  )
}
