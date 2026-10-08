import { useState } from "react"
import { IconCopy, IconKey, IconLoader2, IconTrash } from "@tabler/icons-react"
import { toast } from "sonner"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { usePoll } from "@/hooks/use-poll"
import { api, errorMessage } from "@/lib/api"
import { useAuth } from "@/lib/auth"
import { formatEpochSeconds } from "@/lib/format"

/** The signed-in user's password and API keys. */
export function AccountPage() {
  const { me } = useAuth()
  if (!me) return null
  return (
    <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
      <Card>
        <CardHeader>
          <CardTitle>{me.login}</CardTitle>
          <CardDescription>{me.email_address}</CardDescription>
        </CardHeader>
        <CardContent className="flex flex-wrap gap-1.5">
          {me.roles.length === 0 && <span className="text-muted-foreground text-sm">No roles: you can sign in but not read any data.</span>}
          {me.roles.map((r) => (
            <Badge key={r.name} variant="secondary" title={r.permissions.join(", ")}>
              {r.name}
              {r.name !== "admin" && r.permissions.length > 0 && <span className="text-muted-foreground ml-1 font-normal">{r.permissions.join(", ")}</span>}
            </Badge>
          ))}
        </CardContent>
      </Card>
      <div className="grid gap-4 lg:grid-cols-2">
        <PasswordCard />
        <ApiKeysCard />
      </div>
    </div>
  )
}

function PasswordCard() {
  const [current, setCurrent] = useState("")
  const [next, setNext] = useState("")
  const [confirm, setConfirm] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()
  const mismatch = confirm.length > 0 && next !== confirm

  const submit = async (event: React.FormEvent) => {
    event.preventDefault()
    if (mismatch) return
    setBusy(true)
    setError(undefined)
    try {
      await api.changePassword(current, next)
      setCurrent("")
      setNext("")
      setConfirm("")
      toast.success("Password changed. Your other sessions were signed out.")
    } catch (e) {
      setError(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Change password</CardTitle>
        <CardDescription>At least 12 characters, not containing your login. Every other session is signed out.</CardDescription>
      </CardHeader>
      <CardContent>
        <form onSubmit={submit} className="grid gap-3">
          <div className="grid gap-1.5">
            <Label htmlFor="current-password">Current password</Label>
            <Input id="current-password" type="password" autoComplete="current-password" value={current} onChange={(e) => setCurrent(e.target.value)} required />
          </div>
          <div className="grid gap-1.5">
            <Label htmlFor="new-password">New password</Label>
            <Input id="new-password" type="password" autoComplete="new-password" minLength={12} value={next} onChange={(e) => setNext(e.target.value)} required />
          </div>
          <div className="grid gap-1.5">
            <Label htmlFor="confirm-password">Confirm new password</Label>
            <Input id="confirm-password" type="password" autoComplete="new-password" value={confirm} onChange={(e) => setConfirm(e.target.value)} required aria-invalid={mismatch} />
            {mismatch && <p className="text-destructive text-xs">The passwords don't match.</p>}
          </div>
          {error && <p className="text-destructive text-sm">{error}</p>}
          <Button type="submit" className="justify-self-start" disabled={busy || !current || next.length < 12 || mismatch}>
            {busy && <IconLoader2 className="animate-spin" />}
            Change password
          </Button>
        </form>
      </CardContent>
    </Card>
  )
}

function ApiKeysCard() {
  const keys = usePoll(api.apiKeys)
  const [name, setName] = useState("")
  const [days, setDays] = useState("90")
  const [busy, setBusy] = useState(false)
  const [created, setCreated] = useState<string>()
  const [error, setError] = useState<string>()

  const create = async (event: React.FormEvent) => {
    event.preventDefault()
    setBusy(true)
    setError(undefined)
    try {
      const expires = Number(days)
      const result = await api.createApiKey(name.trim(), expires > 0 ? expires : undefined)
      setCreated(result.key)
      setName("")
      await keys.refresh()
    } catch (e) {
      setError(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  const revoke = async (id: string, keyName: string) => {
    try {
      await api.revokeApiKey(id)
      toast.success(`Revoked '${keyName}'.`)
      await keys.refresh()
    } catch (e) {
      toast.error(errorMessage(e))
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>API keys</CardTitle>
        <CardDescription>
          For scripts and the CLI (<code>HEXDB_TOKEN</code>). A key acts with your roles. Send it as <code>Authorization: Bearer &lt;key&gt;</code>.
        </CardDescription>
      </CardHeader>
      <CardContent className="grid gap-4">
        {created && (
          <div className="bg-muted/50 grid gap-2 rounded-md border p-3">
            <p className="text-sm font-medium">Copy this key now. It won't be shown again.</p>
            <div className="flex gap-2">
              <Input readOnly value={created} className="font-mono text-xs" onFocus={(e) => e.currentTarget.select()} />
              <Button
                variant="outline"
                size="icon"
                aria-label="Copy key"
                onClick={async () => {
                  await navigator.clipboard.writeText(created)
                  toast.success("Copied.")
                }}
              >
                <IconCopy />
              </Button>
            </div>
            <Button variant="ghost" size="sm" className="justify-self-start" onClick={() => setCreated(undefined)}>
              Done
            </Button>
          </div>
        )}
        <div className="divide-y rounded-md border">
          {keys.data?.length === 0 && <p className="text-muted-foreground p-3 text-sm">No API keys.</p>}
          {keys.data?.map((k) => (
            <div key={k.id} className="flex items-center gap-3 px-3 py-2">
              <IconKey className="text-muted-foreground size-4 shrink-0" />
              <div className="min-w-0 flex-1">
                <div className="truncate text-sm font-medium">{k.name}</div>
                <div className="text-muted-foreground text-xs">
                  created {formatEpochSeconds(k.created)} · {k.expires ? `expires ${formatEpochSeconds(k.expires)}` : "never expires"}
                </div>
              </div>
              <Button variant="ghost" size="icon" className="text-muted-foreground hover:text-destructive size-8" aria-label={`Revoke ${k.name}`} onClick={() => void revoke(k.id, k.name)}>
                <IconTrash />
              </Button>
            </div>
          ))}
        </div>
        <form onSubmit={create} className="flex flex-wrap items-end gap-2">
          <div className="grid min-w-40 flex-1 gap-1.5">
            <Label htmlFor="key-name">Name</Label>
            <Input id="key-name" placeholder="CI deploys" value={name} onChange={(e) => setName(e.target.value)} />
          </div>
          <div className="grid w-28 gap-1.5">
            <Label htmlFor="key-days">Expires (days)</Label>
            <Input id="key-days" type="number" min={0} max={3650} value={days} onChange={(e) => setDays(e.target.value)} title="0 = never" />
          </div>
          <Button type="submit" disabled={busy || !name.trim()}>
            {busy && <IconLoader2 className="animate-spin" />}
            Create key
          </Button>
        </form>
        {error && <p className="text-destructive text-sm">{error}</p>}
      </CardContent>
    </Card>
  )
}
