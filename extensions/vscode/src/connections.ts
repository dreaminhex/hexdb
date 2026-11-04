// Saved connections and signed-in sessions. A connection's full URI, credentials included,
// is kept in VS Code's SecretStorage; global state holds only its name and a redacted URI.

import { HexDB, HexDBError, parseConnectionUri, redactConnectionUri, type ConnectionUri } from "@dreaminhex/hexdb"
import * as vscode from "vscode"

import type { Me } from "./lib"

export interface SavedConnection {
  id: string
  name: string
  /** The URI without its password or API key, for display. */
  display: string
}

type Method = "GET" | "POST" | "PUT" | "PATCH" | "DELETE"

const listKey = "hexdb.connections"
const secretKey = (id: string) => `hexdb.connection.${id}`

/** A signed-in connection to one lattice. */
export class Session {
  readonly db: HexDB
  readonly uri: ConnectionUri
  me: Me | undefined

  constructor(
    readonly connection: SavedConnection,
    uriText: string,
    private readonly askCode: () => PromiseLike<string | undefined>,
  ) {
    this.uri = parseConnectionUri(uriText)
    this.db = new HexDB({ url: uriText, retries: 2 })
  }

  async open(): Promise<void> {
    await this.signIn()
    this.me = await this.db.request<Me>("GET", "/auth/me")
  }

  private async signIn(): Promise<void> {
    const { login, password } = this.uri
    if (login === null) return
    if (password === null) throw new Error("The connection URI has a login but no password. Edit the connection to add one.")
    try {
      await this.db.login(login, password)
    } catch (e) {
      if (e instanceof HexDBError && e.code === "mfa_required") {
        const code = await this.askCode()
        if (!code) throw new Error("Sign-in needs a one-time code.")
        await this.db.login(login, password, code)
      } else {
        throw e
      }
    }
  }

  /** A REST request; signs in again once if a session expired. */
  async request<T>(method: Method, path: string, body?: unknown): Promise<T> {
    try {
      return await this.db.request<T>(method, path, body)
    } catch (e) {
      if (e instanceof HexDBError && e.status === 401 && this.uri.login !== null) {
        await this.signIn()
        return this.db.request<T>(method, path, body)
      }
      throw e
    }
  }

  get isAdmin(): boolean {
    return this.me?.is_admin === true
  }
}

export class Connections implements vscode.Disposable {
  private readonly sessions = new Map<string, Promise<Session>>()
  private readonly changed = new vscode.EventEmitter<void>()
  readonly onDidChange = this.changed.event

  constructor(private readonly context: vscode.ExtensionContext) {}

  dispose(): void {
    this.changed.dispose()
  }

  list(): SavedConnection[] {
    return this.context.globalState.get<SavedConnection[]>(listKey, [])
  }

  get(id: string): SavedConnection | undefined {
    return this.list().find((c) => c.id === id)
  }

  async uriOf(id: string): Promise<string | undefined> {
    return this.context.secrets.get(secretKey(id))
  }

  async save(name: string, uri: string, id?: string): Promise<SavedConnection> {
    const connection: SavedConnection = { id: id ?? newId(), name, display: redactConnectionUri(uri) }
    await this.context.secrets.store(secretKey(connection.id), uri)
    const list = this.list().filter((c) => c.id !== connection.id)
    const at = this.list().findIndex((c) => c.id === connection.id)
    list.splice(at < 0 ? list.length : at, 0, connection)
    await this.context.globalState.update(listKey, list)
    this.sessions.delete(connection.id)
    this.changed.fire()
    return connection
  }

  async remove(id: string): Promise<void> {
    await this.signOut(id)
    await this.context.secrets.delete(secretKey(id))
    await this.context.globalState.update(
      listKey,
      this.list().filter((c) => c.id !== id),
    )
    this.changed.fire()
  }

  /** The connection's session, signing in on first use. */
  session(id: string): Promise<Session> {
    let pending = this.sessions.get(id)
    if (!pending) {
      pending = this.open(id)
      this.sessions.set(id, pending)
      // A failed sign-in isn't cached: the next use tries again.
      pending.catch(() => this.sessions.delete(id))
    }
    return pending
  }

  /** A session that's already open, without signing in. */
  async openSession(id: string): Promise<Session | undefined> {
    const pending = this.sessions.get(id)
    return pending ? pending.catch(() => undefined) : undefined
  }

  isOpen(id: string): boolean {
    return this.sessions.has(id)
  }

  async signOut(id: string): Promise<void> {
    const pending = this.sessions.get(id)
    this.sessions.delete(id)
    const session = pending ? await pending.catch(() => undefined) : undefined
    if (session && session.uri.login !== null) await session.db.logout().catch(() => undefined)
    this.changed.fire()
  }

  private async open(id: string): Promise<Session> {
    const connection = this.get(id)
    const uri = await this.uriOf(id)
    if (!connection || !uri) throw new Error("That connection no longer exists.")
    const session = new Session(connection, uri, () =>
      vscode.window.showInputBox({ title: `Sign in to ${connection.name}`, prompt: "One-time code from your authenticator app (or a backup code)", ignoreFocusOut: true }),
    )
    await session.open()
    return session
  }
}

function newId(): string {
  return Date.now().toString(36) + Math.random().toString(36).slice(2, 8)
}

/** A readable message for any error from HexDB or the network. */
export function errorMessage(e: unknown): string {
  if (e instanceof HexDBError) return e.status ? `${e.message} (${e.status} ${e.code})` : e.message
  return e instanceof Error ? e.message : String(e)
}
