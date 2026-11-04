// Unit tests for the extension's pure helpers (built to out/ by esbuild.mjs).
import assert from "node:assert/strict"
import { test } from "node:test"

import { can, duration, graphqlTessellation, parseParam, singleTable, sqlIdent, statementAt, tabulate, toConnectionUri, toCsv, withoutCredentials } from "../out/lib.mjs"
import { describeRegion, place } from "../out/regions.mjs"

test("connection URIs from what was typed", () => {
  assert.equal(toConnectionUri("hexdbs://db.example.com"), "hexdbs://db.example.com")
  assert.equal(toConnectionUri("http://127.0.0.1:7700"), "hexdb://127.0.0.1:7700")
  assert.equal(toConnectionUri("127.0.0.1:7710"), "hexdb://127.0.0.1:7710")
  assert.equal(toConnectionUri("https://hexdb-playground.fly.dev/"), "hexdbs://hexdb-playground.fly.dev")
  // An http URL without a port is port 80, not hexdb://'s 7700.
  assert.equal(toConnectionUri("http://db.example.com"), "hexdb://db.example.com:80")
  assert.equal(toConnectionUri("https://x.example.com/hexdb", { apiKey: "hxk_a+b" }), "hexdbs://x.example.com/hexdb?apiKey=hxk_a%2Bb")
  assert.equal(toConnectionUri("hexdbs://x.example.com?timeout=5", { apiKey: "k" }), "hexdbs://x.example.com?timeout=5&apiKey=k")
  assert.equal(toConnectionUri("hexdbs://x.example.com", { login: "ada@corp", password: "p:w@" }), "hexdbs://ada%40corp:p%3Aw%40@x.example.com")
  assert.throws(() => toConnectionUri("mongodb://x"), /hexdb:\/\//)
  assert.throws(() => toConnectionUri("hexdb://a:b@x", { apiKey: "k" }), /already has credentials/)
  assert.throws(() => toConnectionUri("hexdb://x?pageSize=0"), /pageSize/)
})

test("credentials are stripped for sharing", () => {
  assert.equal(withoutCredentials("hexdbs://ada:pw@x.example.com?apiKey=k&timeout=5"), "hexdbs://x.example.com?timeout=5")
  assert.equal(withoutCredentials("hexdbs://x.example.com?timeout=5&apiKey=k"), "hexdbs://x.example.com?timeout=5")
  assert.equal(withoutCredentials("hexdbs://x.example.com?apiKey=k"), "hexdbs://x.example.com")
})

test("permissions follow the server's rules", () => {
  const me = { login: "p", is_admin: false, grants: [{ permissions: ["read"], tessellations: ["*"] }, { permissions: ["read", "write"], tessellations: ["sandbox"] }] }
  assert.equal(can(me, "read", "orders"), true)
  assert.equal(can(me, "write", "orders"), false)
  assert.equal(can(me, "write", "sandbox"), true)
  assert.equal(can({ login: "a", is_admin: true, grants: [] }, "write", "orders"), true)
  assert.equal(can(undefined, "read", "orders"), false)
})

test("SQL helpers", () => {
  assert.equal(sqlIdent("orders"), "orders")
  assert.equal(sqlIdent("sandbox-events"), '"sandbox-events"')
  assert.equal(singleTable("SELECT * FROM orders WHERE x = 1"), "orders")
  assert.equal(singleTable('select id from "my-tess" -- from other'), "my-tess")
  assert.equal(singleTable("SELECT * FROM a JOIN b ON a.id = b.a"), undefined)
  assert.equal(singleTable("SELECT * FROM a WHERE id IN (SELECT a FROM b)"), undefined)
})

test("the statement at the cursor", () => {
  const text = "-- intro; not a split\nSELECT 1;\n\nSELECT 'a;b' FROM t;\n  SELECT 3"
  assert.equal(statementAt(text, text.indexOf("SELECT 1") + 3).text, "-- intro; not a split\nSELECT 1")
  assert.equal(statementAt(text, text.indexOf("'a;b'")).text, "SELECT 'a;b' FROM t")
  assert.equal(statementAt(text, text.length).text, "SELECT 3")
  assert.equal(statementAt("   ", 0).text, "")
  // A cursor on an empty line after the last statement runs the last one.
  assert.equal(statementAt("SELECT 1;\n\n", 11).text, "SELECT 1")
})

test("lists of objects become tables", () => {
  const t = tabulate({ data: { orders: [{ total: 1, id: "a" }, { id: "b", status: "paid" }] } })
  assert.deepEqual(t.columns.map((c) => c.name), ["id", "total", "status"])
  assert.deepEqual(t.rows, [["a", 1, null], ["b", null, "paid"]])
  assert.deepEqual(tabulate({ documents: [{ id: "x" }], total: 1 }).rows, [["x"]])
  assert.equal(tabulate({ data: { count: 3 } }), undefined)
  assert.equal(tabulate([1, 2]), undefined)
  const docs = tabulate({ data: { documents: { total: 9, documents: [{ id: "a", data: { total: 5 } }] } } })
  assert.deepEqual(docs.columns.map((c) => c.name), ["id", "total"])
  assert.deepEqual(docs.rows, [["a", 5]])
  assert.equal(graphqlTessellation('{ documents(tessellation: "orders") { total } count(tessellation: "orders") }'), "orders")
  assert.equal(graphqlTessellation('{ a: count(tessellation: "x") b: count(tessellation: "y") }'), undefined)
})

test("CSV quoting", () => {
  assert.equal(toCsv([{ name: "a" }, { name: "b" }], [["x,y", { k: 1 }], [null, 'say "hi"']]), 'a,b\r\n"x,y","{""k"":1}"\r\n,"say ""hi"""\r\n')
})

test("function parameters are typed", () => {
  assert.equal(parseParam("integer", "42"), 42)
  assert.throws(() => parseParam("integer", "4.2"))
  assert.equal(parseParam("number", "4.5"), 4.5)
  assert.equal(parseParam("boolean", "TRUE"), true)
  assert.deepEqual(parseParam("array", "[1,2]"), [1, 2])
  assert.equal(parseParam("any", "plain text"), "plain text")
  assert.throws(() => parseParam("object", "{bad"))
  assert.equal(parseParam("string", " keep spaces "), " keep spaces ")
})

test("regions and durations", () => {
  assert.deepEqual(place("ORD"), { city: "Chicago", country: "United States" })
  assert.equal(describeRegion("lhr"), "London (lhr)")
  assert.equal(describeRegion("mars-1"), "mars-1")
  assert.equal(describeRegion(""), "No region")
  assert.equal(duration(64523), "17h 55m")
  assert.equal(duration(5), "5s")
})
