// The workbench's commands (src-tauri/src/main.rs).
import { invoke } from "@tauri-apps/api/core"

export const COLORS = ["green", "blue", "purple", "amber", "red", "gray"] as const
export type Color = (typeof COLORS)[number]

export interface Ssh {
  host: string
  port: number
  user: string
  identity_file: string
}

export interface Environment {
  id: string
  name: string
  color: Color
  url: string
  ssh: Ssh | null
  has_key: boolean
}

export interface Health {
  ok: boolean
  version: string | null
  message: string
}

export interface HostKey {
  line: string
  kind: string
  fingerprint: string
}

export interface Difference {
  tessellation: string
  what: "only_left" | "only_right" | "kind" | "index_only_left" | "index_only_right" | "index_differs" | "schema"
  detail: string
  left: unknown
  right: unknown
}

export const api = {
  list: () => invoke<Environment[]>("list_environments"),
  /** `key`: a new API key, "" to remove it, undefined to keep it. */
  save: (environment: Environment, key?: string) => invoke<Environment>("save_environment", { environment, key }),
  remove: (id: string) => invoke<void>("delete_environment", { id }),
  check: (id: string) => invoke<Health>("check_environment", { id }),
  open: (id: string) => invoke<void>("open_environment", { id }),
  scanHost: (id: string) => invoke<HostKey[]>("scan_host", { id }),
  trustHost: (keys: HostKey[]) => invoke<void>("trust_host", { keys }),
  compare: (left: string, right: string) => invoke<Difference[]>("compare_environments", { left, right }),
}
