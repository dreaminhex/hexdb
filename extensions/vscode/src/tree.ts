// The Connections view: each connection's lattice (hexes and regions), tessellations and functions.

import * as vscode from "vscode"

import { errorMessage, type Connections, type SavedConnection, type Session } from "./connections"
import { describeRegion } from "./regions"

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
  | { kind: "message"; label: string; error?: boolean }

const roleIcon: Record<string, string> = { Overseer: "star-full", Harvester: "circle-filled", Replicant: "circle-outline" }

export class ConnectionsTree implements vscode.TreeDataProvider<Node> {
  private readonly changed = new vscode.EventEmitter<Node | undefined>()
  readonly onDidChangeTreeData = this.changed.event
  /** Connections whose icon was repainted after signing in. */
  private readonly painted = new Set<string>()

  constructor(private readonly connections: Connections) {
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
            `**${h.name}** — ${h.role}${h.is_self ? " (answered this connection)" : ""}`,
            `Region: ${describeRegion(h.region)}`,
            `Status: ${h.status}${h.replication_state ? `, ${h.replication_state}` : ""}`,
            h.lag !== null && h.lag !== undefined ? `Lag: ${h.lag} changes` : "",
            h.slicing && h.slices_held !== null && h.slices_held !== undefined ? `Slices held: ${h.slices_held} of 64` : "",
            h.version ? `Version: ${h.version}` : "",
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
            `**${f.name}** (${f.kind}${f.runtime ? `, ${f.runtime}` : ""})`,
            f.description ?? "",
            f.params?.length ? `Parameters: ${f.params.map((p) => `\`${p.name}: ${p.type}${p.required ? "" : "?"}\``).join(", ")}` : "",
          ]
            .filter(Boolean)
            .join("\n\n"),
        )
        item.iconPath = new vscode.ThemeIcon(f.kind === "script" ? "code" : "symbol-function")
        item.contextValue = node.admin ? "function-admin" : "function"
        item.command = { command: "hexdb.openFunction", title: "Open Function", arguments: [node] }
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
    return nodes
  }
}

function rank(role: string): number {
  return role === "Overseer" ? 0 : role === "Harvester" ? 1 : 2
}
