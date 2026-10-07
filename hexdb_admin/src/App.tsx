import { lazy, Suspense } from "react"

import { AppSidebar } from "@/components/app-sidebar"
import { SiteHeader } from "@/components/site-header"
import { SidebarInset, SidebarProvider } from "@/components/ui/sidebar"
import { Toaster } from "@/components/ui/sonner"
import { useRoute } from "@/lib/router"
import { LogsPage } from "@/pages/logs"
import { OverviewPage } from "@/pages/overview"
import { PluginsPage } from "@/pages/plugins"
import { RolesPage } from "@/pages/roles"
import { TessellationsPage } from "@/pages/tessellations"
import { UsersPage } from "@/pages/users"

// Pages with the CodeMirror/GraphQL editor load on first visit.
const QueriesPage = lazy(() => import("@/pages/queries").then((m) => ({ default: m.QueriesPage })))
const DocumentsPage = lazy(() => import("@/pages/documents").then((m) => ({ default: m.DocumentsPage })))

const PAGES: Record<string, { title: string; element: React.ComponentType }> = {
  "/": { title: "Overview", element: OverviewPage },
  "/queries": { title: "Query Console", element: QueriesPage },
  "/tessellations": { title: "Tessellations", element: TessellationsPage },
  "/documents": { title: "Documents", element: DocumentsPage },
  "/users": { title: "Users", element: UsersPage },
  "/roles": { title: "Roles", element: RolesPage },
  "/logs": { title: "Logs", element: LogsPage },
  "/plugins": { title: "Plugins", element: PluginsPage },
}

function App() {
  const route = useRoute()
  const page = PAGES[route] ?? PAGES["/"]
  const Page = page.element

  return (
    <SidebarProvider>
      <AppSidebar variant="inset" activeRoute={PAGES[route] ? route : "/"} />
      <SidebarInset>
        <SiteHeader title={page.title} />
        <div className="flex min-h-0 flex-1 flex-col">
          <Suspense fallback={<div className="text-muted-foreground p-6 text-sm">Loading…</div>}>
            <Page />
          </Suspense>
        </div>
      </SidebarInset>
      <Toaster richColors closeButton position="bottom-right" />
    </SidebarProvider>
  )
}

export default App
