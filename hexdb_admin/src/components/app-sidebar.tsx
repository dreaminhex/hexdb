import * as React from "react"
import {
  IconBrandGoogleBigQuery,
  IconDashboard,
  IconFileCode2,
  IconHexagon3d,
  IconListCheck,
  IconMathFunction,
  IconArrowsSplit,
  IconLogout,
  IconPlug,
  IconReport,
  IconSettings,
  IconUserCheck,
  IconUserCircle,
  type Icon,
} from "@tabler/icons-react"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
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
import type { Action } from "@/lib/api"
import { has, useAuth } from "@/lib/auth"
import { href, linkHandler } from "@/lib/router"
import { cn } from "@/lib/utils"

interface NavItem {
  title: string
  icon: Icon
  /** In-app route; items without one aren't built yet and render disabled. */
  route?: string
  /** Shown only to users with this permission. */
  requires?: Action
}

const SECTIONS: { label?: string; items: NavItem[] }[] = [
  {
    items: [
      { title: "Dashboard", route: "/", icon: IconDashboard, requires: "status" },
      { title: "Queries", route: "/queries", icon: IconBrandGoogleBigQuery },
    ],
  },
  {
    label: "Database",
    items: [
      { title: "Tessellations", route: "/tessellations", icon: IconHexagon3d },
      { title: "Documents", route: "/documents", icon: IconFileCode2 },
      { title: "Streams", route: "/streams", icon: IconArrowsSplit },
      { title: "Functions", route: "/functions", icon: IconMathFunction },
    ],
  },
  {
    label: "Security",
    items: [
      { title: "Users", route: "/users", icon: IconUserCircle, requires: "admin" },
      { title: "Roles", route: "/roles", icon: IconUserCheck, requires: "admin" },
      { title: "Audit Trail", route: "/audit", icon: IconListCheck, requires: "audit" },
    ],
  },
  {
    label: "System",
    items: [{ title: "Plugins", route: "/plugins", icon: IconPlug, requires: "plugins" }],
  },
]

const FOOTER_ITEMS: NavItem[] = [
  { title: "Logs", route: "/logs", icon: IconReport, requires: "logs" },
  { title: "Settings", route: "/settings", icon: IconSettings, requires: "admin" },
]

/** The signed-in user, with links to the account page and to sign out. */
function SignedInUser({ active }: { active: boolean }) {
  const { me, signOut } = useAuth()
  if (!me) return null
  return (
    <div className="flex items-center gap-2 px-2">
      <a
        href={href("/account")}
        onClick={linkHandler("/account")}
        className={cn("hover:bg-sidebar-accent flex min-w-0 flex-1 items-center gap-2 rounded-md px-1 py-1", active && "bg-sidebar-accent")}
        title="Account: password and API keys"
      >
        <IconUserCircle className="text-muted-foreground size-5 shrink-0" />
        <span className="grid min-w-0 text-left text-sm leading-tight">
          <span className="truncate font-medium">{me.login}</span>
          <span className="text-muted-foreground truncate text-xs">{me.is_admin ? "Administrator" : me.roles.map((r) => r.name).join(", ") || "No roles"}</span>
        </span>
      </a>
      <Button variant="ghost" size="icon" className="text-muted-foreground size-8 shrink-0" aria-label="Sign out" title="Sign out" onClick={() => void signOut()}>
        <IconLogout />
      </Button>
    </div>
  )
}

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
  const { me } = useAuth()
  const visible = (item: NavItem) => !item.requires || has(me, item.requires)
  const sections = SECTIONS.map((section) => ({ ...section, items: section.items.filter(visible) })).filter((s) => s.items.length > 0)
  const footerItems = FOOTER_ITEMS.filter(visible)
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
        {sections.map((section, i) => (
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
              {footerItems.map((item) => (
                <SidebarMenuItem key={item.title}>
                  <NavLink item={item} active={item.route === activeRoute} />
                </SidebarMenuItem>
              ))}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>
      <SidebarFooter>
        <SignedInUser active={activeRoute === "/account"} />
        <HexIdentity />
      </SidebarFooter>
    </Sidebar>
  )
}
