// Tests for the plugin SDK's API client: node --test plugins/sdk/hexdb-plugin.test.mjs
import assert from "node:assert/strict"
import http from "node:http"
import { test } from "node:test"

import { api } from "./hexdb-plugin.mjs"

/** A local server answering each request with the next of `answers` (status codes); records what it got. */
async function server(answers) {
  const seen = []
  const s = http.createServer((req, res) => {
    req.resume()
    req.on("end", () => {
      seen.push({ method: req.method, key: req.headers["idempotency-key"] ?? null })
      const status = answers[Math.min(seen.length - 1, answers.length - 1)]
      res.writeHead(status, { "content-type": "application/json", "retry-after": "0" })
      res.end(JSON.stringify(status < 400 ? { id: `doc-${seen.length}` } : { error: { code: "unavailable", message: "synthetic" } }))
    })
  })
  await new Promise((resolve) => s.listen(0, "127.0.0.1", resolve))
  const client = api({ base: `http://127.0.0.1:${s.address().port}`, key: "SYNTHETIC_ONLY", retries: 2 })
  return { client, seen, close: () => new Promise((resolve) => s.close(resolve)) }
}

test("a write retried after an uncertain answer carries the same idempotency key", async () => {
  // The first insert committed, but its answer was a 503: the retry must be recognizable as the same write.
  const { client, seen, close } = await server([503, 201])
  try {
    await client.insert("jobs", { value: "one logical insert" })
    assert.equal(seen.length, 2)
    assert.ok(seen[0].key, "the write carries a key")
    assert.equal(seen[1].key, seen[0].key, "and the retry carries the same one")
  } finally {
    await close()
  }
})

test("each write gets its own key", async () => {
  const { client, seen, close } = await server([201])
  try {
    await client.insert("jobs", { n: 1 })
    await client.insert("jobs", { n: 2 })
    assert.notEqual(seen[0].key, seen[1].key)
  } finally {
    await close()
  }
})

test("a write sent without a key isn't repeated after an uncertain answer", async () => {
  const { client, seen, close } = await server([503, 201])
  try {
    await assert.rejects(client.post("/functions/send-email/run", {}, { "Idempotency-Key": "" }), { status: 503 })
    assert.equal(seen.length, 1)
    assert.equal(seen[0].key, null)
  } finally {
    await close()
  }
})

test("reads and 429s are retried", async () => {
  const { client, seen, close } = await server([429, 503, 200])
  try {
    await client.query("jobs", { filter: {} })
    assert.equal(seen.length, 3)
    assert.equal(seen[0].key, null, "queries are reads: no key")
  } finally {
    await close()
  }
})
