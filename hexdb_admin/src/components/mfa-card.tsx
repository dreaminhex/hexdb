import { useState } from "react"
import { IconCopy, IconLoader2, IconShieldCheck, IconShieldOff } from "@tabler/icons-react"
import { toast } from "sonner"

import { QrCode } from "@/components/qr-code"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { usePoll } from "@/hooks/use-poll"
import { api, errorMessage } from "@/lib/api"
import { useAuth } from "@/lib/auth"

type Step =
  | { kind: "idle" }
  | { kind: "password" }
  | { kind: "scan"; secret: string; uri: string }
  | { kind: "codes"; codes: string[] }
  | { kind: "confirm"; action: "disable" | "regenerate" }

/** Turn multi-factor authentication (TOTP) on or off for the signed-in user. */
export function MfaCard() {
  const { refresh } = useAuth()
  const status = usePoll(api.mfa)
  const [step, setStep] = useState<Step>({ kind: "idle" })
  const [password, setPassword] = useState("")
  const [code, setCode] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()

  const run = async (work: () => Promise<void>) => {
    setBusy(true)
    setError(undefined)
    try {
      await work()
    } catch (e) {
      setError(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }
  const reset = () => {
    setStep({ kind: "idle" })
    setPassword("")
    setCode("")
    setError(undefined)
  }

  const enabled = status.data?.enabled ?? false

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          Multi-factor authentication
          {status.data && (enabled ? <Badge variant="secondary">On</Badge> : <Badge variant="outline">Off</Badge>)}
        </CardTitle>
        <CardDescription>
          Signing in then also needs a code from an authenticator app (Google Authenticator, 1Password, Authy, ...). API keys aren't affected.
        </CardDescription>
      </CardHeader>
      <CardContent className="grid gap-4">
        {step.kind === "idle" && (
          <div className="flex flex-wrap items-center gap-2">
            {!enabled && (
              <Button onClick={() => setStep({ kind: "password" })} disabled={!status.data}>
                <IconShieldCheck /> Set up
              </Button>
            )}
            {enabled && (
              <>
                <span className="text-muted-foreground text-sm">{status.data?.backup_codes_left} backup codes left.</span>
                <Button variant="outline" size="sm" onClick={() => setStep({ kind: "confirm", action: "regenerate" })}>
                  New backup codes
                </Button>
                <Button variant="outline" size="sm" onClick={() => setStep({ kind: "confirm", action: "disable" })}>
                  <IconShieldOff /> Turn off
                </Button>
              </>
            )}
          </div>
        )}

        {step.kind === "password" && (
          <form
            className="grid gap-3"
            onSubmit={(e) => {
              e.preventDefault()
              void run(async () => {
                const setup = await api.mfaSetup(password)
                setPassword("")
                setStep({ kind: "scan", secret: setup.secret, uri: setup.otpauth_uri })
              })
            }}
          >
            <div className="grid gap-1.5">
              <Label htmlFor="mfa-password">Current password</Label>
              <Input id="mfa-password" type="password" autoComplete="current-password" value={password} onChange={(e) => setPassword(e.target.value)} autoFocus required />
            </div>
            <div className="flex gap-2">
              <Button type="submit" disabled={busy || !password}>
                {busy && <IconLoader2 className="animate-spin" />}
                Continue
              </Button>
              <Button type="button" variant="ghost" onClick={reset}>
                Cancel
              </Button>
            </div>
          </form>
        )}

        {step.kind === "scan" && (
          <form
            className="grid gap-3"
            onSubmit={(e) => {
              e.preventDefault()
              void run(async () => {
                const result = await api.mfaEnable(code.trim())
                setCode("")
                setStep({ kind: "codes", codes: result.backup_codes })
                await status.refresh()
                // Other sessions were signed out; this one has a new cookie.
                await refresh()
              })
            }}
          >
            <p className="text-sm">Scan this with your authenticator app, then enter the code it shows.</p>
            <div className="flex flex-wrap items-center gap-4">
              <QrCode value={step.uri} label="QR code for your authenticator app" />
              <div className="grid gap-1 text-xs">
                <span className="text-muted-foreground">Or enter this key:</span>
                <code className="bg-muted rounded px-2 py-1 font-mono break-all">{step.secret.match(/.{1,4}/g)?.join(" ")}</code>
              </div>
            </div>
            <div className="grid max-w-xs gap-1.5">
              <Label htmlFor="mfa-code">Code</Label>
              <Input id="mfa-code" autoComplete="one-time-code" inputMode="numeric" placeholder="123456" className="font-mono tracking-widest" value={code} onChange={(e) => setCode(e.target.value)} required />
            </div>
            <div className="flex gap-2">
              <Button type="submit" disabled={busy || code.trim().length < 6}>
                {busy && <IconLoader2 className="animate-spin" />}
                Turn on
              </Button>
              <Button type="button" variant="ghost" onClick={reset}>
                Cancel
              </Button>
            </div>
          </form>
        )}

        {step.kind === "codes" && (
          <div className="grid gap-3">
            <p className="text-sm font-medium">Save these backup codes somewhere safe. Each works once, if you lose your authenticator. They won't be shown again.</p>
            <div className="bg-muted/50 grid grid-cols-2 gap-1 rounded-md border p-3 font-mono text-sm">
              {step.codes.map((c) => (
                <span key={c}>{c}</span>
              ))}
            </div>
            <div className="flex gap-2">
              <Button
                variant="outline"
                onClick={async () => {
                  await navigator.clipboard.writeText(step.codes.join("\n"))
                  toast.success("Copied.")
                }}
              >
                <IconCopy /> Copy
              </Button>
              <Button onClick={reset}>Done</Button>
            </div>
          </div>
        )}

        {step.kind === "confirm" && (
          <form
            className="grid gap-3"
            onSubmit={(e) => {
              e.preventDefault()
              void run(async () => {
                if (step.action === "disable") {
                  await api.mfaDisable(password, code.trim())
                  toast.success("Multi-factor authentication is off.")
                  reset()
                } else {
                  const result = await api.mfaBackupCodes(password, code.trim())
                  setPassword("")
                  setCode("")
                  setStep({ kind: "codes", codes: result.backup_codes })
                }
                await status.refresh()
              })
            }}
          >
            <p className="text-sm">{step.action === "disable" ? "Confirm with your password and a current code." : "New codes replace the old ones. Confirm with your password and a current code."}</p>
            <div className="grid gap-3 sm:grid-cols-2">
              <div className="grid gap-1.5">
                <Label htmlFor="mfa-confirm-password">Password</Label>
                <Input id="mfa-confirm-password" type="password" autoComplete="current-password" value={password} onChange={(e) => setPassword(e.target.value)} required />
              </div>
              <div className="grid gap-1.5">
                <Label htmlFor="mfa-confirm-code">Code</Label>
                <Input id="mfa-confirm-code" autoComplete="one-time-code" className="font-mono" value={code} onChange={(e) => setCode(e.target.value)} required />
              </div>
            </div>
            <div className="flex gap-2">
              <Button type="submit" variant={step.action === "disable" ? "destructive" : "default"} disabled={busy || !password || !code.trim()}>
                {busy && <IconLoader2 className="animate-spin" />}
                {step.action === "disable" ? "Turn off" : "Replace backup codes"}
              </Button>
              <Button type="button" variant="ghost" onClick={reset}>
                Cancel
              </Button>
            </div>
          </form>
        )}

        {error && <p className="text-destructive text-sm">{error}</p>}
      </CardContent>
    </Card>
  )
}
