import { IconShieldCheck } from "@tabler/icons-react"

import { Badge } from "@/components/ui/badge"
import { Card } from "@/components/ui/card"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { usePoll } from "@/hooks/use-poll"
import { api } from "@/lib/api"
import { href, linkHandler } from "@/lib/router"

/** Roles and who holds them. Roles are built in and read-only. */
export function RolesPage() {
  const roles = usePoll(api.roles)
  const users = usePoll(api.users)

  const holders = (role: string) => (users.data ?? []).filter((u) => u.roles.some((g) => g.name === role))

  return (
    <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
      <p className="text-muted-foreground text-sm">
        HexDB's roles are built in. Grant them to users on the{" "}
        <a className="text-foreground underline-offset-4 hover:underline" href={href("/users")} onClick={linkHandler("/users")}>
          Users
        </a>{" "}
        page. Enforcement arrives with authentication.
      </p>
      {(roles.error || users.error) && <p className="text-destructive text-sm">{(roles.error ?? users.error)?.message}</p>}
      <Card className="gap-0 overflow-hidden py-0">
        <Table>
          <TableHeader className="bg-muted/60">
            <TableRow>
              <TableHead className="pl-6">Role</TableHead>
              <TableHead>Description</TableHead>
              <TableHead className="pr-6">Held by</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {(roles.data ?? []).map((role) => {
              const people = holders(role.name)
              return (
                <TableRow key={role.id}>
                  <TableCell className="pl-6 font-medium">
                    <span className="flex items-center gap-1.5">
                      {role.name === "admin" && <IconShieldCheck className="text-primary size-4" />}
                      {role.name}
                    </span>
                  </TableCell>
                  <TableCell className="text-muted-foreground">{role.description}</TableCell>
                  <TableCell className="pr-6">
                    <div className="flex flex-wrap gap-1">
                      {people.length === 0 && <span className="text-muted-foreground text-sm">Nobody</span>}
                      {people.map((u) => (
                        <Badge key={u.id} variant="outline">
                          {u.login}
                        </Badge>
                      ))}
                    </div>
                  </TableCell>
                </TableRow>
              )
            })}
          </TableBody>
        </Table>
      </Card>
    </div>
  )
}
