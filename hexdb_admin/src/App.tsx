import { lazy, Suspense } from "react"

import { AppSidebar } from "@/components/app-sidebar"
import { SiteHeader } from "@/components/site-header"
import { SidebarInset, SidebarProvider } from "@/components/ui/sidebar"
import { Toaster } from "@/components/ui/sonner"
import { AuthProvider, useAuth } from "@/lib/auth"
import { useRoute } from "@/lib/router"
import { AccountPage } from "@/pages/account"
import { LoginPage } from "@/pages/login"
import { LogsPage } from "@/pages/logs"
import { OverviewPage } from "@/pages/overview"
import { PluginsPage } from "@/pages/plugins"
import { RolesPage } from "@/pages/roles"
import { TessellationsPage } from "@/pages/tessellations"
import { UsersPage } from "@/pages/users"

// Pages with the CodeMirror/GraphQL editor load on first visit.
const QueriesPage = lazy(() => import("@/pages/queries").then((m) => ({ default: m.QueriesPage })))
const DocumentsPage = lazy(() => import("@/pages/documents").then((m) => ({ default: m.DocumentsPage })))

interface Page {
  title: string
  element: React.ComponentType
  /** Only administrators see it (the server enforces this too). */
  adminOnly?: boolean
}

const PAGES: Record<string, Page> = {
  "/": { title: "Overview", element: OverviewPage, adminOnly: true },
  "/queries": { title: "Query Console", element: QueriesPage },
  "/tessellations": { title: "Tessellations", element: TessellationsPage },
  "/documents": { title: "Documents", element: DocumentsPage },
  "/users": { title: "Users", element: UsersPage, adminOnly: true },
  "/roles": { title: "Roles", element: RolesPage, adminOnly: true },
  "/logs": { title: "Logs", element: LogsPage, adminOnly: true },
  "/plugins": { title: "Plugins", element: PluginsPage, adminOnly: true },
  "/account": { title: "Account", element: AccountPage },
}

function Shell() {
  const { me } = useAuth()
  const route = useRoute()

  if (me === undefined) {
    return <div className="text-muted-foreground flex min-h-svh items-center justify-center text-sm">Loading…</div>
  }
  if (me === null) {
    return <LoginPage />
  }

  const home = me.is_admin ? "/" : "/tessellations"
  const allowed = (path: string) => !!PAGES[path] && (me.is_admin || !PAGES[path].adminOnly)
  const active = allowed(route) ? route : home
  const page = PAGES[active]
  const Page = page.element

  return (
    <SidebarProvider>
      <AppSidebar variant="inset" activeRoute={active} />
      <SidebarInset>
        <SiteHeader title={page.title} />
        <div className="flex min-h-0 flex-1 flex-col">
          <Suspense fallback={<div className="text-muted-foreground p-6 text-sm">Loading…</div>}>
            <Page />
          </Suspense>
        </div>
      </SidebarInset>
    </SidebarProvider>
  )
}

function App() {
  return (
    <AuthProvider>
      <Shell />
      <Toaster richColors closeButton position="bottom-right" />
    </AuthProvider>
  )
}

export default App
