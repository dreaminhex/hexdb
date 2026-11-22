import { useState } from "react"
import { IconKey, IconLoader2, IconLock, IconShieldLock } from "@tabler/icons-react"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { api, ApiError, errorMessage } from "@/lib/api"
import { useAuth } from "@/lib/auth"
import { useNotice } from "@/hooks/use-notice"

/** Where the sign-in is: the password, a new one (when the password works once), or a one-time code. */
type Step = "password" | "choose" | "code"

/**
 * Sign-in screen, shown whenever there's no valid session. A password that works
 * once (the first administrator's, or one an administrator handed out) leads to a
 * step choosing a new one; accounts with multi-factor authentication get a step
 * asking for a one-time code.
 */
export function LoginPage() {
  const { refresh } = useAuth()
  const notice = useNotice()
  const [login, setLogin] = useState("")
  const [password, setPassword] = useState("")
  const [newPassword, setNewPassword] = useState("")
  const [confirm, setConfirm] = useState("")
  const [code, setCode] = useState("")
  const [step, setStep] = useState<Step>("password")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()

  const submit = async (event: React.FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    // Read what the fields hold now: a browser's autofill fills them without telling React until the page is touched.
    const form = new FormData(event.currentTarget)
    const field = (name: string, fallback: string) => String(form.get(name) ?? fallback)
    const [user, secret, otp] = [field("login", login), field("password", password), field("code", code)]
    const [chosen, again] = [field("new-password", newPassword), field("confirm-password", confirm)]
    if (step === "choose" && chosen !== again) {
      setError("The two passwords don't match.")
      return
    }
    setLogin(user)
    setPassword(secret)
    setBusy(true)
    setError(undefined)
    try {
      await api.login(user.trim(), secret, step === "code" ? otp.trim() : undefined, step === "password" ? undefined : chosen)
      setPassword("")
      setNewPassword("")
      setConfirm("")
      setCode("")
      await refresh()
    } catch (e) {
      if (e instanceof ApiError && e.code === "password_change_required") {
        setStep("choose")
      } else if (e instanceof ApiError && e.code === "mfa_required") {
        // The password was right; ask for the code (an error only if one was already given).
        if (step === "code") setError(e.message)
        setStep("code")
        setCode("")
      } else {
        setError(errorMessage(e))
      }
    } finally {
      setBusy(false)
    }
  }

  const startOver = () => {
    setStep("password")
    setPassword("")
    setNewPassword("")
    setConfirm("")
    setCode("")
    setError(undefined)
  }

  const title = { password: "Sign in", choose: "Choose a new password", code: "Verification code" }[step]
  const description = {
    password: "Use your HexDB account. Administrators can create accounts on the Users page.",
    choose: `The password for ${login.trim()} works once. Choose your own to finish signing in.`,
    code: `Enter the 6-digit code from your authenticator app for ${login.trim()}, or one of your backup codes.`,
  }[step]
  const Icon = { password: IconLock, choose: IconKey, code: IconShieldLock }[step]

  return (
    <div className="bg-muted/40 flex min-h-svh items-center justify-center p-4">
      <div className="w-full max-w-sm space-y-6">
        <img src="hexdb_lg.png" alt="HexDB" className="mx-auto h-12 w-auto" />
        {notice && (
          <p role="note" className="rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-center text-xs text-amber-900 dark:text-amber-200">
            {notice}
          </p>
        )}
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Icon className="text-muted-foreground size-5" />
              {title}
            </CardTitle>
            <CardDescription>{description}</CardDescription>
          </CardHeader>
          <CardContent>
            <form onSubmit={submit} className="grid gap-4">
              {step === "password" && (
                <>
                  <div className="grid gap-2">
                    <Label htmlFor="login">Login</Label>
                    <Input id="login" name="login" value={login} onChange={(e) => setLogin(e.target.value)} autoComplete="username" autoFocus required />
                  </div>
                  <div className="grid gap-2">
                    <Label htmlFor="password">Password</Label>
                    <Input id="password" name="password" type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="current-password" required />
                  </div>
                </>
              )}
              {step === "choose" && (
                <>
                  <div className="grid gap-2">
                    <Label htmlFor="new-password">New password</Label>
                    <Input
                      id="new-password"
                      name="new-password"
                      type="password"
                      value={newPassword}
                      onChange={(e) => setNewPassword(e.target.value)}
                      autoComplete="new-password"
                      minLength={12}
                      autoFocus
                      required
                    />
                    <p className="text-muted-foreground text-xs">At least 12 characters, not your login, not a common password.</p>
                  </div>
                  <div className="grid gap-2">
                    <Label htmlFor="confirm-password">Confirm it</Label>
                    <Input
                      id="confirm-password"
                      name="confirm-password"
                      type="password"
                      value={confirm}
                      onChange={(e) => setConfirm(e.target.value)}
                      autoComplete="new-password"
                      required
                    />
                  </div>
                </>
              )}
              {step === "code" && (
                <div className="grid gap-2">
                  <Label htmlFor="code">Code</Label>
                  <Input
                    id="code"
                    name="code"
                    value={code}
                    onChange={(e) => setCode(e.target.value)}
                    autoComplete="one-time-code"
                    inputMode="text"
                    placeholder="123456"
                    className="font-mono tracking-widest"
                    autoFocus
                    required
                  />
                </div>
              )}
              {error && (
                <p className="text-destructive text-sm" role="alert">
                  {error}
                </p>
              )}
              <Button type="submit" disabled={busy}>
                {busy && <IconLoader2 className="animate-spin" />}
                {{ password: "Sign in", choose: "Set password and sign in", code: "Verify" }[step]}
              </Button>
              {step !== "password" && (
                <Button type="button" variant="ghost" size="sm" onClick={startOver}>
                  Use a different account
                </Button>
              )}
            </form>
          </CardContent>
        </Card>
      </div>
    </div>
  )
}
