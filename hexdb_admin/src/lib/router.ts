import { useEffect, useState } from "react"

/** The app is served under Vite's base path (e.g. "/ui/"). */
const BASE = import.meta.env.BASE_URL.replace(/\/$/, "")

/** Full URL for an in-app route like "/queries". */
export function href(route: string): string {
  return `${BASE}${route === "/" ? "/" : route}`
}

/** The current in-app route, e.g. "/" or "/queries". */
function currentRoute(): string {
  const path = window.location.pathname
  const route = path.startsWith(BASE) ? path.slice(BASE.length) : path
  return route.replace(/\/$/, "") || "/"
}

const listeners = new Set<() => void>()

/** Navigate to an in-app route without reloading. */
export function navigate(route: string) {
  if (currentRoute() === route) return
  window.history.pushState(null, "", href(route))
  listeners.forEach((listener) => listener())
}

/** Subscribe to the current route. */
export function useRoute(): string {
  const [route, setRoute] = useState(currentRoute)
  useEffect(() => {
    const update = () => setRoute(currentRoute())
    listeners.add(update)
    window.addEventListener("popstate", update)
    return () => {
      listeners.delete(update)
      window.removeEventListener("popstate", update)
    }
  }, [])
  return route
}

/** Click handler for links: navigate in-app unless the user wants a new tab. */
export function linkHandler(route: string) {
  return (event: React.MouseEvent) => {
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.button !== 0) return
    event.preventDefault()
    navigate(route)
  }
}
