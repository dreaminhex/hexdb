// Quarantined files (HexDB 2.4+): uploads the malware scan flagged, which administrators release
// (a false positive: stored as a normal file) or delete. The HexDB icon in the activity bar shows
// how many are waiting across the signed-in connections, and a new one raises a notification.
// Only signed-in sessions are checked, so this never prompts for a sign-in on its own.

import { HexDBError } from "@dreaminhex/hexdb"
import * as vscode from "vscode"

import type { Connections, SavedConnection } from "./connections"

export interface QuarantinedFile {
  id: string
  tessellation: string
  path: string
  name: string
  size: number
  content_type?: string
  sha256?: string
  uploaded_by?: string
  uploaded_at?: string
  /** Set when a rescan, not the upload, found it. */
  rescan?: boolean
  scan: { status: string; detections: { engine: string; name: string }[]; engines: string[]; scanned_at?: string }
}

/** How often signed-in sessions are checked for new quarantined files. */
const POLL_MS = 60_000

export class Quarantine implements vscode.Disposable {
  /** Each connection's quarantined files, as last read (absent: not read, not an admin, or a HexDB before 2.4). */
  private readonly files = new Map<string, QuarantinedFile[]>()
  /** File IDs already announced (or present at the first read), per connection. */
  private readonly known = new Map<string, Set<string>>()
  private readonly changed = new vscode.EventEmitter<string>()
  /** Fires with a connection ID when its quarantine changed. */
  readonly onDidChange = this.changed.event
  private readonly timer: ReturnType<typeof setInterval>
  private view: vscode.TreeView<unknown> | undefined

  constructor(private readonly connections: Connections) {
    this.timer = setInterval(() => void this.poll(), POLL_MS)
    connections.onDidChange(() => {
      // A connection signed out or removed no longer counts.
      for (const id of [...this.files.keys()]) if (!connections.isOpen(id)) this.forget(id)
      this.updateBadge()
    })
  }

  dispose(): void {
    clearInterval(this.timer)
    this.changed.dispose()
  }

  /** The view whose activity bar icon carries the badge. */
  attach(view: vscode.TreeView<unknown>): void {
    this.view = view
    this.updateBadge()
  }

  /** The files last read for a connection, without asking the server. */
  cached(connectionId: string): QuarantinedFile[] | undefined {
    return this.files.get(connectionId)
  }

  /**
   * Reads a connection's quarantine. Null when it has none to show: the user isn't an
   * administrator, or the server is older than HexDB 2.4.
   */
  async list(connection: SavedConnection): Promise<QuarantinedFile[] | null> {
    const session = await this.connections.session(connection.id)
    if (!session.isAdmin) {
      this.forget(connection.id)
      return null
    }
    try {
      const { files } = await session.request<{ files: QuarantinedFile[] }>("GET", "/quarantine")
      this.record(connection, files)
      return files
    } catch (e) {
      if (e instanceof HexDBError && (e.status === 404 || e.status === 403)) {
        this.forget(connection.id)
        return null
      }
      throw e
    }
  }

  /** Stores the file as a normal file in its tessellation, after a confirmation. */
  async release(connection: SavedConnection, file: QuarantinedFile): Promise<void> {
    const matched = file.scan.detections.map((d) => d.name).join(", ") || "a malware signature"
    const yes = "Release"
    const answer = await vscode.window.showWarningMessage(
      `Release ${file.name}?`,
      {
        modal: true,
        detail: `It matched ${matched}. Releasing stores it in ${file.tessellation} as a normal file that anyone with access can download, and it replicates to every hex. Only release a file you're sure is safe.`,
      },
      yes,
    )
    if (answer !== yes) return
    const session = await this.connections.session(connection.id)
    await session.request("POST", `/quarantine/${encodeURIComponent(file.id)}/release`)
    vscode.window.showInformationMessage(`Released ${file.name} into ${file.tessellation}.`)
    await this.list(connection)
  }

  /** Deletes the file for good, after a confirmation. */
  async delete(connection: SavedConnection, file: QuarantinedFile): Promise<void> {
    const yes = "Delete"
    const answer = await vscode.window.showWarningMessage(`Delete ${file.name} for good?`, { modal: true, detail: "The file and its content are removed. This can't be undone." }, yes)
    if (answer !== yes) return
    const session = await this.connections.session(connection.id)
    await session.request("DELETE", `/quarantine/${encodeURIComponent(file.id)}`)
    vscode.window.showInformationMessage(`Deleted ${file.name}.`)
    await this.list(connection)
  }

  /** Asks the lattice to scan every stored file again now. */
  async rescan(connection: SavedConnection): Promise<void> {
    const session = await this.connections.session(connection.id)
    await session.request("POST", "/quarantine/rescan")
    vscode.window.showInformationMessage(`${connection.name} is rescanning every stored file. Anything it finds appears here.`)
  }

  /** Checks every signed-in administrator session; announces new files. */
  async poll(): Promise<void> {
    for (const connection of this.connections.list()) {
      const session = await this.connections.openSession(connection.id)
      if (!session?.isAdmin) continue
      try {
        await this.list(connection)
      } catch {
        // Unreachable for now: keep what was last read; the next round tries again.
      }
    }
  }

  private record(connection: SavedConnection, files: QuarantinedFile[]): void {
    const before = this.files.get(connection.id)
    const known = this.known.get(connection.id)
    this.files.set(connection.id, files)
    const ids = new Set(files.map((f) => f.id))
    if (known) {
      const fresh = files.filter((f) => !known.has(f.id))
      if (fresh.length) void this.announce(connection, fresh)
    }
    // The first read sets what's known without announcing it: the badge shows it.
    this.known.set(connection.id, ids)
    const same = before && before.length === files.length && before.every((f) => ids.has(f.id))
    this.updateBadge()
    if (!same) this.changed.fire(connection.id)
  }

  private async announce(connection: SavedConnection, fresh: QuarantinedFile[]): Promise<void> {
    const first = fresh[0]!
    const matched = first.scan.detections.map((d) => d.name).join(", ")
    const text =
      fresh.length === 1
        ? `HexDB quarantined ${first.name} on ${connection.name}${matched ? `: it matched ${matched}` : ""}.`
        : `HexDB quarantined ${fresh.length} files on ${connection.name}.`
    const show = "Show"
    if ((await vscode.window.showWarningMessage(text, show)) === show) {
      await vscode.commands.executeCommand("hexdb.connections.focus")
    }
  }

  private forget(connectionId: string): void {
    const had = this.files.delete(connectionId)
    this.known.delete(connectionId)
    if (had) this.changed.fire(connectionId)
  }

  private updateBadge(): void {
    if (!this.view) return
    const total = [...this.files.values()].reduce((n, files) => n + files.length, 0)
    this.view.badge = total
      ? { value: total, tooltip: `${total} quarantined ${total === 1 ? "file" : "files"}: uploads the malware scan flagged, waiting for an administrator` }
      : undefined
  }
}

/** A file's detections, readably: "Win.Trojan.Agent (ClamAV), EICAR_Test_File (YARA-X)". */
export function describeDetections(file: QuarantinedFile): string {
  return file.scan.detections.map((d) => `${d.name} (${d.engine})`).join(", ") || "flagged by the malware scan"
}

