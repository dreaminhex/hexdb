// The lattice view: one panel per connection showing its hexes by region, refreshed while visible.

import * as vscode from "vscode"

import { errorMessage, type Connections, type SavedConnection } from "./connections"
import { place } from "./regions"
import { webviewHtml } from "./results"
import type { FromLattice, LatticeView, ToLattice } from "./shared/messages"
import type { StatusInfo } from "./tree"

export class LatticePanels implements vscode.Disposable {
  private readonly panels = new Map<string, vscode.WebviewPanel>()

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly connections: Connections,
  ) {}

  dispose(): void {
    for (const p of this.panels.values()) p.dispose()
  }

  show(connection: SavedConnection): void {
    const existing = this.panels.get(connection.id)
    if (existing) {
      existing.reveal()
      return
    }
    const panel = vscode.window.createWebviewPanel("hexdb.lattice", `Lattice: ${connection.name}`, vscode.ViewColumn.Active, {
      enableScripts: true,
      localResourceRoots: [vscode.Uri.joinPath(this.context.extensionUri, "dist", "webview"), vscode.Uri.joinPath(this.context.extensionUri, "media")],
    })
    panel.iconPath = vscode.Uri.joinPath(this.context.extensionUri, "media", "hexdb.svg")
    panel.webview.html = webviewHtml(panel.webview, this.context.extensionUri, "lattice", `Lattice: ${connection.name}`)
    this.panels.set(connection.id, panel)

    const update = async () => {
      const lattice = await this.load(connection)
      void panel.webview.postMessage({ type: "lattice", lattice } satisfies ToLattice)
    }
    let timer: NodeJS.Timeout | undefined
    const schedule = () => {
      clearInterval(timer)
      const seconds = vscode.workspace.getConfiguration("hexdb").get<number>("latticeRefreshSeconds", 5)
      timer = seconds > 0 && panel.visible ? setInterval(() => void update(), seconds * 1000) : undefined
    }
    panel.webview.onDidReceiveMessage((m: FromLattice) => {
      if (m.type === "ready" || m.type === "refresh") void update()
    })
    panel.onDidChangeViewState(() => {
      schedule()
      if (panel.visible) void update()
    })
    panel.onDidDispose(() => {
      clearInterval(timer)
      this.panels.delete(connection.id)
    })
    schedule()
  }

  private async load(connection: SavedConnection): Promise<LatticeView> {
    const base: LatticeView = { connection: connection.name, url: connection.display, name: "", answeredBy: "", version: "", regions: [], hexes: [], fetchedAt: new Date().toISOString() }
    try {
      const session = await this.connections.session(connection.id)
      const status = await session.request<StatusInfo>("GET", "/status")
      const lattice = status.network?.lattice
      const hexes = lattice?.hexes ?? []
      return {
        ...base,
        url: session.uri.url,
        name: lattice?.name ?? "",
        answeredBy: status.name,
        version: status.version,
        uptimeSeconds: status.uptime_seconds,
        regions: (lattice?.regions ?? []).map((code) => ({ code, ...place(code) })),
        hexes: hexes.map((h) => ({
          id: h.id,
          name: h.name,
          role: h.role,
          status: h.status,
          region: h.region ?? "",
          ...place(h.region),
          version: h.version,
          lag: h.lag,
          lagMs: h.lag_ms,
          slicesHeld: h.slices_held,
          slicing: h.slicing,
          replicationState: h.replication_state,
          lastSeen: h.last_seen,
          isSelf: h.is_self === true,
          preference: h.preference,
        })),
      }
    } catch (e) {
      return { ...base, error: `Couldn't read the lattice: ${errorMessage(e)}` }
    }
  }
}
