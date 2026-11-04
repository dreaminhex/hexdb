// HexDB for VS Code: connections, the lattice, tessellations, SQL and GraphQL, and stored functions.

import { parseConnectionUri } from "@dreaminhex/hexdb"
import * as vscode from "vscode"

import { Connections, errorMessage, Session, type SavedConnection } from "./connections"
import { HexDBFileSystem, scheme } from "./fs"
import { Functions } from "./functions"
import { LatticePanels } from "./lattice"
import { toConnectionUri, withoutCredentials } from "./lib"
import { Queries } from "./queries"
import { ResultsPanel } from "./results"
import { ConnectionsTree, type Node } from "./tree"

const playgroundInfo = "https://hexdb.app/api/playground"

export function activate(context: vscode.ExtensionContext): { connections: Connections; queries: Queries; results: ResultsPanel; tree: ConnectionsTree } {
  const connections = new Connections(context)
  const tree = new ConnectionsTree(connections)
  const results = new ResultsPanel(context)
  const fs = new HexDBFileSystem(connections)
  const queries = new Queries(context, connections, results)
  const lattices = new LatticePanels(context, connections)
  const functions = new Functions(connections, fs, results, () => tree.refresh())
  const view = vscode.window.createTreeView("hexdb.connections", { treeDataProvider: tree, showCollapseAll: true })

  fs.onDidSave(() => tree.refresh())

  /** The connection a command is about: the tree node's, or one the user picks. */
  const target = async (node?: Node): Promise<SavedConnection | undefined> =>
    node && "connection" in node ? node.connection : queries.pickConnection("Which HexDB connection?")

  const command = (id: string, run: (...args: never[]) => unknown) =>
    vscode.commands.registerCommand(id, async (...args: never[]) => {
      try {
        await run(...args)
      } catch (e) {
        vscode.window.showErrorMessage(errorMessage(e))
      }
    })

  context.subscriptions.push(
    connections,
    results,
    queries,
    lattices,
    view,
    vscode.workspace.registerFileSystemProvider(scheme, fs, { isCaseSensitive: true }),

    command("hexdb.addConnection", async () => {
      const saved = await connectionWizard(connections)
      if (saved) await view.reveal({ kind: "connection", connection: saved }, { expand: true }).then(undefined, () => undefined)
    }),
    command("hexdb.addPlayground", async () => {
      const saved = await addPlayground(connections)
      if (saved) await view.reveal({ kind: "connection", connection: saved }, { expand: true }).then(undefined, () => undefined)
    }),
    command("hexdb.editConnection", async (node?: Node) => {
      const connection = await target(node)
      if (connection) await connectionWizard(connections, connection)
    }),
    command("hexdb.removeConnection", async (node?: Node) => {
      const connection = await target(node)
      if (!connection) return
      const remove = "Remove"
      if ((await vscode.window.showWarningMessage(`Remove the connection ${connection.name}? Its saved credentials are deleted from VS Code.`, { modal: true }, remove)) === remove) {
        await connections.remove(connection.id)
      }
    }),
    command("hexdb.copyConnectionUri", async (node?: Node) => {
      const connection = await target(node)
      const uri = connection && (await connections.uriOf(connection.id))
      if (!uri) return
      await vscode.env.clipboard.writeText(withoutCredentials(uri))
      vscode.window.showInformationMessage("Copied the connection URI, without its credentials.")
    }),
    command("hexdb.signOut", async (node?: Node) => {
      const connection = await target(node)
      if (connection) await connections.signOut(connection.id)
    }),
    command("hexdb.refresh", () => tree.refresh()),
    command("hexdb.openLattice", async (node?: Node) => {
      const connection = await target(node)
      if (connection) lattices.show(connection)
    }),
    command("hexdb.newSqlQuery", async (node?: Node) => queries.newQuery("sql", await target(node))),
    command("hexdb.newGraphqlQuery", async (node?: Node) => queries.newQuery("graphql", await target(node))),
    command("hexdb.runQuery", () => queries.run()),
    command("hexdb.useConnection", () => queries.useConnection()),
    command("hexdb.browseTessellation", async (node?: Node) => {
      if (node?.kind === "tessellation") await queries.browse(node.connection, node.tessellation.name)
    }),
    command("hexdb.queryTessellation", async (node?: Node) => {
      if (node?.kind !== "tessellation") return
      const name = node.tessellation.name
      const ident = /^[A-Za-z_][A-Za-z0-9_]*$/.test(name) ? name : `"${name}"`
      await queries.newQuery("sql", node.connection, `SELECT *\nFROM ${ident}\nLIMIT 100;\n`)
    }),
    command("hexdb.openDocument", async (node?: Node) => {
      const connection = await target(node)
      if (!connection) return
      let tessellation = node?.kind === "tessellation" ? node.tessellation.name : undefined
      tessellation ??= await vscode.window.showInputBox({ title: "Open a document", prompt: "Tessellation" })
      if (!tessellation) return
      const id = await vscode.window.showInputBox({ title: `Open a document in ${tessellation}`, prompt: "Document ID" })
      if (id) await queries.openDocument(connection, tessellation, id.trim())
    }),
    command("hexdb.newFunction", async (node?: Node) => {
      const connection = await target(node)
      if (connection) await functions.create(connection)
    }),
    command("hexdb.openFunction", async (node?: Node) => {
      if (node?.kind === "function") await functions.open(node.connection, node.fn)
    }),
    command("hexdb.openFunctionDefinition", async (node?: Node) => {
      if (node?.kind === "function") await functions.open(node.connection, node.fn, true)
    }),
    command("hexdb.runFunction", async (node?: Node) => {
      if (node?.kind === "function") await functions.run(node.connection, node.fn)
    }),
    command("hexdb.deleteFunction", async (node?: Node) => {
      if (node?.kind === "function") await functions.delete(node.connection, node.fn)
    }),
  )

  // Internals for the integration tests (test/integration); not a public API.
  return { connections, queries, results, tree }
}

export function deactivate(): void {
  // Sessions end with the extension host; nothing to flush.
}

/** Add a connection, or edit one: URI, credentials if it has none, a name, then a test sign-in. */
async function connectionWizard(connections: Connections, existing?: SavedConnection): Promise<SavedConnection | undefined> {
  const previous = existing ? await connections.uriOf(existing.id) : undefined
  let value = previous ? withoutCredentials(previous) : ""
  for (;;) {
    const input = await vscode.window.showInputBox({
      title: existing ? `Edit ${existing.name}` : "Add a HexDB connection",
      prompt: existing
        ? "Connection URI. Leave the credentials out to keep the saved ones."
        : "Connection URI: hexdb://host:port (HTTP) or hexdbs://host (HTTPS), with ?apiKey=... or login:password@. An http(s) address works too.",
      placeHolder: "hexdbs://db.example.com?apiKey=hxk_...",
      value,
      ignoreFocusOut: true,
      validateInput: (v) => {
        try {
          toConnectionUri(v)
          return undefined
        } catch (e) {
          return e instanceof Error ? e.message : String(e)
        }
      },
    })
    if (input === undefined) return undefined
    value = input
    let uri = toConnectionUri(input)
    let parsed = parseConnectionUri(uri)

    if (parsed.login === null && parsed.apiKey === null) {
      const kept = previous ? parseConnectionUri(previous) : undefined
      if (kept && (kept.login !== null || kept.apiKey !== null)) {
        uri = toConnectionUri(uri, kept.apiKey !== null ? { apiKey: kept.apiKey } : { login: kept.login!, password: kept.password ?? "" })
      } else {
        const how = await vscode.window.showQuickPick(
          [
            { label: "API key", description: "Create one on the admin UI's Account page", key: "key" },
            { label: "Login and password", description: "Accounts with MFA are asked for a code", key: "login" },
          ],
          { title: "How do you sign in?", ignoreFocusOut: true },
        )
        if (!how) return undefined
        if (how.key === "key") {
          const apiKey = await vscode.window.showInputBox({ title: "API key", prompt: "hxk_...", password: true, ignoreFocusOut: true, validateInput: (v) => (v.trim() ? undefined : "Enter the API key") })
          if (!apiKey) return undefined
          uri = toConnectionUri(uri, { apiKey: apiKey.trim() })
        } else {
          const login = await vscode.window.showInputBox({ title: "Login", ignoreFocusOut: true, validateInput: (v) => (v.trim() ? undefined : "Enter your login") })
          if (!login) return undefined
          const password = await vscode.window.showInputBox({ title: `Password for ${login}`, password: true, ignoreFocusOut: true })
          if (password === undefined) return undefined
          uri = toConnectionUri(uri, { login: login.trim(), password })
        }
      }
      parsed = parseConnectionUri(uri)
    } else if (parsed.login !== null && parsed.password === null) {
      const password = await vscode.window.showInputBox({ title: `Password for ${parsed.login}`, password: true, ignoreFocusOut: true })
      if (password === undefined) return undefined
      uri = uri.replace(/^(hexdbs?:\/\/[^@/?#]*)@/i, `$1:${encodeURIComponent(password)}@`)
    }

    const host = new URL(parsed.url).host
    const name =
      existing?.name ??
      (await vscode.window.showInputBox({ title: "Name this connection", value: host, ignoreFocusOut: true, validateInput: (v) => (v.trim() ? undefined : "Enter a name") }))
    if (!name) return undefined

    const problem = await vscode.window.withProgress({ location: vscode.ProgressLocation.Notification, title: `Connecting to ${host}…` }, () => testConnection(name, uri))
    if (!problem) return connections.save(name.trim(), uri, existing?.id)
    const retry = "Edit URI"
    const keep = "Save Anyway"
    const choice = await vscode.window.showErrorMessage(`Couldn't connect to ${host}: ${problem}`, retry, keep)
    if (choice === keep) return connections.save(name.trim(), uri, existing?.id)
    if (choice !== retry) return undefined
  }
}

/** Sign in and read /auth/me; returns the problem, if any. */
async function testConnection(name: string, uri: string): Promise<string | undefined> {
  try {
    const session = new Session({ id: "test", name, display: "" }, uri, () =>
      vscode.window.showInputBox({ title: `Sign in to ${name}`, prompt: "One-time code from your authenticator app (or a backup code)", ignoreFocusOut: true }),
    )
    await session.open()
    if (session.uri.login !== null) await session.db.logout().catch(() => undefined)
    return undefined
  } catch (e) {
    return errorMessage(e)
  }
}

/** Add the public playground with the demo login the website publishes. */
async function addPlayground(connections: Connections): Promise<SavedConnection | undefined> {
  const existing = connections.list().find((c) => /hexdb-playground\.fly\.dev/i.test(c.display))
  if (existing) {
    vscode.window.showInformationMessage(`The playground is already connected as ${existing.name}.`)
    return existing
  }
  const info = await vscode.window.withProgress({ location: vscode.ProgressLocation.Notification, title: "Finding the HexDB playground…" }, async () => {
    const response = await fetch(playgroundInfo, { headers: { Accept: "application/json" } })
    if (!response.ok) throw new Error(`hexdb.app answered ${response.status}`)
    return (await response.json()) as { configured?: boolean; overseer?: { url?: string }; login?: { user?: string; password?: string } }
  })
  const url = info.overseer?.url
  const login = info.login
  if (!info.configured || !url || !login?.user || !login.password) throw new Error("The playground isn't available right now; try again in a minute.")
  const uri = toConnectionUri(url, { login: login.user, password: login.password })
  const saved = await connections.save("HexDB Playground", uri)
  vscode.window.showInformationMessage("Connected to the public playground. It's read-only except the sandbox tessellation, and resets every hour.")
  return saved
}
