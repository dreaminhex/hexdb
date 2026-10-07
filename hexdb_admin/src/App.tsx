import { AppSidebar } from "@/components/app-sidebar"
import { ChartAreaInteractive } from "@/components/chart-area-interactive"
import { DataTable } from "./components/data-table"
import { SectionCards } from "@/components/section-cards"
import { SiteHeader } from "@/components/site-header"
import { SidebarInset, SidebarProvider } from "@/components/ui/sidebar"
import { lazy, Suspense } from "react"
import { useRoute } from "@/lib/router"
import data from "./app/dashboard/data.json"

// The query console bundles the GraphQL editor, so load it only when visited.
const QueriesPage = lazy(() => import("@/pages/queries").then((m) => ({ default: m.QueriesPage })))

function DashboardPage() {
  return (
    <div className="@container/main flex flex-1 flex-col gap-2">
      <div className="flex flex-col gap-4 py-4 md:gap-6 md:py-6">
        <SectionCards />
        <div className="px-4 lg:px-6">
          <ChartAreaInteractive />
        </div>
        <DataTable data={data} />
      </div>
    </div>
  )
}

const PAGES: Record<string, { title: string; element: React.ComponentType }> = {
  "/": { title: "Overview", element: DashboardPage },
  "/queries": { title: "Query Console", element: QueriesPage },
}

function App() {
  const route = useRoute()
  const page = PAGES[route] ?? PAGES["/"]
  const Page = page.element

  return (
    <SidebarProvider>
      <AppSidebar variant="inset" activeRoute={route} />
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

export default App
