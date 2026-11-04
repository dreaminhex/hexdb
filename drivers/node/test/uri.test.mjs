// Connection URIs: the shared vectors (drivers/testing/connection-uris.json) and sign-in. No server needed.
import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { test } from "node:test"

import { HexDB, parseConnectionUri, redactConnectionUri } from "../dist/index.js"

const vectors = JSON.parse(readFileSync(new URL("../../testing/connection-uris.json", import.meta.url), "utf8"))

for (const v of vectors.valid) {
  test(`parses ${v.uri}`, () => {
    const { uri: _uri, ...expected } = v
    assert.deepEqual({ ...parseConnectionUri(v.uri) }, expected)
  })
}

for (const v of vectors.invalid) {
  test(`refuses ${v.uri} (${v.reason})`, () => {
    assert.throws(() => parseConnectionUri(v.uri), Error)
  })
}

test("redacts passwords and API keys", () => {
  assert.equal(redactConnectionUri("hexdbs://ada:s3cret@db.example.com?apiKey=hxk_1&timeout=5"), "hexdbs://ada:***@db.example.com?apiKey=***&timeout=5")
  assert.equal(redactConnectionUri("hexdb://localhost"), "hexdb://localhost")
})

function recorder(answers) {
  const calls = []
  const fetch = async (url, init) => {
    calls.push({ url, method: init.method, auth: init.headers.Authorization, body: init.body })
    const answer = answers(url)
    return new Response(JSON.stringify(answer), { status: 200, headers: { "Content-Type": "application/json" } })
  }
  return { calls, fetch }
}

test("a URI with a login signs in before the first request", async () => {
  const { calls, fetch } = recorder((url) => (url.endsWith("/auth/login") ? { token: "hxs.t", user: {}, expires_at: 1 } : { tessellations: [] }))
  const db = new HexDB({ url: "hexdbs://ada:p%40ss@db.example.com/base", fetch })
  assert.equal(db.url, "https://db.example.com/base")
  await Promise.all([db.request("GET", "/tessellations"), db.request("GET", "/tessellations")])
  assert.equal(calls.filter((c) => c.url.endsWith("/auth/login")).length, 1)
  assert.deepEqual(JSON.parse(calls[0].body), { login: "ada", password: "p@ss", return_token: true })
  assert.equal(calls[1].url, "https://db.example.com/base/tessellations")
  assert.equal(calls[1].auth, "Bearer hxs.t")
})

test("HexDB.connect signs in at once; an apiKey is sent as a bearer token", async () => {
  const { calls, fetch } = recorder(() => ({ token: "hxs.t", user: {}, expires_at: 1 }))
  await HexDB.connect({ url: "hexdb://ada:pw@localhost", fetch })
  assert.equal(calls.length, 1)
  assert.equal(calls[0].url, "http://localhost:7700/auth/login")

  const keyed = recorder(() => ({}))
  const db = new HexDB({ url: "hexdb://localhost:7710?apiKey=hxk_1", fetch: keyed.fetch })
  await db.request("GET", "/auth/me")
  assert.equal(keyed.calls[0].auth, "Bearer hxk_1")
  assert.equal(keyed.calls[0].url, "http://localhost:7710/auth/me")
})

test("an explicit apiKey wins over the URI's", async () => {
  const { calls, fetch } = recorder(() => ({}))
  const db = new HexDB({ url: "hexdb://ada:pw@localhost", apiKey: "hxk_explicit", fetch })
  await db.request("GET", "/auth/me")
  assert.equal(calls.length, 1)
  assert.equal(calls[0].auth, "Bearer hxk_explicit")
})
