import { type Icon } from "@tabler/icons-react"

import {
  SidebarGroup,
  SidebarGroupContent,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
} from "@/components/ui/sidebar"
import { href, linkHandler } from "@/lib/router"

export function NavMain({
  items,
  activeRoute,
}: {
  items: {
    title: string
    /** In-app route (e.g. "/queries"); items without one aren't built yet. */
    route?: string
    url?: string
    icon?: Icon
  }[]
  activeRoute?: string
}) {
  return (
    <SidebarGroup>
      <SidebarGroupContent className="flex flex-col gap-2">
        <SidebarMenu>
          {items.map((item) => (
            <SidebarMenuItem key={item.title}>
              {item.route ? (
                <SidebarMenuButton tooltip={item.title} isActive={item.route === activeRoute} asChild>
                  <a href={href(item.route)} onClick={linkHandler(item.route)}>
                    {item.icon && <item.icon />}
                    <span>{item.title}</span>
                  </a>
                </SidebarMenuButton>
              ) : (
                <SidebarMenuButton tooltip={item.title}>
                  {item.icon && <item.icon />}
                  <span>{item.title}</span>
                </SidebarMenuButton>
              )}
            </SidebarMenuItem>
          ))}
        </SidebarMenu>
      </SidebarGroupContent>
    </SidebarGroup>
  )
}
