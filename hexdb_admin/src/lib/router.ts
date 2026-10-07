import { useEffect, useState } from "react"

/** The app is served under Vite's base path (e.g. "/ui/"). */
const BASE = import.meta.env.BASE_URL.replace(/\/$/, "")

/** Full URL for an in-app route like "/queries" or "/documents?tessellation=x". */
export function href(route: string): string {
  return `${BASE}${route === "/" ? "/" : route}`
}

/** The current in-app route path, e.g. "/" or "/queries" (no query string). */
function currentRoute(): string {
  const path = window.location.pathname
  const route = path.startsWith(BASE) ? path.slice(BASE.length) : path
  return route.replace(/\/$/, "") || "/"
}

const listeners = new Set<() => void>()
const notify = () => listeners.forEach((listener) => listener())

/** Navigate to an in-app route (optionally with a query string) without reloading. */
export function navigate(route: string) {
  const target = href(route)
  if (window.location.pathname + window.location.search === target) return
  window.history.pushState(null, "", target)
  notify()
}

function useLocationVersion(): number {
  const [version, setVersion] = useState(0)
  useEffect(() => {
    const update = () => setVersion((v) => v + 1)
    listeners.add(update)
    window.addEventListener("popstate", update)
    return () => {
      listeners.delete(update)
      window.removeEventListener("popstate", update)
    }
  }, [])
  return version
}

/** Subscribe to the current route path. */
export function useRoute(): string {
  useLocationVersion()
  return currentRoute()
}

/** Read a query-string parameter; re-renders when it changes. */
export function useQueryParam(name: string): string | null {
  useLocationVersion()
  return new URLSearchParams(window.location.search).get(name)
}

/** Set (or with null, remove) a query-string parameter without adding history entries. */
export function setQueryParam(name: string, value: string | null) {
  const params = new URLSearchParams(window.location.search)
  if (value === null || value === "") params.delete(name)
  else params.set(name, value)
  const query = params.toString()
  window.history.replaceState(null, "", window.location.pathname + (query ? `?${query}` : ""))
  notify()
}

/** Click handler for links: navigate in-app unless the user wants a new tab. */
export function linkHandler(route: string) {
  return (event: React.MouseEvent) => {
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.button !== 0) return
    event.preventDefault()
    navigate(route)
  }
}
