import { lazy, Suspense } from "react"

import { AppSidebar } from "@/components/app-sidebar"
import { SiteHeader } from "@/components/site-header"
import { NoticeBar } from "@/components/notice-bar"
import { SidebarInset, SidebarProvider } from "@/components/ui/sidebar"
import { Toaster } from "@/components/ui/sonner"
import { AuthProvider, has, useAuth, type Requirement } from "@/lib/auth"
import { useRoute } from "@/lib/router"
import { ShareView } from "@/pages/share-view"
import { LoginPage } from "@/pages/login"

// Each page loads on first visit.
const AccountPage = lazy(() => import("@/pages/account").then((m) => ({ default: m.AccountPage })))
const AskPage = lazy(() => import("@/pages/ask").then((m) => ({ default: m.AskPage })))
const SharesPage = lazy(() => import("@/pages/shares").then((m) => ({ default: m.SharesPage })))
const AuditPage = lazy(() => import("@/pages/audit").then((m) => ({ default: m.AuditPage })))
const ConfigurationPage = lazy(() => import("@/pages/configuration").then((m) => ({ default: m.ConfigurationPage })))
const LogsPage = lazy(() => import("@/pages/logs").then((m) => ({ default: m.LogsPage })))
const OverviewPage = lazy(() => import("@/pages/overview").then((m) => ({ default: m.OverviewPage })))
const PluginsPage = lazy(() => import("@/pages/plugins").then((m) => ({ default: m.PluginsPage })))
const RolesPage = lazy(() => import("@/pages/roles").then((m) => ({ default: m.RolesPage })))
const SettingsPage = lazy(() => import("@/pages/settings").then((m) => ({ default: m.SettingsPage })))
const TessellationsPage = lazy(() => import("@/pages/tessellations").then((m) => ({ default: m.TessellationsPage })))
const UsersPage = lazy(() => import("@/pages/users").then((m) => ({ default: m.UsersPage })))
const QueriesPage = lazy(() => import("@/pages/queries").then((m) => ({ default: m.QueriesPage })))
const DocumentsPage = lazy(() => import("@/pages/documents").then((m) => ({ default: m.DocumentsPage })))
const QueuesPage = lazy(() => import("@/pages/queues").then((m) => ({ default: m.QueuesPage })))
const StreamsPage = lazy(() => import("@/pages/streams").then((m) => ({ default: m.StreamsPage })))
const FunctionsPage = lazy(() => import("@/pages/functions").then((m) => ({ default: m.FunctionsPage })))
const FilesPage = lazy(() => import("@/pages/files").then((m) => ({ default: m.FilesPage })))

interface Page {
  title: string
  element: React.ComponentType
  /** Shown only to users with this permission (the server enforces it too). */
  requires?: Requirement
}

const PAGES: Record<string, Page> = {
  "/": { title: "Overview", element: OverviewPage, requires: "status" },
  "/queries": { title: "Query Console", element: QueriesPage },
  "/ask": { title: "Ask", element: AskPage, requires: "chat" },
  "/tessellations": { title: "Tessellations", element: TessellationsPage },
  "/documents": { title: "Documents", element: DocumentsPage },
  "/files": { title: "Files", element: FilesPage },
  "/shares": { title: "Shared links", element: SharesPage, requires: "share" },
  "/streams": { title: "Streams", element: StreamsPage },
  "/queues": { title: "Queues", element: QueuesPage },
  "/functions": { title: "Functions", element: FunctionsPage },
  "/users": { title: "Users", element: UsersPage, requires: "admin" },
  "/roles": { title: "Roles", element: RolesPage, requires: "admin" },
  "/audit": { title: "Audit Trail", element: AuditPage, requires: "audit" },
  "/logs": { title: "Logs", element: LogsPage, requires: "logs" },
  "/plugins": { title: "Plugins", element: PluginsPage, requires: "plugins" },
  "/configuration": { title: "Configuration", element: ConfigurationPage, requires: "view_config" },
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
  const route = useRoute()
  // A sharing link's page is for people without an account: no sign-in, no app chrome.
  if (route.startsWith("/s/")) {
    return <ShareView code={route.slice(3).split("/")[0]} />
  }
  return (
    <AuthProvider>
      <Shell />
      <Toaster richColors closeButton position="bottom-right" />
    </AuthProvider>
  )
}

export default App
