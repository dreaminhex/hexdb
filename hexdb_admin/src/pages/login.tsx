import { useState } from "react"
import { IconLoader2, IconLock, IconShieldLock } from "@tabler/icons-react"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { api, ApiError, errorMessage } from "@/lib/api"
import { useAuth } from "@/lib/auth"
import { useNotice } from "@/hooks/use-notice"

/**
 * Sign-in screen, shown whenever there's no valid session. Accounts with
 * multi-factor authentication get a second step asking for a one-time code.
 */
export function LoginPage() {
  const { refresh } = useAuth()
  const notice = useNotice()
  const [login, setLogin] = useState("")
  const [password, setPassword] = useState("")
  const [code, setCode] = useState("")
  const [needsCode, setNeedsCode] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()

  const submit = async (event: React.FormEvent) => {
    event.preventDefault()
    setBusy(true)
    setError(undefined)
    try {
      await api.login(login.trim(), password, needsCode ? code.trim() : undefined)
      setPassword("")
      setCode("")
      await refresh()
    } catch (e) {
      if (e instanceof ApiError && e.code === "mfa_required") {
        // The password was right; ask for the code (an error only if one was already given).
        if (needsCode) setError(e.message)
        setNeedsCode(true)
        setCode("")
      } else {
        setError(errorMessage(e))
      }
    } finally {
      setBusy(false)
    }
  }

  const startOver = () => {
    setNeedsCode(false)
    setPassword("")
    setCode("")
    setError(undefined)
  }

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
              {needsCode ? <IconShieldLock className="text-muted-foreground size-5" /> : <IconLock className="text-muted-foreground size-5" />}
              {needsCode ? "Verification code" : "Sign in"}
            </CardTitle>
            <CardDescription>
              {needsCode
                ? `Enter the 6-digit code from your authenticator app for ${login.trim()}, or one of your backup codes.`
                : "Use your HexDB account. Administrators can create accounts on the Users page."}
            </CardDescription>
          </CardHeader>
          <CardContent>
            <form onSubmit={submit} className="grid gap-4">
              {!needsCode && (
                <>
                  <div className="grid gap-2">
                    <Label htmlFor="login">Login</Label>
                    <Input id="login" value={login} onChange={(e) => setLogin(e.target.value)} autoComplete="username" autoFocus required />
                  </div>
                  <div className="grid gap-2">
                    <Label htmlFor="password">Password</Label>
                    <Input id="password" type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="current-password" required />
                  </div>
                </>
              )}
              {needsCode && (
                <div className="grid gap-2">
                  <Label htmlFor="code">Code</Label>
                  <Input
                    id="code"
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
              <Button type="submit" disabled={busy || !login.trim() || !password || (needsCode && !code.trim())}>
                {busy && <IconLoader2 className="animate-spin" />}
                {needsCode ? "Verify" : "Sign in"}
              </Button>
              {needsCode && (
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
