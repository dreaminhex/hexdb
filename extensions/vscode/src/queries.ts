// Running SQL and GraphQL from editors. Each query editor is bound to a connection (shown in the
// status bar); Ctrl+Enter runs the statement under the cursor, or the selection.

import * as vscode from "vscode"

import { errorMessage, type Connections, type SavedConnection, type Session } from "./connections"
import { documentUri } from "./fs"
import { graphqlTessellation, singleTable, sqlIdent, statementAt, tabulate } from "./lib"
import type { ResultsPanel } from "./results"
import type { Column, ResultTab } from "./shared/messages"

interface SqlAnswer {
  columns: Column[]
  rows: unknown[][]
  next: string | null
  plan?: unknown
}

const bindingsKey = "hexdb.editorConnections"

export class Queries implements vscode.Disposable {
  private readonly status: vscode.StatusBarItem
  private readonly disposables: vscode.Disposable[] = []
  private readonly catalog = new Map<string, { at: number; tables: Promise<Map<string, Column[]>> }>()

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly connections: Connections,
    private readonly results: ResultsPanel,
  ) {
    this.status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 50)
    this.status.command = "hexdb.useConnection"
    this.disposables.push(
      this.status,
      vscode.window.onDidChangeActiveTextEditor(() => this.updateStatus()),
      connections.onDidChange(() => this.updateStatus()),
      vscode.languages.registerCompletionItemProvider({ language: "sql" }, { provideCompletionItems: (doc, pos) => this.complete(doc, pos) }, ".", " ", '"'),
    )
    this.updateStatus()
  }

  dispose(): void {
    for (const d of this.disposables) d.dispose()
  }

  // ---- bindings ----------------------------------------------------------

  private bindings(): Record<string, string> {
    return this.context.workspaceState.get<Record<string, string>>(bindingsKey, {})
  }

  bound(doc: vscode.TextDocument): SavedConnection | undefined {
    const id = this.bindings()[doc.uri.toString()]
    return id ? this.connections.get(id) : undefined
  }

  async bind(doc: vscode.TextDocument, connection: SavedConnection): Promise<void> {
    const all = { ...this.bindings(), [doc.uri.toString()]: connection.id }
    // Forget bindings of untitled editors that are gone.
    const open = new Set(vscode.workspace.textDocuments.map((d) => d.uri.toString()))
    for (const key of Object.keys(all)) if (key.startsWith("untitled:") && !open.has(key)) delete all[key]
    await this.context.workspaceState.update(bindingsKey, all)
    this.updateStatus()
  }

  private updateStatus(): void {
    const editor = vscode.window.activeTextEditor
    const lang = editor?.document.languageId
    if (!editor || (lang !== "sql" && lang !== "graphql")) {
      this.status.hide()
      void vscode.commands.executeCommand("setContext", "hexdb.editorBound", false)
      return
    }
    const connection = this.bound(editor.document)
    this.status.text = connection ? `$(database) HexDB: ${connection.name}` : "$(database) HexDB: choose a connection"
    this.status.tooltip = connection ? `${connection.display}\nCtrl+Enter runs the statement at the cursor. Click to change.` : "Choose the HexDB connection this editor's queries run on"
    this.status.show()
    void vscode.commands.executeCommand("setContext", "hexdb.editorBound", connection !== undefined)
  }

  /** Ask which connection to use (the only one, if there is one). */
  async pickConnection(placeHolder = "Run on which HexDB connection?"): Promise<SavedConnection | undefined> {
    const list = this.connections.list()
    if (list.length === 0) {
      const add = "Add Connection"
      if ((await vscode.window.showInformationMessage("Add a HexDB connection first.", add)) === add) await vscode.commands.executeCommand("hexdb.addConnection")
      return undefined
    }
    if (list.length === 1) return list[0]
    const pick = await vscode.window.showQuickPick(
      list.map((c) => ({ label: c.name, description: c.display, connection: c })),
      { placeHolder },
    )
    return pick?.connection
  }

  async useConnection(): Promise<void> {
    const editor = vscode.window.activeTextEditor
    if (!editor) return
    const connection = await this.pickConnection("Use which HexDB connection for this editor?")
    if (connection) await this.bind(editor.document, connection)
  }

  // ---- new editors -------------------------------------------------------

  async newQuery(language: "sql" | "graphql", connection?: SavedConnection, content?: string): Promise<void> {
    connection ??= await this.pickConnection("New query on which connection?")
    if (!connection) return
    const text =
      content ??
      (language === "sql"
        ? `-- ${connection.name}: Ctrl+Enter runs the statement at the cursor (or the selection).\n-- Tessellations are tables; see the manual's SQL chapter.\n\nSELECT *\nFROM \nLIMIT 100;\n`
        : `# ${connection.name}: Ctrl+Enter runs the query (or the selection).\n# Fields: documents, document, count, aggregate and more; see the manual's GraphQL chapter.\n\n{\n  documents(tessellation: "", limit: 20) {\n    total\n    documents { id data }\n  }\n}\n`)
    const doc = await vscode.workspace.openTextDocument({ language, content: text })
    await this.bind(doc, connection)
    const editor = await vscode.window.showTextDocument(doc)
    if (content === undefined) {
      // After "FROM " in SQL, between the tessellation's quotes in GraphQL.
      const pos = new vscode.Position(4, language === "sql" ? 5 : 27)
      editor.selection = new vscode.Selection(pos, pos)
    }
  }

  // ---- running -----------------------------------------------------------

  async run(): Promise<void> {
    const editor = vscode.window.activeTextEditor
    if (!editor) return
    const doc = editor.document
    let connection = this.bound(doc)
    if (!connection) {
      connection = await this.pickConnection()
      if (!connection) return
      await this.bind(doc, connection)
    }
    const selected = editor.selection.isEmpty ? undefined : doc.getText(editor.selection)
    if (doc.languageId === "graphql") {
      const query = (selected ?? doc.getText()).trim()
      if (!query) return
      await this.runGraphql(connection, query)
    } else {
      const statement = selected?.trim() || statementAt(doc.getText(), doc.offsetAt(editor.selection.active)).text
      if (!statement) {
        vscode.window.showInformationMessage("There's no SQL statement at the cursor.")
        return
      }
      await this.runSql(connection, statement)
    }
  }

  async runSql(connection: SavedConnection, sql: string, title?: string, tessellation?: string): Promise<void> {
    const id = this.results.newId()
    const label = title ?? (tessellation ? tessellation : `SQL ${id.slice(1)}`)
    const pending: ResultTab = { id, title: label, connection: connection.name, source: sql }
    this.results.show(pending)
    const pageSize = vscode.workspace.getConfiguration("hexdb").get<number>("pageSize", 200)
    const started = Date.now()
    let session: Session
    try {
      session = await this.connections.session(connection.id)
      const answer = await session.request<SqlAnswer>("POST", "/sql", { sql, page_size: pageSize })
      const table = tessellation ?? singleTable(sql)
      const tab: ResultTab = {
        ...pending,
        ms: Date.now() - started,
        columns: answer.columns,
        rows: answer.rows,
        hasMore: answer.next !== null,
        tessellation: table,
      }
      let cursor = answer.next
      this.results.show(tab, {
        loadMore: async (current) => {
          const more = await session.request<SqlAnswer>("POST", "/sql", { sql, page_size: pageSize, cursor })
          cursor = more.next
          return { ...current, rows: [...(current.rows ?? []), ...more.rows], hasMore: more.next !== null }
        },
        openDocument: table ? (docId) => void this.openDocument(connection, table, docId) : undefined,
      })
    } catch (e) {
      this.results.show({ ...pending, ms: Date.now() - started, error: errorMessage(e) })
    }
  }

  async runGraphql(connection: SavedConnection, query: string): Promise<void> {
    const id = this.results.newId()
    const pending: ResultTab = { id, title: `GraphQL ${id.slice(1)}`, connection: connection.name, source: query }
    this.results.show(pending)
    const started = Date.now()
    try {
      const session = await this.connections.session(connection.id)
      const answer = await session.request<{ data?: unknown; errors?: { message: string; path?: unknown[] }[] }>("POST", "/graphql", { query })
      const ms = Date.now() - started
      const errors = answer.errors?.map((e) => (e.path ? `${e.message} (at ${e.path.join(".")})` : e.message))
      if (errors?.length && (answer.data === null || answer.data === undefined)) {
        this.results.show({ ...pending, ms, error: errors.join("\n") })
        return
      }
      const table = tabulate(answer.data)
      const tessellation = graphqlTessellation(query)
      this.results.show(
        { ...pending, ms, json: answer, columns: table?.columns, rows: table?.rows, notes: errors, tessellation },
        { openDocument: tessellation ? (docId) => void this.openDocument(connection, tessellation, docId) : undefined },
      )
    } catch (e) {
      this.results.show({ ...pending, ms: Date.now() - started, error: errorMessage(e) })
    }
  }

  /** Show a tessellation's documents, a page at a time. */
  browse(connection: SavedConnection, tessellation: string): Promise<void> {
    return this.runSql(connection, `SELECT * FROM ${sqlIdent(tessellation)}`, tessellation, tessellation)
  }

  async openDocument(connection: SavedConnection, tessellation: string, id: string): Promise<void> {
    try {
      const doc = await vscode.workspace.openTextDocument(documentUri(connection.id, tessellation, id))
      await vscode.window.showTextDocument(doc, { preview: true, viewColumn: vscode.ViewColumn.One })
    } catch (e) {
      vscode.window.showErrorMessage(`Couldn't open ${tessellation}/${id}: ${errorMessage(e)}`)
    }
  }

  // ---- completion --------------------------------------------------------

  private tables(connection: SavedConnection): Promise<Map<string, Column[]>> {
    const cached = this.catalog.get(connection.id)
    if (cached && Date.now() - cached.at < 60_000) return cached.tables
    const tables = (async () => {
      const session = await this.connections.session(connection.id)
      const { tables } = await session.request<{ tables: { name: string }[] }>("GET", "/sql/tables")
      const entries = await Promise.all(
        tables.map(async (t) => {
          const { columns } = await session.request<{ columns: Column[] }>("GET", `/sql/columns?table=${encodeURIComponent(t.name)}`).catch(() => ({ columns: [] as Column[] }))
          return [t.name, columns] as const
        }),
      )
      return new Map(entries)
    })()
    tables.catch(() => this.catalog.delete(connection.id))
    this.catalog.set(connection.id, { at: Date.now(), tables })
    return tables
  }

  private async complete(doc: vscode.TextDocument, position: vscode.Position): Promise<vscode.CompletionItem[]> {
    const connection = this.bound(doc)
    if (!connection) return []
    let tables: Map<string, Column[]>
    try {
      tables = await this.tables(connection)
    } catch {
      return []
    }
    const before = doc.lineAt(position).text.slice(0, position.character)
    const dotted = /("?)([A-Za-z0-9_-]+)\1\.\w*$/.exec(before)
    const items: vscode.CompletionItem[] = []
    const column = (c: Column, table: string) => {
      const item = new vscode.CompletionItem(c.name, vscode.CompletionItemKind.Field)
      item.detail = `${table}${c.type ? ` · ${c.type}` : ""}`
      item.sortText = `0${c.name}`
      return item
    }
    if (dotted && tables.has(dotted[2])) return tables.get(dotted[2])!.map((c) => column(c, dotted[2]))
    for (const name of tables.keys()) {
      const item = new vscode.CompletionItem(name, vscode.CompletionItemKind.Class)
      item.detail = "tessellation"
      item.insertText = sqlIdent(name)
      item.sortText = `1${name}`
      items.push(item)
    }
    // Columns of the tables the statement names.
    const text = doc.getText()
    for (const [name, columns] of tables) {
      if (new RegExp(`\\b${name.replace(/[-]/g, "\\-")}\\b`, "i").test(text)) for (const c of columns) items.push(column(c, name))
    }
    return items
  }
}
