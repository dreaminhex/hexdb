import { useEffect, useState } from "react"
import { IconLoader2, IconLock, IconPencil, IconPlus, IconShieldCheck, IconShieldLock, IconTrash } from "@tabler/icons-react"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/confirm-dialog"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Checkbox } from "@/components/ui/checkbox"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Textarea } from "@/components/ui/textarea"
import { Label } from "@/components/ui/label"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { usePoll } from "@/hooks/use-poll"
import { api, errorMessage, type Role, type RoleGrant, type User, type UserChanges } from "@/lib/api"
import { formatEpochSeconds } from "@/lib/format"

// ---------------------------------------------------------------------------
// Role grants editor
// ---------------------------------------------------------------------------

type GrantDraft = Record<string, { checked: boolean; tessellations: string }>

function draftFrom(roles: Role[], grants: RoleGrant[]): GrantDraft {
  return Object.fromEntries(
    roles.map((role) => {
      const grant = grants.find((g) => g.name === role.name)
      return [role.name, { checked: !!grant, tessellations: grant?.tessellations.join(", ") ?? "" }]
    }),
  )
}

function grantsFrom(draft: GrantDraft): RoleGrant[] {
  return Object.entries(draft)
    .filter(([, g]) => g.checked)
    .map(([name, g]) => ({
      name,
      tessellations: g.tessellations.split(",").map((p) => p.trim()).filter(Boolean),
    }))
}

/** True if a role has permissions that apply per tessellation (so a grant needs a tessellation list). */
const scoped = (role: Role) => role.permissions.some((p) => p === "read" || p === "write" || p === "manage")

function RoleGrantsEditor({ roles, draft, onChange }: { roles: Role[]; draft: GrantDraft; onChange: (draft: GrantDraft) => void }) {
  return (
    <div className="grid gap-2">
      <Label>Roles</Label>
      <div className="divide-y rounded-md border">
        {roles.map((role) => {
          const entry = draft[role.name] ?? { checked: false, tessellations: "" }
          const id = `role-${role.name}`
          return (
            <div key={role.name} className="grid gap-2 p-3">
              <div className="flex items-start gap-2">
                <Checkbox
                  id={id}
                  checked={entry.checked}
                  onCheckedChange={(checked) => onChange({ ...draft, [role.name]: { ...entry, checked: checked === true } })}
                  className="mt-0.5"
                />
                <label htmlFor={id} className="grid gap-0.5 text-sm leading-tight">
                  <span className="font-medium">
                    {role.name}
                    <span className="text-muted-foreground ml-2 font-mono text-xs font-normal">{role.permissions.join(", ")}</span>
                  </span>
                  <span className="text-muted-foreground text-xs">{role.description}</span>
                </label>
              </div>
              {entry.checked && scoped(role) && (
                <Input
                  value={entry.tessellations}
                  onChange={(e) => onChange({ ...draft, [role.name]: { ...entry, tessellations: e.target.value } })}
                  placeholder="Tessellations, comma-separated, or * for all"
                  className="h-8 font-mono text-xs"
                  aria-label={`${role.name} tessellations`}
                />
              )}
            </div>
          )
        })}
      </div>
      <p className="text-muted-foreground text-xs">Changes take effect on the user's next request. Locking a user or resetting their password signs them out everywhere.</p>
    </div>
  )
}

// ---------------------------------------------------------------------------
// Create / edit dialog
// ---------------------------------------------------------------------------

function UserDialog({
  user,
  roles,
  open,
  onOpenChange,
  onSaved,
}: {
  /** null = new user. */
  user: User | null
  roles: Role[]
  open: boolean
  onOpenChange: (open: boolean) => void
  onSaved: () => void
}) {
  const [login, setLogin] = useState("")
  const [email, setEmail] = useState("")
  const [password, setPassword] = useState("")
  const [locked, setLocked] = useState(false)
  const [resetMfa, setResetMfa] = useState(false)
  const [attributes, setAttributes] = useState("")
  const [draft, setDraft] = useState<GrantDraft>({})
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (!open) return
    setLogin(user?.login ?? "")
    setEmail(user?.email_address ?? "")
    setPassword("")
    setLocked(user?.is_locked ?? false)
    setResetMfa(false)
    setAttributes(user?.attributes && Object.keys(user.attributes).length ? JSON.stringify(user.attributes, null, 2) : "")
    setDraft(draftFrom(roles, user?.roles ?? []))
    setError(null)
  }, [open, user, roles])

  const submit = async (event: React.FormEvent) => {
    event.preventDefault()
    setBusy(true)
    setError(null)
    try {
      let attrs: Record<string, unknown> = {}
      if (attributes.trim()) {
        try {
          attrs = JSON.parse(attributes)
        } catch (e) {
          throw new Error(`Attributes aren't valid JSON: ${e instanceof Error ? e.message : String(e)}`)
        }
      }
      if (user) {
        const changes: UserChanges = {}
        if (JSON.stringify(attrs) !== JSON.stringify(user.attributes ?? {})) changes.attributes = attrs
        if (login !== user.login) changes.login = login
        if (email !== user.email_address) changes.email_address = email
        if (password) changes.password = password
        if (locked !== user.is_locked) changes.is_locked = locked
        if (resetMfa) changes.use_mfa = false
        const grants = grantsFrom(draft)
        if (JSON.stringify(grants) !== JSON.stringify(user.roles)) changes.roles = grants
        if (Object.keys(changes).length > 0) {
          await api.updateUser(user.id, changes)
          toast.success(`Updated ${login}.`)
        }
      } else {
        await api.createUser({ login, email_address: email, password, roles: grantsFrom(draft), attributes: attrs })
        toast.success(`Created ${login}.`)
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
    <Dialog open={open} onOpenChange={(next) => !busy && onOpenChange(next)}>
      <DialogContent className="max-h-[90svh] overflow-y-auto sm:max-w-lg">
        <form onSubmit={submit} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>{user ? `Edit ${user.login}` : "New user"}</DialogTitle>
            <DialogDescription>Passwords are hashed with Argon2 and never shown again.</DialogDescription>
          </DialogHeader>
          <div className="grid gap-2">
            <Label htmlFor="user-login">Login</Label>
            <Input id="user-login" value={login} onChange={(e) => setLogin(e.target.value)} autoComplete="off" required autoFocus={!user} />
          </div>
          <div className="grid gap-2">
            <Label htmlFor="user-email">Email</Label>
            <Input id="user-email" type="email" value={email} onChange={(e) => setEmail(e.target.value)} autoComplete="off" required />
          </div>
          <div className="grid gap-2">
            <Label htmlFor="user-password">{user ? "New password (optional)" : "Password"}</Label>
            <Input
              id="user-password"
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              autoComplete="new-password"
              required={!user}
              minLength={12}
              placeholder={user ? "Leave blank to keep the current password" : "At least 12 characters, not containing the login"}
            />
          </div>
          {user && (
            <div className="flex items-center gap-2">
              <Checkbox id="user-locked" checked={locked} onCheckedChange={(checked) => setLocked(checked === true)} />
              <Label htmlFor="user-locked">Locked (can't sign in)</Label>
            </div>
          )}
          {user?.use_mfa && (
            <div className="flex items-start gap-2">
              <Checkbox id="user-mfa" checked={resetMfa} onCheckedChange={(checked) => setResetMfa(checked === true)} className="mt-0.5" />
              <Label htmlFor="user-mfa" className="grid gap-0.5 font-normal">
                <span className="font-medium">Reset multi-factor authentication</span>
                <span className="text-muted-foreground text-xs">For a user who lost their authenticator and backup codes. They can set it up again from their Account page.</span>
              </Label>
            </div>
          )}
          <RoleGrantsEditor roles={roles} draft={draft} onChange={setDraft} />
          <div className="grid gap-2">
            <Label htmlFor="user-attributes">Attributes (optional)</Label>
            <Textarea
              id="user-attributes"
              value={attributes}
              onChange={(e) => setAttributes(e.target.value)}
              className="min-h-16 font-mono text-xs"
              spellCheck={false}
              placeholder={'{ "region": "EU" }'}
            />
            <p className="text-muted-foreground text-xs">
              Values role filters can refer to, e.g. <code>{'{"$user": "attributes.region"}'}</code>. A filter whose attribute is missing matches nothing.
            </p>
          </div>
          {error && <p className="text-destructive text-sm">{error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)} disabled={busy}>
              Cancel
            </Button>
            <Button type="submit" disabled={busy}>
              {busy && <IconLoader2 className="animate-spin" />}
              {user ? "Save" : "Create user"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

export function UsersPage() {
  const users = usePoll(api.users)
  const roles = usePoll(api.roles)
  const [editing, setEditing] = useState<User | null | undefined>(undefined)
  const [deleting, setDeleting] = useState<User | null>(null)

  const rows = [...(users.data ?? [])].sort((a, b) => a.login.localeCompare(b.login))

  return (
    <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
      <div className="flex items-center gap-2">
        <p className="text-muted-foreground text-sm">{users.data ? `${rows.length} user${rows.length === 1 ? "" : "s"}` : "Loading…"}</p>
        <Button className="ml-auto" size="sm" onClick={() => setEditing(null)} disabled={!roles.data}>
          <IconPlus /> New user
        </Button>
      </div>
      {(users.error || roles.error) && <p className="text-destructive text-sm">{(users.error ?? roles.error)?.message}</p>}

      <Card className="gap-0 overflow-hidden py-0">
        <Table>
          <TableHeader className="bg-muted/60">
            <TableRow>
              <TableHead className="pl-6">Login</TableHead>
              <TableHead>Email</TableHead>
              <TableHead>Roles</TableHead>
              <TableHead>Status</TableHead>
              <TableHead>Created</TableHead>
              <TableHead>Last login</TableHead>
              <TableHead className="w-24 pr-6" />
            </TableRow>
          </TableHeader>
          <TableBody>
            {rows.map((user) => (
              <TableRow key={user.id}>
                <TableCell className="pl-6 font-medium">{user.login}</TableCell>
                <TableCell className="text-muted-foreground">{user.email_address}</TableCell>
                <TableCell>
                  <div className="flex flex-wrap gap-1">
                    {user.roles.length === 0 && <span className="text-muted-foreground text-sm">None</span>}
                    {user.roles.map((grant) => (
                      <Badge key={grant.name} variant="outline" title={grant.tessellations.join(", ") || "No tessellations"}>
                        {grant.name === "admin" && <IconShieldCheck className="size-3" />}
                        {grant.name}
                        {grant.name !== "admin" && grant.tessellations.length > 0 && (
                          <span className="text-muted-foreground font-mono font-normal">: {grant.tessellations.join(", ")}</span>
                        )}
                      </Badge>
                    ))}
                  </div>
                </TableCell>
                <TableCell>
                  <div className="flex flex-wrap gap-1">
                    {user.is_locked ? (
                      <Badge variant="secondary" className="gap-1">
                        <IconLock className="size-3" /> Locked
                      </Badge>
                    ) : (
                      <Badge variant="outline">Active</Badge>
                    )}
                    {user.use_mfa && (
                      <Badge variant="outline" className="gap-1" title="Multi-factor authentication is on">
                        <IconShieldLock className="size-3" /> MFA
                      </Badge>
                    )}
                  </div>
                </TableCell>
                <TableCell className="text-muted-foreground text-sm">{formatEpochSeconds(user.created)}</TableCell>
                <TableCell className="text-muted-foreground text-sm">{formatEpochSeconds(user.last_login)}</TableCell>
                <TableCell className="pr-6">
                  <div className="flex justify-end gap-1">
                    <Button variant="ghost" size="icon" className="text-muted-foreground size-8" aria-label={`Edit ${user.login}`} onClick={() => setEditing(user)} disabled={!roles.data}>
                      <IconPencil />
                    </Button>
                    <Button variant="ghost" size="icon" className="text-muted-foreground hover:text-destructive size-8" aria-label={`Delete ${user.login}`} onClick={() => setDeleting(user)}>
                      <IconTrash />
                    </Button>
                  </div>
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
      </Card>

      {roles.data && (
        <UserDialog
          user={editing ?? null}
          roles={roles.data}
          open={editing !== undefined}
          onOpenChange={(open) => !open && setEditing(undefined)}
          onSaved={() => void users.refresh()}
        />
      )}
      <ConfirmDialog
        open={deleting !== null}
        onOpenChange={(open) => !open && setDeleting(null)}
        title={`Delete ${deleting?.login}?`}
        description="The account is removed permanently."
        confirmLabel="Delete user"
        onConfirm={async () => {
          await api.deleteUser(deleting!.id)
          toast.success(`Deleted ${deleting!.login}.`)
          void users.refresh()
        }}
      />
    </div>
  )
}
