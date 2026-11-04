// Stored functions: create, open, run and delete. Editing and saving goes through the file system
// provider (fs.ts); creating, changing and deleting need an administrator, running needs only a sign-in.

import * as vscode from "vscode"

import { errorMessage, type Connections, type SavedConnection } from "./connections"
import { functionUri, type HexDBFileSystem } from "./fs"
import { parseParam, tabulate } from "./lib"
import type { ResultsPanel } from "./results"
import type { FunctionInfo } from "./tree"

const templates: Record<string, (name: string) => Record<string, unknown>> = {
  python: (name) => ({
    kind: "script",
    runtime: "python",
    description: "",
    timeout_seconds: 30,
    params: [{ name: "name", type: "string", default: "world" }],
    code: [
      "import json, sys",
      "",
      "# Input on stdin: {\"params\", \"function\", \"caller\"}. Print the result as JSON.",
      "# HEXDB_API and HEXDB_TOKEN (a session of the caller) are in the environment.",
      "request = json.load(sys.stdin)",
      "params = request[\"params\"]",
      `print(json.dumps({"greeting": f"Hello, {params['name']}!", "function": "${name}"}))`,
      "",
    ].join("\n"),
  }),
  typescript: (name) => ({
    kind: "script",
    runtime: "typescript",
    description: "",
    timeout_seconds: 30,
    params: [{ name: "name", type: "string", default: "world" }],
    code: [
      "// Input on stdin: {params, function, caller}. Print the result as JSON.",
      "// HEXDB_API and HEXDB_TOKEN (a session of the caller) are in the environment.",
      "import { readFileSync } from \"node:fs\"",
      "",
      "const request: { params: { name: string } } = JSON.parse(readFileSync(0, \"utf8\"))",
      `console.log(JSON.stringify({ greeting: \`Hello, \${request.params.name}!\`, function: "${name}" }))`,
      "",
    ].join("\n"),
  }),
  javascript: (name) => ({
    kind: "script",
    runtime: "javascript",
    description: "",
    timeout_seconds: 30,
    params: [{ name: "name", type: "string", default: "world" }],
    code: [
      "// Input on stdin: {params, function, caller}. Print the result as JSON.",
      "// HEXDB_API and HEXDB_TOKEN (a session of the caller) are in the environment.",
      "const request = JSON.parse(require(\"node:fs\").readFileSync(0, \"utf8\"))",
      `console.log(JSON.stringify({ greeting: \`Hello, \${request.params.name}!\`, function: "${name}" }))`,
      "",
    ].join("\n"),
  }),
  query: () => ({
    kind: "query",
    tessellation: "orders",
    description: "",
    params: [{ name: "status", type: "string", required: true }],
    body: { filter: { status: { $param: "status" } }, sort: "-created", limit: 50 },
  }),
  aggregate: () => ({
    kind: "aggregate",
    tessellation: "orders",
    description: "",
    params: [],
    body: { group_by: ["status"], aggregates: { orders: { $count: "*" } } },
  }),
  transaction: () => ({
    kind: "transaction",
    description: "",
    params: [{ name: "id", type: "string", required: true }],
    body: { operations: [{ op: "patch", tessellation: "orders", id: { $param: "id" }, data: { status: "paid" } }] },
  }),
}

export class Functions {
  constructor(
    private readonly connections: Connections,
    private readonly fs: HexDBFileSystem,
    private readonly results: ResultsPanel,
    private readonly refresh: () => void,
  ) {}

  async open(connection: SavedConnection, fn: FunctionInfo, definition = false): Promise<void> {
    const session = await this.connections.session(connection.id)
    // Only administrators see a script's code; others get the definition they can see.
    const uri = functionUri(connection.id, fn, definition || !session.isAdmin)
    try {
      await vscode.window.showTextDocument(await vscode.workspace.openTextDocument(uri), { preview: false })
    } catch (e) {
      vscode.window.showErrorMessage(`Couldn't open ${fn.name}: ${errorMessage(e)}`)
    }
  }

  async create(connection: SavedConnection): Promise<void> {
    const session = await this.connections.session(connection.id)
    if (!session.isAdmin) {
      vscode.window.showWarningMessage("Only administrators can create functions.")
      return
    }
    const kind = await vscode.window.showQuickPick(
      [
        { label: "Python script", description: "Runs python on the server", key: "python" },
        { label: "TypeScript script", description: "Runs Node.js 22.6+ with type stripping", key: "typescript" },
        { label: "JavaScript script", description: "Runs node", key: "javascript" },
        { label: "Query", description: "A filtered, sorted query on one tessellation", key: "query" },
        { label: "Aggregate", description: "An aggregation on one tessellation", key: "aggregate" },
        { label: "Transaction", description: "Operations across tessellations, atomic", key: "transaction" },
      ],
      { title: "New function", placeHolder: "What kind of function?" },
    )
    if (!kind) return
    const name = await vscode.window.showInputBox({
      title: "New function",
      prompt: "Name (letters, digits, _ and -)",
      validateInput: (v) => (/^[A-Za-z0-9_-]{1,64}$/.test(v) ? undefined : "1 to 64 letters, digits, _ or -"),
    })
    if (!name) return
    const def: Record<string, unknown> = { name, ...templates[kind.key](name) }
    try {
      await session.request("POST", "/functions", def)
    } catch (e) {
      vscode.window.showErrorMessage(`Couldn't create ${name}: ${errorMessage(e)}`)
      return
    }
    this.refresh()
    const info: FunctionInfo = { name, kind: def.kind as string, runtime: def.runtime as string | undefined }
    // A query, aggregate or transaction is all definition; a script opens on its code.
    await this.open(connection, info, info.kind !== "script")
  }

  async run(connection: SavedConnection, fn: FunctionInfo): Promise<void> {
    const params: Record<string, unknown> = {}
    for (const p of fn.params ?? []) {
      const def = p.default === undefined ? "" : typeof p.default === "string" ? p.default : JSON.stringify(p.default)
      const value = await vscode.window.showInputBox({
        title: `Run ${fn.name}`,
        prompt: `${p.name} (${p.type}${p.required ? ", required" : ", optional: leave empty for the default"})${p.description ? `: ${p.description}` : ""}`,
        value: def,
        ignoreFocusOut: true,
        validateInput: (v) => {
          if (v === "" && !p.required) return undefined
          try {
            parseParam(p.type, v)
            return undefined
          } catch (e) {
            return e instanceof Error ? e.message : String(e)
          }
        },
      })
      if (value === undefined) return
      if (value !== "" || p.required) params[p.name] = parseParam(p.type, value)
    }
    const id = this.results.newId()
    const source = `${fn.name}(${Object.entries(params)
      .map(([k, v]) => `${k}: ${JSON.stringify(v)}`)
      .join(", ")})`
    const pending = { id, title: fn.name, connection: connection.name, source }
    this.results.show(pending)
    const started = Date.now()
    try {
      const session = await this.connections.session(connection.id)
      const answer = await session.request<{ result: unknown; ms?: number }>("POST", `/functions/${encodeURIComponent(fn.name)}/run`, { params })
      const table = tabulate(answer.result)
      this.results.show({ ...pending, ms: answer.ms ?? Date.now() - started, json: answer.result, columns: table?.columns, rows: table?.rows, tessellation: fn.tessellation })
    } catch (e) {
      this.results.show({ ...pending, ms: Date.now() - started, error: errorMessage(e) })
    }
  }

  async delete(connection: SavedConnection, fn: FunctionInfo): Promise<void> {
    const yes = "Delete"
    const answer = await vscode.window.showWarningMessage(`Delete the function ${fn.name} from ${connection.name}? Schedules and triggers that use it will fail.`, { modal: true }, yes)
    if (answer !== yes) return
    try {
      const session = await this.connections.session(connection.id)
      await session.request("DELETE", `/functions/${encodeURIComponent(fn.name)}`)
    } catch (e) {
      vscode.window.showErrorMessage(`Couldn't delete ${fn.name}: ${errorMessage(e)}`)
      return
    }
    for (const definition of [false, true]) this.fs.forget(functionUri(connection.id, fn, definition))
    this.refresh()
  }
}
