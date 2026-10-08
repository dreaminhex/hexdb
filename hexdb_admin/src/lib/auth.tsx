import { createContext, useCallback, useContext, useEffect, useState } from "react"

import { api, ApiError, UNAUTHORIZED_EVENT, type Action, type Me } from "@/lib/api"

interface AuthState {
  /** The signed-in user; `null` when signed out; `undefined` while checking. */
  me: Me | null | undefined
  /** Re-read the signed-in user (after sign-in, or a role change). */
  refresh: () => Promise<void>
  signOut: () => Promise<void>
}

const AuthContext = createContext<AuthState>({ me: undefined, refresh: async () => {}, signOut: async () => {} })

/**
 * Tracks the signed-in user. The session itself is an HttpOnly cookie that
 * scripts can't read; any 401 from the API switches back to the sign-in screen.
 */
export function AuthProvider({ children }: { children: React.ReactNode }) {
  const [me, setMe] = useState<Me | null | undefined>(undefined)

  const refresh = useCallback(async () => {
    try {
      setMe(await api.me())
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) setMe(null)
      else throw e
    }
  }, [])

  const signOut = useCallback(async () => {
    try {
      await api.logout()
    } finally {
      setMe(null)
    }
  }, [])

  useEffect(() => {
    refresh().catch(() => setMe(null))
    const onUnauthorized = () => setMe(null)
    window.addEventListener(UNAUTHORIZED_EVENT, onUnauthorized)
    return () => window.removeEventListener(UNAUTHORIZED_EVENT, onUnauthorized)
  }, [refresh])

  return <AuthContext.Provider value={{ me, refresh, signOut }}>{children}</AuthContext.Provider>
}

export type Permission = "read" | "write" | "manage"

/**
 * Whether the user may do something to a tessellation. Mirrors the server's
 * rules so the UI only offers what will work; the server enforces them.
 */
// eslint-disable-next-line react-refresh/only-export-components
export function can(me: Me | null | undefined, permission: Permission, tessellation: string): boolean {
  if (!me) return false
  if (me.is_admin) return true
  return (me.grants ?? []).some(
    (grant) => grant.permissions.includes(permission) && (grant.tessellations.includes("*") || grant.tessellations.includes(tessellation)),
  )
}

/** Whether the user holds a permission that applies everywhere (status, logs, audit, ...). */
// eslint-disable-next-line react-refresh/only-export-components
export function has(me: Me | null | undefined, action: Action): boolean {
  if (!me) return false
  return me.is_admin || (me.permissions ?? []).includes(action)
}

// eslint-disable-next-line react-refresh/only-export-components
export function useAuth(): AuthState {
  return useContext(AuthContext)
}
