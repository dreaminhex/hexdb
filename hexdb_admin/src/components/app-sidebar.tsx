import * as React from "react"
import {
  IconBrandGoogleBigQuery,
  IconDashboard,
  IconFileCode2,
  IconHexagon3d,
  IconReport,
  IconSettings,
  IconUserCheck,
  IconUserCircle
} from "@tabler/icons-react"

import { NavDatabase } from "@/components/nav-database"
import { NavMain } from "@/components/nav-main"
import { NavSecondary } from "@/components/nav-secondary"
import { NavUser } from "@/components/nav-user"
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuItem,
} from "@/components/ui/sidebar"
import { NavSecurity } from "./nav-security"

const data = {
  user: {
    name: "hexdbadmin",
    email: "@hexdb.ai",
  },
  navMain: [
    {
      title: "Dashboard",
      url: "#",
      icon: IconDashboard,
    },
    {
      title: "Queries",
      url: "#",
      icon: IconBrandGoogleBigQuery,
    },
    {
      title: "Logs",
      url: "#",
      icon: IconReport,
    }
  ],
  navSecondary: [
    {
      title: "Settings",
      url: "#",
      icon: IconSettings,
    }
  ],
  database: [
    {
      name: "Tessellations",
      url: "#",
      icon: IconHexagon3d,
    },
    {
      name: "Documents",
      url: "#",
      icon: IconFileCode2,
    }
  ],
  security: [
    {
      name: "Roles",
      url: "#",
      icon: IconUserCheck,
    },
    {
      name: "Users",
      url: "#",
      icon: IconUserCircle,
    }
  ],
}

export function AppSidebar({ ...props }: React.ComponentProps<typeof Sidebar>) {
  return (
    <Sidebar collapsible="offcanvas" {...props}>
      <SidebarHeader>
        <SidebarMenu>
          <SidebarMenuItem>
            <img src="hexdb_logo_light.png" alt="logo" className="h-7" />
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarHeader>
      <SidebarContent>
        <NavMain items={data.navMain} />
        <NavDatabase items={data.database} />
        <NavSecurity items={data.security} />
        <NavSecondary items={data.navSecondary} className="mt-auto" />
      </SidebarContent>
      <SidebarFooter>
        <NavUser user={data.user} />
      </SidebarFooter>
    </Sidebar>
  )
}
