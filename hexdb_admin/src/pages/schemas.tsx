import { lazy, Suspense } from "react"
import { IconClockPause, IconFileDescription, IconLoader2 } from "@tabler/icons-react"

import { Badge } from "@/components/ui/badge"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { usePoll } from "@/hooks/use-poll"
import { api } from "@/lib/api"
import { setQueryParam, useQueryParam } from "@/lib/router"
import { cn } from "@/lib/utils"

// The editor brings CodeMirror; load it with the page's content, not the app.
const SchemaManager = lazy(() => import("@/components/schemas-dialog").then((m) => ({ default: m.SchemaManager })))

/** Every tessellation's schema in one place: versions, a version saved to apply later, and the editor. */
export function SchemasPage() {
  const tessellations = usePoll(() => api.tessellations(), 5000, [])
  const list = (tessellations.data ?? []).filter((t) => t.kind === "user" || t.kind === "files")
  const chosen = useQueryParam("tessellation") ?? list[0]?.name ?? null

  return (
    <div className="grid gap-4 lg:grid-cols-[16rem_1fr]">
      <Card className="h-fit">
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <IconFileDescription className="size-4" aria-hidden /> Schemas
          </CardTitle>
          <CardDescription>
            Optional per tessellation. Save a new version to apply now, or to apply later: until then, writes follow the current version.
          </CardDescription>
        </CardHeader>
        <CardContent className="px-2">
          {tessellations.error && <p className="text-destructive px-2 text-sm">{tessellations.error.message}</p>}
          {tessellations.data && list.length === 0 && <p className="text-muted-foreground px-2 text-sm">No tessellations yet.</p>}
          <nav aria-label="Tessellations" className="grid gap-0.5">
            {list.map((t) => (
              <button
                key={t.name}
                type="button"
                onClick={() => setQueryParam("tessellation", t.name)}
                aria-current={t.name === chosen ? "page" : undefined}
                className={cn(
                  "hover:bg-accent flex items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm",
                  t.name === chosen && "bg-accent font-medium",
                )}
              >
                <span className="min-w-0 flex-1 truncate">{t.name}</span>
                {t.schema_pending && <IconClockPause className="text-primary size-3.5 shrink-0" aria-label="A version is waiting to be applied" />}
                <Badge variant="outline" className="shrink-0 font-normal">
                  {t.schema_version ? `v${t.schema_version}` : "none"}
                </Badge>
              </button>
            ))}
          </nav>
        </CardContent>
      </Card>
      <Card>
        <CardHeader>
          <CardTitle>{chosen ? `'${chosen}'` : "Schema"}</CardTitle>
          <CardDescription>
            Writes that don't fit the current version are rejected. A new version may rename, copy, remove, default, or convert fields; existing
            documents are migrated in the background when it's applied, and any document is upgraded when it's written.
          </CardDescription>
        </CardHeader>
        <CardContent>
          {chosen ? (
            <Suspense fallback={<IconLoader2 className="text-muted-foreground animate-spin" />}>
              <SchemaManager key={chosen} tessellation={chosen} roomy />
            </Suspense>
          ) : (
            <p className="text-muted-foreground text-sm">Create a tessellation first.</p>
          )}
        </CardContent>
      </Card>
    </div>
  )
}
