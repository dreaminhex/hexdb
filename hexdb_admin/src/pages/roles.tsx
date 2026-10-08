import { useEffect, useState } from "react"
import { IconLoader2, IconLock, IconPencil, IconPlus, IconShieldCheck, IconTrash } from "@tabler/icons-react"
import { toast } from "sonner"

import { ConfirmDialog } from "@/components/confirm-dialog"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Checkbox } from "@/components/ui/checkbox"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Textarea } from "@/components/ui/textarea"
import { Label } from "@/components/ui/label"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { usePoll } from "@/hooks/use-poll"
import { api, errorMessage, type Action, type PermissionInfo, type Role, type RoleInput } from "@/lib/api"
import { href, linkHandler } from "@/lib/router"

function RoleDialog({
  role,
  catalog,
  open,
  onOpenChange,
  onSaved,
}: {
  /** null = new role. */
  role: Role | null
  catalog: PermissionInfo[]
  open: boolean
  onOpenChange: (open: boolean) => void
  onSaved: () => void
}) {
  const [name, setName] = useState("")
  const [description, setDescription] = useState("")
  const [permissions, setPermissions] = useState<Action[]>([])
  const [restrictions, setRestrictions] = useState("")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string>()

  useEffect(() => {
    if (!open) return
    setName(role?.name ?? "")
    setDescription(role?.description ?? "")
    setPermissions(role?.permissions ?? [])
    setRestrictions(role?.restrictions && Object.keys(role.restrictions).length ? JSON.stringify(role.restrictions, null, 2) : "")
    setError(undefined)
  }, [open, role])

  const toggle = (action: Action, on: boolean) => setPermissions((current) => (on ? [...current, action] : current.filter((a) => a !== action)))

  const submit = async (event: React.FormEvent) => {
    event.preventDefault()
    setBusy(true)
    setError(undefined)
    try {
      let parsed: RoleInput["restrictions"] = {}
      if (restrictions.trim()) {
        try {
          parsed = JSON.parse(restrictions)
        } catch (e) {
          throw new Error(`Restrictions aren't valid JSON: ${e instanceof Error ? e.message : String(e)}`)
        }
      }
      if (role) {
        await api.updateRole(role.name, { description, permissions, restrictions: parsed })
        toast.success(`Updated ${role.name}.`)
      } else {
        await api.createRole({ name: name.trim(), description, permissions, restrictions: parsed })
        toast.success(`Created ${name.trim()}.`)
      }
      onSaved()
      onOpenChange(false)
    } catch (e) {
      setError(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  const group = (scoped: boolean) => catalog.filter((p) => p.scoped === scoped)

  return (
    <Dialog open={open} onOpenChange={(next) => !busy && onOpenChange(next)}>
      <DialogContent className="max-h-[90svh] overflow-y-auto sm:max-w-lg">
        <form onSubmit={submit} className="grid gap-4">
          <DialogHeader>
            <DialogTitle>{role ? `Edit ${role.name}` : "New role"}</DialogTitle>
            <DialogDescription>A role is a set of permissions. Grant it to users on the Users page, choosing the tessellations it applies to.</DialogDescription>
          </DialogHeader>
          {!role && (
            <div className="grid gap-2">
              <Label htmlFor="role-name">Name</Label>
              <Input id="role-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="e.g. analyst" pattern="[A-Za-z0-9_\-]{1,64}" required autoFocus />
            </div>
          )}
          <div className="grid gap-2">
            <Label htmlFor="role-description">Description</Label>
            <Input id="role-description" value={description} onChange={(e) => setDescription(e.target.value)} maxLength={500} />
          </div>
          {[true, false].map((scoped) => (
            <div key={String(scoped)} className="grid gap-2">
              <Label>{scoped ? "On the granted tessellations" : "Everywhere"}</Label>
              <div className="divide-y rounded-md border">
                {group(scoped).map((p) => {
                  const id = `perm-${p.name}`
                  return (
                    <div key={p.name} className="flex items-start gap-2 p-2.5">
                      <Checkbox id={id} checked={permissions.includes(p.name)} onCheckedChange={(on) => toggle(p.name, on === true)} className="mt-0.5" />
                      <label htmlFor={id} className="grid gap-0.5 text-sm leading-tight">
                        <span className="font-medium">{p.name}</span>
                        <span className="text-muted-foreground text-xs">{p.description}</span>
                      </label>
                    </div>
                  )
                })}
              </div>
            </div>
          ))}
          <div className="grid gap-2">
            <Label htmlFor="role-restrictions">Restrictions (optional)</Label>
            <Textarea
              id="role-restrictions"
              value={restrictions}
              onChange={(e) => setRestrictions(e.target.value)}
              className="min-h-28 font-mono text-xs"
              spellCheck={false}
              placeholder={'{\n  "orders": {\n    "filter": { "region": { "$user": "attributes.region" } },\n    "hide": ["cost"]\n  }\n}'}
            />
            <p className="text-muted-foreground text-xs">
              Per tessellation (or <code>*</code>): <code>filter</code> limits the documents this role can see and write, <code>hide</code> lists fields it
              can't see or change. <code>{'{"$user": "login"}'}</code> or <code>{'{"$user": "attributes.region"}'}</code> stand for the user's values
              (set attributes on the Users page). Restricted grants can't manage tessellations.
            </p>
          </div>
          {error && <p className="text-destructive text-sm whitespace-pre-wrap">{error}</p>}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)} disabled={busy}>
              Cancel
            </Button>
            <Button type="submit" disabled={busy || permissions.length === 0 || (!role && !name.trim())}>
              {busy && <IconLoader2 className="animate-spin" />}
              {role ? "Save" : "Create role"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

/** Roles (permission sets) and who holds them. Built-in roles are fixed; custom roles can be edited. */
export function RolesPage() {
  const catalog = usePoll(api.roleCatalog)
  const users = usePoll(api.users)
  const [editing, setEditing] = useState<Role | null | undefined>(undefined)
  const [deleting, setDeleting] = useState<Role | null>(null)

  const holders = (role: string) => (users.data ?? []).filter((u) => u.roles.some((g) => g.name === role))
  const roles = [...(catalog.data?.roles ?? [])].sort((a, b) => Number(b.builtin) - Number(a.builtin) || a.name.localeCompare(b.name))

  return (
    <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
      <div className="flex items-start gap-4">
        <p className="text-muted-foreground max-w-3xl text-sm">
          A role is a named set of permissions. Grant roles on the{" "}
          <a className="text-foreground underline-offset-4 hover:underline" href={href("/users")} onClick={linkHandler("/users")}>
            Users
          </a>{" "}
          page; each grant lists the tessellations its read, write and manage permissions apply to (<code>*</code> for all). The other
          permissions apply everywhere. Changes take effect on the next request.
        </p>
        <Button className="ml-auto shrink-0" size="sm" onClick={() => setEditing(null)} disabled={!catalog.data}>
          <IconPlus /> New role
        </Button>
      </div>
      {(catalog.error || users.error) && <p className="text-destructive text-sm">{(catalog.error ?? users.error)?.message}</p>}
      <Card className="gap-0 overflow-hidden py-0">
        <Table>
          <TableHeader className="bg-muted/60">
            <TableRow>
              <TableHead className="pl-6">Role</TableHead>
              <TableHead>Permissions</TableHead>
              <TableHead>Held by</TableHead>
              <TableHead className="w-24 pr-6" />
            </TableRow>
          </TableHeader>
          <TableBody>
            {roles.map((role) => {
              const people = holders(role.name)
              return (
                <TableRow key={role.id}>
                  <TableCell className="pl-6 align-top">
                    <div className="flex items-center gap-1.5 font-medium">
                      {role.name === "admin" && <IconShieldCheck className="text-primary size-4" />}
                      {role.name}
                      {role.builtin && <IconLock className="text-muted-foreground size-3.5" aria-label="Built in" />}
                    </div>
                    <div className="text-muted-foreground max-w-xs text-xs whitespace-normal">{role.description}</div>
                  </TableCell>
                  <TableCell className="align-top">
                    <div className="flex flex-wrap gap-1">
                      {role.permissions.map((p) => (
                        <Badge key={p} variant={p === "admin" ? "default" : "secondary"} className="font-mono font-normal">
                          {p}
                        </Badge>
                      ))}
                    </div>
                  </TableCell>
                  <TableCell className="align-top">
                    <div className="flex flex-wrap gap-1">
                      {people.length === 0 && <span className="text-muted-foreground text-sm">Nobody</span>}
                      {people.map((u) => (
                        <Badge key={u.id} variant="outline">
                          {u.login}
                        </Badge>
                      ))}
                    </div>
                  </TableCell>
                  <TableCell className="pr-6 align-top">
                    {!role.builtin && (
                      <div className="flex justify-end gap-1">
                        <Button variant="ghost" size="icon" className="text-muted-foreground size-8" aria-label={`Edit ${role.name}`} onClick={() => setEditing(role)}>
                          <IconPencil />
                        </Button>
                        <Button
                          variant="ghost"
                          size="icon"
                          className="text-muted-foreground hover:text-destructive size-8"
                          aria-label={`Delete ${role.name}`}
                          onClick={() => setDeleting(role)}
                          disabled={people.length > 0}
                          title={people.length > 0 ? "Remove it from its users first" : undefined}
                        >
                          <IconTrash />
                        </Button>
                      </div>
                    )}
                  </TableCell>
                </TableRow>
              )
            })}
          </TableBody>
        </Table>
      </Card>
      {catalog.data && (
        <RoleDialog
          role={editing ?? null}
          catalog={catalog.data.permissions}
          open={editing !== undefined}
          onOpenChange={(open) => !open && setEditing(undefined)}
          onSaved={() => void catalog.refresh()}
        />
      )}
      <ConfirmDialog
        open={deleting !== null}
        onOpenChange={(open) => !open && setDeleting(null)}
        title={`Delete the role ${deleting?.name}?`}
        description="Nobody holds it, so nothing else changes."
        confirmLabel="Delete role"
        onConfirm={async () => {
          await api.deleteRole(deleting!.name)
          toast.success(`Deleted ${deleting!.name}.`)
          void catalog.refresh()
        }}
      />
    </div>
  )
}
