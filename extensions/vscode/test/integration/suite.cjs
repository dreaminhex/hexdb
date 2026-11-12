// Runs inside VS Code's extension host (see run.mjs). Talks to a real lattice: the public
// playground by default (read-only checks), or HEXDB_TEST_URI. Under drivers/testing/server.mjs
// it seeds a few documents and also checks function editing as an administrator.
const assert = require("node:assert/strict")
const vscode = require("vscode")

const checks = []
const check = (name, fn) => checks.push({ name, fn })

async function until(what, fn, ms = 30000) {
  const end = Date.now() + ms
  for (;;) {
    const value = await fn()
    if (value) return value
    if (Date.now() > end) throw new Error(`Timed out waiting for ${what}`)
    await new Promise((r) => setTimeout(r, 200))
  }
}

let api
let connection

check("activates and registers its commands", async () => {
  const ext = vscode.extensions.getExtension("dreaminhex.hexdb")
  assert.ok(ext, "extension not found")
  api = await ext.activate()
  const commands = await vscode.commands.getCommands(true)
  for (const c of ["hexdb.addConnection", "hexdb.runQuery", "hexdb.openLattice", "hexdb.newFunction", "hexdb.runFunction"]) assert.ok(commands.includes(c), c)
})

check("adds a connection", async () => {
  if (process.env.HEXDB_TEST_URI) {
    connection = await api.connections.save("Test", process.env.HEXDB_TEST_URI)
  } else {
    await vscode.commands.executeCommand("hexdb.addPlayground")
    connection = api.connections.list()[0]
  }
  assert.ok(connection)
  assert.doesNotMatch(connection.display, /:[^*@/]+@/, "the display URI must not show the password")
  const session = await api.connections.session(connection.id)
  assert.ok(session.me.login)
  if (process.env.HEXDB_TEST_SEED === "1") {
    // A throwaway server (drivers/testing/server.mjs): give it what the playground has.
    for (const name of ["orders", "products"]) {
      await session.request("POST", "/tessellations", { name })
      for (let i = 0; i < 12; i++) await session.request("POST", `/${name}`, { n: i, status: i % 2 ? "paid" : "new" })
    }
    await session.request("POST", "/functions", { name: "paid_orders", kind: "query", tessellation: "orders", params: [{ name: "status", type: "string", default: "paid" }], body: { filter: { status: { $param: "status" } } } })
  }
})

check("the tree shows the lattice, tessellations and functions", async () => {
  const [root] = await api.tree.getChildren()
  const children = await api.tree.getChildren(root)
  const kinds = children.map((c) => c.kind)
  // Administrators of HexDB 2.4+ also see the quarantine.
  assert.deepEqual(kinds.filter((k) => k !== "message" && k !== "quarantine"), ["lattice", "tessellations", "functions", "deletes"])
  const hexes = await api.tree.getChildren(children[0])
  assert.ok(hexes.length >= 1)
  assert.ok(hexes.every((h) => h.kind === "hex"))
  const item = api.tree.getTreeItem(hexes[0])
  assert.match(String(item.description), /Overseer|Harvester|Replicant/)
  const tessellations = await api.tree.getChildren(children[1])
  assert.ok(tessellations.some((t) => t.kind === "tessellation"))
  const columns = await api.tree.getChildren(tessellations.find((t) => t.kind === "tessellation"))
  assert.ok(columns.length > 0)
})

check("runs SQL from an editor and pages", async () => {
  const doc = await vscode.workspace.openTextDocument({ language: "sql", content: "-- first\nSELECT 1;\n\nSELECT * FROM orders" })
  await api.queries.bind(doc, connection)
  const editor = await vscode.window.showTextDocument(doc)
  editor.selection = new vscode.Selection(new vscode.Position(3, 3), new vscode.Position(3, 3))
  await vscode.workspace.getConfiguration("hexdb").update("pageSize", 5, vscode.ConfigurationTarget.Global)
  await vscode.commands.executeCommand("hexdb.runQuery")
  const tab = await until("SQL results", () => api.results.latest?.rows && api.results.latest)
  assert.equal(tab.error, undefined)
  assert.equal(tab.source, "SELECT * FROM orders")
  assert.equal(tab.rows.length, 5)
  assert.equal(tab.tessellation, "orders")
  assert.equal(tab.hasMore, true)
})

check("runs GraphQL", async () => {
  const doc = await vscode.workspace.openTextDocument({ language: "graphql", content: '{ documents(tessellation: "products", limit: 3) { total documents { id data } } }' })
  await api.queries.bind(doc, connection)
  await vscode.window.showTextDocument(doc)
  await vscode.commands.executeCommand("hexdb.runQuery")
  const tab = await until("GraphQL results", () => api.results.latest?.source.includes("products") && api.results.latest.json && api.results.latest)
  assert.equal(tab.error, undefined)
  assert.equal(tab.rows.length, 3)
  assert.equal(tab.columns[0].name, "id")
  assert.equal(tab.tessellation, "products")
})

check("opens a document and a function as files", async () => {
  const session = await api.connections.session(connection.id)
  const { rows } = await session.request("POST", "/sql", { sql: "SELECT id FROM products LIMIT 1" })
  const doc = await vscode.workspace.openTextDocument(vscode.Uri.parse(`hexdbfs://${connection.id}/documents/products/${rows[0][0]}.json`))
  assert.equal(JSON.parse(doc.getText()).id, rows[0][0])
  const { functions } = await session.request("GET", "/functions")
  if (functions.length > 0) {
    const fn = await vscode.workspace.openTextDocument(vscode.Uri.parse(`hexdbfs://${connection.id}/functions/${functions[0].name}.json`))
    assert.equal(JSON.parse(fn.getText()).kind, functions[0].kind)
  }
})

check("runs a function", async () => {
  const session = await api.connections.session(connection.id)
  const { functions } = await session.request("GET", "/functions")
  const fn = functions.find((f) => f.kind === "query" && (f.params ?? []).every((p) => p.default !== undefined || !p.required))
  if (!fn) return
  // Accept every parameter's default.
  const original = vscode.window.showInputBox
  vscode.window.showInputBox = async (options) => options?.value ?? ""
  try {
    const tree = { kind: "function", connection, fn, admin: false }
    await vscode.commands.executeCommand("hexdb.runFunction", tree)
  } finally {
    vscode.window.showInputBox = original
  }
  const tab = await until("function results", () => api.results.latest?.title === fn.name && (api.results.latest.json !== undefined || api.results.latest.error) && api.results.latest)
  assert.equal(tab.error, undefined)
})

check("administrators see quarantined files, with a badge, and release or delete them", async () => {
  const session = await api.connections.session(connection.id)
  if (!session.isAdmin) return console.log("  (skipped: not an administrator)")
  if (process.env.HEXDB_TEST_SEED !== "1") return console.log("  (skipped: only on a throwaway server)")
  // The EICAR test file, which HexDB's built-in YARA rules flag (built from hex, as in hexdb_core).
  const hex = "58354F2150254041505B345C505A58353428505E2937434329377D2445494341522D5354414E444152442D414E544956495255532D544553542D46494C452124482B482A"
  const eicar = new Uint8Array(hex.match(/../g).map((b) => parseInt(b, 16)))
  await session.request("POST", "/tessellations", { name: "uploads", kind: "files" })
  for (const name of ["first.com", "second.com"]) {
    await assert.rejects(session.db.requestRaw("PUT", `/uploads/_files/inbox/${name}`, new Blob([eicar])), (e) => e.status === 422 && e.code === "file_quarantined")
  }

  // The tree shows them under Quarantine, and the badge counts them.
  const [root] = await api.tree.getChildren()
  const children = await api.tree.getChildren(root)
  const node = children.find((c) => c.kind === "quarantine")
  assert.ok(node, "no Quarantine node for an administrator")
  assert.equal(node.files.length, 2)
  assert.match(String(api.tree.getTreeItem(node).description), /2 files/)
  const files = await api.tree.getChildren(node)
  assert.deepEqual(files.map((f) => f.kind), ["quarantined", "quarantined"])
  assert.match(String(api.tree.getTreeItem(files[0]).description), /EICAR/)
  assert.equal(api.quarantine.cached(connection.id).length, 2)

  // Release one (a false positive) and delete the other; the confirmations are answered here.
  const byName = (name) => files.find((f) => f.file.name === name).file
  const original = vscode.window.showWarningMessage
  vscode.window.showWarningMessage = async (_text, _options, choice) => choice
  try {
    await api.quarantine.release(connection, byName("first.com"))
    await api.quarantine.delete(connection, byName("second.com"))
  } finally {
    vscode.window.showWarningMessage = original
  }
  assert.equal(api.quarantine.cached(connection.id).length, 0)
  const released = await session.db.requestRaw("GET", "/uploads/_files/inbox/first.com")
  assert.equal(released.status, 200)
  await assert.rejects(session.db.requestRaw("GET", "/uploads/_files/inbox/second.com"), (e) => e.status === 404)

  // With nothing left, the Quarantine node goes away.
  const after = await api.tree.getChildren(root)
  assert.equal(after.find((c) => c.kind === "quarantine"), undefined, "an empty Quarantine node is still shown")
})

check("the delete history lists deletes, restores one and purges another", async () => {
  if (process.env.HEXDB_TEST_SEED !== "1") return console.log("  (skipped: only on a throwaway server)")
  const session = await api.connections.session(connection.id)
  await session.request("POST", "/tessellations", { name: "scratch" })
  for (let i = 0; i < 4; i++) await session.request("POST", "/scratch", { n: i })
  const count = async () => (await session.request("GET", "/scratch/count")).count
  await session.request("POST", "/scratch/_delete", { filter: { n: { $lt: 2 } } })
  assert.equal(await count(), 2)

  const [root] = await api.tree.getChildren()
  const deletes = (await api.tree.getChildren(root)).find((c) => c.kind === "deletes")
  assert.ok(deletes, "no Deletes node")
  const items = await api.tree.getChildren(deletes)
  const first = items.find((i) => i.kind === "delete" && i.batch.tessellation === "scratch")
  assert.ok(first, JSON.stringify(items.map((i) => i.kind)))
  assert.equal(api.tree.getTreeItem(first).label, "2 documents from scratch")
  assert.equal(api.tree.getTreeItem(first).contextValue, "delete-restorable")

  const original = vscode.window.showWarningMessage
  vscode.window.showWarningMessage = async (_text, _options, choice) => choice
  try {
    assert.equal(await api.deletes.restore(api.connections, connection, first.batch), true)
    assert.equal(await count(), 4)
    await session.request("POST", "/scratch/_delete", { filter: { n: { $gte: 3 } } })
    const second = (await api.deletes.list(api.connections, connection)).find((b) => b.tessellation === "scratch" && !b.restored_at)
    assert.equal(await api.deletes.purge(api.connections, connection, second), true)
  } finally {
    vscode.window.showWarningMessage = original
  }
  const after = await api.tree.getChildren(deletes)
  const states = after.filter((i) => i.kind === "delete" && i.batch.tessellation === "scratch").map((i) => api.tree.getTreeItem(i).description)
  assert.ok(states.some((d) => /restored/.test(d)) && states.some((d) => /purged/.test(d)), JSON.stringify(states))
})

check("administrators edit, save, run and delete a script function", async () => {
  const session = await api.connections.session(connection.id)
  if (!session.isAdmin) return console.log("  (skipped: not an administrator)")
  const name = `vscode_test_${Date.now()}`
  await session.request("POST", "/functions", { name, kind: "script", runtime: "javascript", params: [], code: 'console.log(JSON.stringify({ v: 1 }))' })
  try {
    const uri = vscode.Uri.parse(`hexdbfs://${connection.id}/functions/${name}.js`)
    const doc = await vscode.workspace.openTextDocument(uri)
    assert.match(doc.getText(), /v: 1/)
    const edit = new vscode.WorkspaceEdit()
    edit.replace(uri, new vscode.Range(0, 0, doc.lineCount, 0), 'console.log(JSON.stringify({ v: 2 }))\n')
    assert.ok(await vscode.workspace.applyEdit(edit))
    assert.ok(await doc.save(), "save failed")
    const saved = await session.request("GET", `/functions/${name}`)
    assert.match(saved.code, /v: 2/)
    // The JSON definition saves too, and keeps the code.
    const defUri = vscode.Uri.parse(`hexdbfs://${connection.id}/functions/${name}.json`)
    const def = JSON.parse(new TextDecoder().decode(await vscode.workspace.fs.readFile(defUri)))
    def.description = "edited in VS Code"
    await vscode.workspace.fs.writeFile(defUri, Buffer.from(JSON.stringify(def)))
    const after = await session.request("GET", `/functions/${name}`)
    assert.equal(after.description, "edited in VS Code")
    assert.match(after.code, /v: 2/)
    const run = await session.request("POST", `/functions/${name}/run`, { params: {} })
    assert.deepEqual(run.result, { v: 2 })
  } finally {
    await session.request("DELETE", `/functions/${name}`).catch(() => undefined)
  }
})

check("removes the connection and its secret", async () => {
  await api.connections.remove(connection.id)
  assert.equal(api.connections.list().length, 0)
  assert.equal(await api.connections.uriOf(connection.id), undefined)
})

exports.run = async function run() {
  let failed = 0
  for (const { name, fn } of checks) {
    try {
      await fn()
      console.log(`ok - ${name}`)
    } catch (e) {
      failed++
      console.log(`not ok - ${name}\n  ${e && e.stack ? e.stack.split("\n").slice(0, 4).join("\n  ") : e}`)
    }
  }
  console.log(`# ${checks.length - failed} passed, ${failed} failed`)
  if (failed) throw new Error(`${failed} integration check(s) failed`)
}
