import { useState } from "react"
import { IconLoader2, IconLock } from "@tabler/icons-react"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { api, errorMessage } from "@/lib/api"
import { useAuth } from "@/lib/auth"

/** Sign-in screen, shown whenever there's no valid session. */
export function LoginPage() {
  const { refresh } = useAuth()
  const [login, setLogin] = useState("")
  const [password, setPassword] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()

  const submit = async (event: React.FormEvent) => {
    event.preventDefault()
    setBusy(true)
    setError(undefined)
    try {
      await api.login(login.trim(), password)
      setPassword("")
      await refresh()
    } catch (e) {
      setError(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="bg-muted/40 flex min-h-svh items-center justify-center p-4">
      <div className="w-full max-w-sm space-y-6">
        <img src="hexdb_lg.png" alt="HexDB" className="mx-auto h-12 w-auto" />
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <IconLock className="text-muted-foreground size-5" /> Sign in
            </CardTitle>
            <CardDescription>Use your HexDB account. Administrators can create accounts on the Users page.</CardDescription>
          </CardHeader>
          <CardContent>
            <form onSubmit={submit} className="grid gap-4">
              <div className="grid gap-2">
                <Label htmlFor="login">Login</Label>
                <Input id="login" value={login} onChange={(e) => setLogin(e.target.value)} autoComplete="username" autoFocus required />
              </div>
              <div className="grid gap-2">
                <Label htmlFor="password">Password</Label>
                <Input
                  id="password"
                  type="password"
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                  autoComplete="current-password"
                  required
                />
              </div>
              {error && (
                <p className="text-destructive text-sm" role="alert">
                  {error}
                </p>
              )}
              <Button type="submit" disabled={busy || !login.trim() || !password}>
                {busy && <IconLoader2 className="animate-spin" />}
                Sign in
              </Button>
            </form>
          </CardContent>
        </Card>
      </div>
    </div>
  )
}
