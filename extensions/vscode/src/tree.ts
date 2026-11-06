// The Connections view: each connection's lattice (hexes and regions), tessellations, functions
// and, for administrators, the files the malware scan quarantined.

import * as vscode from "vscode"

import { errorMessage, type Connections, type SavedConnection, type Session } from "./connections"
import { describeDetections, type Quarantine, type QuarantinedFile } from "./quarantine"
import { describeRegion } from "./regions"
import { formatLag } from "./shared/format"

export interface TessellationInfo {
  name: string
  kind?: string
  indexes?: string[]
  schema_version?: number | null
}

export interface FunctionInfo {
  name: string
  kind: string
  description?: string
  runtime?: string
  tessellation?: string
  params?: { name: string; type: string; required?: boolean; default?: unknown; description?: string }[]
}

export interface LatticeHex {
  id: string
  name: string
  role: string
  status: string
  region?: string
  version?: string
  lag?: number | null
  /** Lag in time, in ms (HexDB 2.3.3+). */
  lag_ms?: number | null
  is_self?: boolean
  slices_held?: number | null
  slicing?: boolean
  replication_state?: string
  last_seen?: string | null
  preference?: string
}

export interface StatusInfo {
  name: string
  version: string
  hex_type: string
  uptime_seconds?: number
  network?: { lattice?: { name?: string; hexes?: LatticeHex[]; regions?: string[] } }
}

export type Node =
  | { kind: "connection"; connection: SavedConnection }
  | { kind: "lattice"; connection: SavedConnection; status: StatusInfo }
  | { kind: "hex"; connection: SavedConnection; hex: LatticeHex }
  | { kind: "tessellations"; connection: SavedConnection }
  | { kind: "tessellation"; connection: SavedConnection; tessellation: TessellationInfo }
  | { kind: "column"; label: string; description?: string }
  | { kind: "functions"; connection: SavedConnection; admin: boolean }
  | { kind: "function"; connection: SavedConnection; fn: FunctionInfo; admin: boolean }
  | { kind: "quarantine"; connection: SavedConnection; files: QuarantinedFile[] }
  | { kind: "quarantined"; connection: SavedConnection; file: QuarantinedFile }
  | { kind: "message"; label: string; error?: boolean }

const roleIcon: Record<string, string> = { Overseer: "star-full", Harvester: "circle-filled", Replicant: "circle-outline" }

export class ConnectionsTree implements vscode.TreeDataProvider<Node> {
  private readonly changed = new vscode.EventEmitter<Node | undefined>()
  readonly onDidChangeTreeData = this.changed.event
  /** Connections whose icon was repainted after signing in. */
  private readonly painted = new Set<string>()

  constructor(
    private readonly connections: Connections,
    private readonly quarantine: Quarantine,
  ) {
    // A file quarantined, released or deleted redraws that connection.
    quarantine.onDidChange((id) => {
      const connection = connections.get(id)
      this.refresh(connection ? { kind: "connection", connection } : undefined)
    })
    connections.onDidChange(() => {
      // Signing out or editing makes the icon change again on the next connect.
      for (const id of this.painted) if (!connections.isOpen(id)) this.painted.delete(id)
      this.refresh()
    })
  }

  refresh(node?: Node): void {
    this.changed.fire(node)
  }

  getTreeItem(node: Node): vscode.TreeItem {
    const C = vscode.TreeItemCollapsibleState
    switch (node.kind) {
      case "connection": {
        const item = new vscode.TreeItem(node.connection.name, C.Collapsed)
        item.id = node.connection.id
        item.description = node.connection.display
        item.tooltip = node.connection.display
        item.contextValue = "connection"
        item.iconPath = new vscode.ThemeIcon("database", this.connections.isOpen(node.connection.id) ? new vscode.ThemeColor("charts.green") : undefined)
        return item
      }
      case "lattice": {
        const lattice = node.status.network?.lattice
        const hexes = lattice?.hexes ?? []
        const regions = lattice?.regions ?? []
        const item = new vscode.TreeItem(lattice?.name ? `Lattice: ${lattice.name}` : "Lattice", C.Collapsed)
        item.id = `${node.connection.id}/lattice`
        item.description = `${hexes.length} ${hexes.length === 1 ? "hex" : "hexes"}${regions.length ? ` · ${regions.length} ${regions.length === 1 ? "region" : "regions"}` : ""}`
        item.iconPath = new vscode.ThemeIcon("type-hierarchy")
        item.contextValue = "lattice"
        item.command = { command: "hexdb.openLattice", title: "Show Lattice", arguments: [node] }
        return item
      }
      case "hex": {
        const h = node.hex
        const item = new vscode.TreeItem(h.name, C.None)
        item.description = `${h.role} · ${describeRegion(h.region)}${h.status !== "active" ? ` · ${h.status}` : ""}`
        item.iconPath = new vscode.ThemeIcon(roleIcon[h.role] ?? "circle-outline", h.status === "active" ? undefined : new vscode.ThemeColor("errorForeground"))
        const tip = new vscode.MarkdownString(
          [
            `**${md(h.name)}** — ${md(h.role)}${h.is_self ? " (connected: this hex answered the connection)" : ""}`,
            `Region: ${md(describeRegion(h.region))}`,
            `Status: ${md(h.status)}${h.replication_state ? `, ${md(h.replication_state)}` : ""}`,
            formatLag(h.lag, h.lag_ms) ? `Lag: ${md(formatLag(h.lag, h.lag_ms))}` : "",
            h.slicing && h.slices_held !== null && h.slices_held !== undefined ? `Slices held: ${md(h.slices_held)} of 64` : "",
            h.version ? `Version: ${md(h.version)}` : "",
          ]
            .filter(Boolean)
            .join("\n\n"),
        )
        item.tooltip = tip
        item.command = { command: "hexdb.openLattice", title: "Show Lattice", arguments: [{ kind: "connection", connection: node.connection }] }
        return item
      }
      case "tessellations": {
        const item = new vscode.TreeItem("Tessellations", C.Collapsed)
        item.id = `${node.connection.id}/tessellations`
        item.iconPath = new vscode.ThemeIcon("folder-library")
        return item
      }
      case "tessellation": {
        const t = node.tessellation
        const files = t.kind === "files"
        const item = new vscode.TreeItem(t.name, C.Collapsed)
        item.id = `${node.connection.id}/t/${t.name}`
        item.description = [files ? "files" : "", t.indexes?.length ? `${t.indexes.length} ${t.indexes.length === 1 ? "index" : "indexes"}` : "", t.schema_version ? `schema v${t.schema_version}` : ""]
          .filter(Boolean)
          .join(" · ")
        item.iconPath = new vscode.ThemeIcon(files ? "file-media" : "table")
        item.contextValue = files ? "tessellation-files" : "tessellation"
        item.tooltip = `${t.name}${t.indexes?.length ? `\nIndexes: ${t.indexes.join(", ")}` : ""}`
        return item
      }
      case "column": {
        const item = new vscode.TreeItem(node.label, C.None)
        item.description = node.description
        item.iconPath = new vscode.ThemeIcon("symbol-field")
        return item
      }
      case "functions": {
        const item = new vscode.TreeItem("Functions", C.Collapsed)
        item.id = `${node.connection.id}/functions`
        item.iconPath = new vscode.ThemeIcon("symbol-method")
        item.contextValue = node.admin ? "functions-admin" : "functions"
        return item
      }
      case "function": {
        const f = node.fn
        const item = new vscode.TreeItem(f.name, C.None)
        item.id = `${node.connection.id}/f/${f.name}`
        item.description = f.kind === "script" ? `script · ${f.runtime ?? ""}` : f.tessellation ? `${f.kind} · ${f.tessellation}` : f.kind
        item.tooltip = new vscode.MarkdownString(
          [
            `**${md(f.name)}** (${md(f.kind)}${f.runtime ? `, ${md(f.runtime)}` : ""})`,
            md(f.description),
            f.params?.length ? `Parameters: ${f.params.map((p) => `${md(p.name)}: ${md(p.type)}${p.required ? "" : "?"}`).join(", ")}` : "",
          ]
            .filter(Boolean)
            .join("\n\n"),
        )
        item.iconPath = new vscode.ThemeIcon(f.kind === "script" ? "code" : "symbol-function")
        item.contextValue = node.admin ? "function-admin" : "function"
        item.command = { command: "hexdb.openFunction", title: "Open Function", arguments: [node] }
        return item
      }
      case "quarantine": {
        const n = node.files.length
        const item = new vscode.TreeItem("Quarantine", n ? C.Expanded : C.Collapsed)
        item.id = `${node.connection.id}/quarantine`
        item.description = n ? `${n} ${n === 1 ? "file" : "files"}` : "nothing quarantined"
        item.tooltip = "Uploads the malware scan flagged. They aren't in their tessellation, can't be downloaded and never reach another hex. Release a false positive, or delete it."
        item.iconPath = new vscode.ThemeIcon(n ? "warning" : "shield", n ? new vscode.ThemeColor("list.warningForeground") : undefined)
        item.contextValue = "quarantine"
        return item
      }
      case "quarantined": {
        const f = node.file
        const item = new vscode.TreeItem(f.name, C.None)
        item.id = `${node.connection.id}/q/${f.id}`
        item.description = `${f.tessellation} · ${f.scan.detections.map((d) => d.name).join(", ")}`
        item.tooltip = new vscode.MarkdownString(
          [
            `**${md(f.name)}** in ${md(f.tessellation)}/${md(f.path)}`,
            `Matched: ${md(describeDetections(f))}`,
            f.uploaded_by ? `Uploaded by ${md(f.uploaded_by)}${f.uploaded_at ? ` at ${md(new Date(f.uploaded_at).toLocaleString())}` : ""}` : "",
            f.rescan ? "Found by a rescan after it was stored." : "",
            `Size: ${md(formatBytes(f.size))}`,
            f.scan.engines.length ? `Scanned by ${md(f.scan.engines.join(", "))}` : "",
          ]
            .filter(Boolean)
            .join("\n\n"),
        )
        item.iconPath = new vscode.ThemeIcon("bug", new vscode.ThemeColor("errorForeground"))
        item.contextValue = "quarantined-file"
        item.command = { command: "hexdb.showQuarantined", title: "Show Details", arguments: [node] }
        return item
      }
      case "message": {
        const item = new vscode.TreeItem(node.label, C.None)
        item.iconPath = new vscode.ThemeIcon(node.error ? "error" : "info", node.error ? new vscode.ThemeColor("errorForeground") : undefined)
        item.tooltip = node.label
        return item
      }
    }
  }

  /** Only connections are revealed (they're top level). */
  getParent(): undefined {
    return undefined
  }

  async getChildren(node?: Node): Promise<Node[]> {
    if (!node) return this.connections.list().map((connection) => ({ kind: "connection", connection }))
    try {
      switch (node.kind) {
        case "connection":
          return await this.connectionChildren(node.connection)
        case "lattice":
          return (node.status.network?.lattice?.hexes ?? [])
            .slice()
            .sort((a, b) => rank(a.role) - rank(b.role) || a.name.localeCompare(b.name))
            .map((hex) => ({ kind: "hex", connection: node.connection, hex }))
        case "tessellations": {
          const session = await this.connections.session(node.connection.id)
          const { tessellations } = await session.request<{ tessellations: TessellationInfo[] }>("GET", "/tessellations")
          const visible = tessellations.filter((t) => !t.name.startsWith("_")).sort((a, b) => a.name.localeCompare(b.name))
          if (visible.length === 0) return [{ kind: "message", label: "No tessellations you can read" }]
          return visible.map((tessellation) => ({ kind: "tessellation", connection: node.connection, tessellation }))
        }
        case "tessellation": {
          const session = await this.connections.session(node.connection.id)
          const { columns } = await session.request<{ columns: { name: string; type?: string }[] }>("GET", `/sql/columns?table=${encodeURIComponent(node.tessellation.name)}`)
          const indexed = new Set(node.tessellation.indexes ?? [])
          if (columns.length === 0) return [{ kind: "message", label: "No documents yet" }]
          return columns.map((c) => ({ kind: "column", label: c.name, description: [c.type, indexed.has(c.name) ? "indexed" : ""].filter(Boolean).join(" · ") }))
        }
        case "functions": {
          const session = await this.connections.session(node.connection.id)
          const { functions } = await session.request<{ functions: FunctionInfo[] }>("GET", "/functions")
          if (functions.length === 0) return [{ kind: "message", label: node.admin ? "No functions yet: use + to add one" : "No functions" }]
          return functions.sort((a, b) => a.name.localeCompare(b.name)).map((fn) => ({ kind: "function", connection: node.connection, fn, admin: node.admin }))
        }
        case "quarantine":
          if (node.files.length === 0) return [{ kind: "message", label: "Nothing is quarantined" }]
          return node.files
            .slice()
            .sort((a, b) => (b.uploaded_at ?? "").localeCompare(a.uploaded_at ?? ""))
            .map((file) => ({ kind: "quarantined", connection: node.connection, file }))
        default:
          return []
      }
    } catch (e) {
      return [{ kind: "message", label: errorMessage(e), error: true }]
    }
  }

  private async connectionChildren(connection: SavedConnection): Promise<Node[]> {
    let session: Session
    try {
      session = await this.connections.session(connection.id)
    } catch (e) {
      return [{ kind: "message", label: `Couldn't connect: ${errorMessage(e)}`, error: true }]
    }
    // The icon turns green once connected (repainted once, or this would loop: a repaint reloads the children).
    if (!this.painted.has(connection.id)) {
      this.painted.add(connection.id)
      setTimeout(() => this.refresh({ kind: "connection", connection }), 0)
    }
    const nodes: Node[] = []
    try {
      const status = await session.request<StatusInfo>("GET", "/status")
      nodes.push({ kind: "lattice", connection, status })
    } catch {
      nodes.push({ kind: "message", label: "Lattice hidden: needs the status permission" })
    }
    nodes.push({ kind: "tessellations", connection }, { kind: "functions", connection, admin: session.isAdmin })
    // Administrators only (and HexDB 2.4+); a server that can't be asked just doesn't show it.
    if (session.isAdmin) {
      const files = await this.quarantine.list(connection).catch(() => null)
      if (files) nodes.push({ kind: "quarantine", connection, files })
    }
    return nodes
  }
}

/** Server text as literal Markdown, so a hostile server can't add links or remote images to a tooltip. */
function md(value: unknown): string {
  return String(value ?? "").replace(/[\\`*_{}[\]()#+\-.!|<>~:]/g, "\\$&")
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  const units = ["KB", "MB", "GB", "TB"]
  let value = bytes / 1024
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit++
  }
  return `${value.toFixed(value < 10 ? 1 : 0)} ${units[unit]}`
}

function rank(role: string): number {
  return role === "Overseer" ? 0 : role === "Harvester" ? 1 : 2
}
