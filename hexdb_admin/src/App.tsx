import { lazy, Suspense } from "react"

import { AppSidebar } from "@/components/app-sidebar"
import { SiteHeader } from "@/components/site-header"
import { NoticeBar } from "@/components/notice-bar"
import { SidebarInset, SidebarProvider } from "@/components/ui/sidebar"
import { Toaster } from "@/components/ui/sonner"
import type { Action } from "@/lib/api"
import { AuthProvider, has, useAuth } from "@/lib/auth"
import { useRoute } from "@/lib/router"
import { AccountPage } from "@/pages/account"
import { AuditPage } from "@/pages/audit"
import { LoginPage } from "@/pages/login"
import { LogsPage } from "@/pages/logs"
import { OverviewPage } from "@/pages/overview"
import { PluginsPage } from "@/pages/plugins"
import { RolesPage } from "@/pages/roles"
import { SettingsPage } from "@/pages/settings"
import { TessellationsPage } from "@/pages/tessellations"
import { UsersPage } from "@/pages/users"

// Pages with the CodeMirror/GraphQL editor load on first visit.
const QueriesPage = lazy(() => import("@/pages/queries").then((m) => ({ default: m.QueriesPage })))
const DocumentsPage = lazy(() => import("@/pages/documents").then((m) => ({ default: m.DocumentsPage })))
const StreamsPage = lazy(() => import("@/pages/streams").then((m) => ({ default: m.StreamsPage })))
const FunctionsPage = lazy(() => import("@/pages/functions").then((m) => ({ default: m.FunctionsPage })))

interface Page {
  title: string
  element: React.ComponentType
  /** Shown only to users with this permission (the server enforces it too). */
  requires?: Action
}

const PAGES: Record<string, Page> = {
  "/": { title: "Overview", element: OverviewPage, requires: "status" },
  "/queries": { title: "Query Console", element: QueriesPage },
  "/tessellations": { title: "Tessellations", element: TessellationsPage },
  "/documents": { title: "Documents", element: DocumentsPage },
  "/streams": { title: "Streams", element: StreamsPage },
  "/functions": { title: "Functions", element: FunctionsPage },
  "/users": { title: "Users", element: UsersPage, requires: "admin" },
  "/roles": { title: "Roles", element: RolesPage, requires: "admin" },
  "/audit": { title: "Audit Trail", element: AuditPage, requires: "audit" },
  "/logs": { title: "Logs", element: LogsPage, requires: "logs" },
  "/plugins": { title: "Plugins", element: PluginsPage, requires: "plugins" },
  "/settings": { title: "Settings", element: SettingsPage, requires: "admin" },
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

  const home = has(me, "status") ? "/" : "/tessellations"
  const allowed = (path: string) => !!PAGES[path] && (!PAGES[path].requires || has(me, PAGES[path].requires!))
  const active = allowed(route) ? route : home
  const page = PAGES[active]
  const Page = page.element

  return (
    <SidebarProvider>
      <AppSidebar variant="inset" activeRoute={active} />
      <SidebarInset>
        <NoticeBar />
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
