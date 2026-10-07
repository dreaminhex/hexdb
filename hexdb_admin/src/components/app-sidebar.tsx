import * as React from "react"
import {
  IconBrandGoogleBigQuery,
  IconDashboard,
  IconFileCode2,
  IconHexagon3d,
  IconReport,
  IconSettings,
  IconUserCheck,
  IconUserCircle,
  type Icon,
} from "@tabler/icons-react"

import { Badge } from "@/components/ui/badge"
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
} from "@/components/ui/sidebar"
import { usePoll } from "@/hooks/use-poll"
import { href, linkHandler } from "@/lib/router"
import { cn } from "@/lib/utils"

interface NavItem {
  title: string
  icon: Icon
  /** In-app route; items without one aren't built yet and render disabled. */
  route?: string
}

const SECTIONS: { label?: string; items: NavItem[] }[] = [
  {
    items: [
      { title: "Dashboard", route: "/", icon: IconDashboard },
      { title: "Queries", route: "/queries", icon: IconBrandGoogleBigQuery },
    ],
  },
  {
    label: "Database",
    items: [
      { title: "Tessellations", route: "/tessellations", icon: IconHexagon3d },
      { title: "Documents", route: "/documents", icon: IconFileCode2 },
    ],
  },
  {
    label: "Security",
    items: [
      { title: "Users", route: "/users", icon: IconUserCircle },
      { title: "Roles", route: "/roles", icon: IconUserCheck },
    ],
  },
]

const FOOTER_ITEMS: NavItem[] = [
  { title: "Logs", icon: IconReport },
  { title: "Settings", icon: IconSettings },
]

function NavLink({ item, active }: { item: NavItem; active: boolean }) {
  if (!item.route) {
    return (
      <SidebarMenuButton tooltip={`${item.title} (coming soon)`} aria-disabled className="text-muted-foreground cursor-default hover:bg-transparent">
        <item.icon />
        <span>{item.title}</span>
        <Badge variant="outline" className="ml-auto px-1.5 py-0 text-[10px] font-normal">
          Soon
        </Badge>
      </SidebarMenuButton>
    )
  }
  return (
    <SidebarMenuButton tooltip={item.title} isActive={active} asChild>
      <a href={href(item.route)} onClick={linkHandler(item.route)}>
        <item.icon />
        <span>{item.title}</span>
      </a>
    </SidebarMenuButton>
  )
}

/** This hex's name, role and health, refreshed every 30 seconds. */
function HexIdentity() {
  const health = usePoll(
    () => fetch("/health").then((r) => (r.ok ? r.json() : Promise.reject(new Error(r.statusText)))),
    30_000,
  )
  const data = health.data as { name: string; hex_type: string; version: string } | undefined
  const online = !!data && !health.error

  return (
    <div className="flex items-center gap-3 rounded-md px-2 py-1.5">
      <span className="relative flex size-2.5 shrink-0">
        {online && <span className="bg-status-good absolute inline-flex size-full animate-ping rounded-full opacity-40" />}
        <span className={cn("relative inline-flex size-2.5 rounded-full", online ? "bg-status-good" : "bg-destructive")} />
      </span>
      <div className="grid min-w-0 flex-1 text-left text-sm leading-tight">
        <span className="truncate font-medium">{data?.name ?? (health.loading ? "Connecting…" : "Unreachable")}</span>
        <span className="text-muted-foreground truncate text-xs">
          {data ? `${data.hex_type} · v${data.version}` : "HexDB"}
        </span>
      </div>
    </div>
  )
}

export function AppSidebar({ activeRoute = "/", ...props }: React.ComponentProps<typeof Sidebar> & { activeRoute?: string }) {
  return (
    <Sidebar collapsible="offcanvas" {...props}>
      <SidebarHeader>
        <SidebarMenu>
          <SidebarMenuItem>
            <a href={href("/")} onClick={linkHandler("/")} aria-label="HexDB dashboard">
              {/* Transparent, with outlined lettering, so one image works in light and dark themes. */}
              <img src="hexdb_lg.png" alt="HexDB" className="h-9 w-auto" />
            </a>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarHeader>
      <SidebarContent>
        {SECTIONS.map((section, i) => (
          <SidebarGroup key={section.label ?? i}>
            {section.label && <SidebarGroupLabel>{section.label}</SidebarGroupLabel>}
            <SidebarGroupContent>
              <SidebarMenu>
                {section.items.map((item) => (
                  <SidebarMenuItem key={item.title}>
                    <NavLink item={item} active={item.route === activeRoute} />
                  </SidebarMenuItem>
                ))}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        ))}
        <SidebarGroup className="mt-auto">
          <SidebarGroupContent>
            <SidebarMenu>
              {FOOTER_ITEMS.map((item) => (
                <SidebarMenuItem key={item.title}>
                  <NavLink item={item} active={false} />
                </SidebarMenuItem>
              ))}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>
      <SidebarFooter>
        <HexIdentity />
      </SidebarFooter>
    </Sidebar>
  )
}
