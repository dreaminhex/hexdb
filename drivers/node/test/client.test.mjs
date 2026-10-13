// Runs the Node.js driver against a real server (see drivers/testing/server.mjs).
import assert from "node:assert/strict"
import { after, before, test } from "node:test"

import { HexDB, HexDBError } from "../dist/index.js"
import { startServer } from "../../testing/server.mjs"

let server
let db

before(async () => {
  server = await startServer()
  db = new HexDB({ url: server.url, apiKey: server.apiKey })
})

after(async () => {
  await server?.stop()
})

test("documents: insert, get, patch, replace, delete", async () => {
  const notes = db.tessellation("notes")
  const doc = await notes.insert({ title: "hello", tags: ["a"] })
  assert.match(doc.id, /^[0-9A-Z]{26}$/)
  assert.equal((await notes.get(doc.id)).title, "hello")
  assert.equal((await notes.patch(doc.id, { title: "hi" })).title, "hi")
  assert.deepEqual((await notes.replace(doc.id, { body: "new" })).body, "new")
  assert.equal((await notes.get(doc.id)).title, undefined)
  assert.equal(await notes.delete(doc.id), true)
  assert.equal(await notes.get(doc.id), null)
  assert.equal(await notes.delete(doc.id), false)
})

test("queries, counts, iteration, aggregation, upsert", async () => {
  const orders = db.tessellation("orders")
  const ids = await orders.insertMany(Array.from({ length: 30 }, (_, i) => ({ n: i, status: i % 3 === 0 ? "paid" : "new", total: i * 10 })))
  assert.equal(ids.length, 30)
  const page = await orders.query({ filter: { status: "paid" }, sort: "-total", limit: 3, fields: ["total"] })
  assert.equal(page.total, 10)
  assert.deepEqual(page.documents.map((d) => d.total), [270, 240, 210])
  assert.equal(await orders.count({ total: { $gte: 100 } }), 20)
  let seen = 0
  for await (const _ of orders.iterate({ limit: 7 })) seen++
  assert.equal(seen, 30)
  const agg = await orders.aggregate({ group_by: ["status"], aggregates: { sum: { $sum: "total" } } })
  assert.equal(agg.rows.length, 2)
  const up = await orders.upsert(["n"], [{ n: 0, status: "void" }, { n: 99, status: "new" }])
  assert.deepEqual([up.inserted, up.replaced], [1, 1])
  assert.equal(up.ids[0], ids[0])
  const updated = await orders.updateWhere({ status: "void" }, { refunded: true })
  assert.equal(updated.modified, 1)
})

test("transactions, idempotency and errors", async () => {
  const result = await db.transaction([{ op: "insert", tessellation: "ledger", data: { amount: 5 } }], { idempotencyKey: "tx-1" })
  assert.equal(result.writes, 1)
  await db.transaction([{ op: "insert", tessellation: "ledger", data: { amount: 5 } }], { idempotencyKey: "tx-1" })
  assert.equal(await db.tessellation("ledger").count(), 1, "replayed, not written twice")
  await assert.rejects(db.tessellation("ledger").query({ filter: { $bogus: 1 } }), (e) => e instanceof HexDBError && e.status === 400)
  const anonymous = new HexDB({ url: server.url })
  await assert.rejects(anonymous.tessellation("ledger").count(), (e) => e instanceof HexDBError && e.status === 401 && e.code === "unauthorized")
})

test("sessions, GraphQL, functions", async () => {
  const session = new HexDB({ url: server.url })
  await session.login(server.adminLogin, server.adminPassword)
  assert.equal((await session.health()).status, "ok")
  const data = await session.graphql("query($t: String!) { count(tessellation: $t) }", { t: "orders" })
  assert.equal(data.count, 31)
  await db.request("POST", "/functions", { name: "big", kind: "query", tessellation: "orders", params: [{ name: "min", type: "number", default: 200 }], body: { filter: { total: { $gte: { $param: "min" } } } } })
  const result = await db.runFunction("big", { min: 250 })
  assert.equal(result.total, 5)
  await session.logout()
})

test("change feed and streams", async () => {
  const controller = new AbortController()
  const changes = db.changes({ tessellation: "feed", signal: controller.signal })
  const first = changes.next()
  await new Promise((r) => setTimeout(r, 300))
  const doc = await db.tessellation("feed").insert({ x: 1 })
  const { value } = await first
  assert.equal(value.id, doc.id)
  assert.equal(value.op, "put")
  controller.abort()

  await db.request("POST", "/streams", { name: "events" })
  const events = db.stream("events")
  const offsets = await events.publish({ payload: { n: 1 } }, { payload: { n: 2 }, key: "k" })
  assert.equal(offsets.length, 2)
  const read = await events.read({ limit: 10 })
  assert.deepEqual(read.messages.map((m) => m.payload.n), [1, 2])
  const got = []
  const stop = new AbortController()
  const consuming = events.consume("workers", async (m) => {
    got.push(m.payload.n)
    if (got.length === 2) stop.abort()
  }, { signal: stop.signal })
  await consuming
  assert.deepEqual(got, [1, 2])
  assert.equal((await events.read({ group: "workers" })).messages.length, 0, "committed")
})

test("files", async () => {
  const media = db.files("media")
  assert.equal((await media.create()).kind, "files")
  await media.create()
  const text = new TextEncoder().encode("Glacier lagoon field notes")
  const doc = await media.upload("notes/hello.txt", text)
  assert.equal(doc.path, "/notes/")
  assert.equal(doc.size, 26)
  assert.deepEqual(await media.download("notes/hello.txt"), text)
  assert.deepEqual(await media.download(doc.id), text)
  assert.equal(new TextDecoder().decode(await media.readRange(doc.id, 8, 14)), "lagoon")
  assert.equal((await media.stat("notes/hello.txt")).id, doc.id)
  assert.equal(await media.stat("notes/missing.txt"), null)
  assert.deepEqual((await media.list("notes")).map((f) => f.name), ["hello.txt"])

  const v2 = await media.upload("notes/hello.txt", new TextEncoder().encode("second draft"))
  assert.equal(v2.version, 2)
  assert.deepEqual(await media.download("notes/hello.txt", { version: 1 }), text)
  assert.equal((await media.restoreVersion(doc.id, 1)).version, 3)

  const big = new Uint8Array(17 * 1024 * 1024)
  for (let i = 0; i < big.length; i += 4096) big[i] = (i / 4096) % 251
  let progress = 0
  const bigDoc = await media.upload("raw/big.bin", big, { onProgress: (n) => (progress = n) })
  assert.equal(bigDoc.size, big.length)
  assert.equal(progress, big.length)
  const back = await media.download(bigDoc.id)
  assert.equal(back.length, big.length)
  assert.ok(back.every((b, i) => b === big[i]))

  for (let i = 0; i < 50 && (await media.get(doc.id)).extract.status !== "done"; i++) await new Promise((r) => setTimeout(r, 200))
  assert.deepEqual((await media.search("glacier")).map((f) => f.name), ["hello.txt"])

  assert.equal(await media.delete(doc.id), true)
  assert.deepEqual((await media.trash()).map((f) => f.id), [doc.id])
  assert.equal((await media.restore(doc.id)).name, "hello.txt")
  await media.delete(doc.id)
  await media.purge(doc.id)
  assert.deepEqual(await media.trash(), [])
})
