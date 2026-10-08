// S3 -> HexDB import. Lists objects under the prefix, imports each new one
// (JSON array, JSON object, or JSON lines), in pages of 1000 documents, with
// an idempotency key per page so a retry after a crash doesn't duplicate.
import { createHash } from "node:crypto"
import { readFileSync, writeFileSync } from "node:fs"
import { GetObjectCommand, ListObjectsV2Command, S3Client } from "@aws-sdk/client-s3"

import { api, log, main, warn } from "../../sdk/hexdb-plugin.mjs"

const env = process.env
const bucket = env.S3_BUCKET
const prefix = env.S3_PREFIX || ""
const target = env.TARGET || "imports"
const keyField = env.KEY_FIELD || ""
const pollMs = Number(env.POLL_SECONDS || 60) * 1000
const STATE = "s3-state.json"
const s3 = new S3Client(env.S3_ENDPOINT ? { endpoint: env.S3_ENDPOINT, forcePathStyle: true } : {})

const loadState = () => {
  try {
    return JSON.parse(readFileSync(STATE, "utf8"))
  } catch {
    return { imported: {} }
  }
}

function parse(text) {
  const trimmed = text.trim()
  if (trimmed.startsWith("[")) return JSON.parse(trimmed)
  if (trimmed.startsWith("{") && !trimmed.includes("\n{")) return [JSON.parse(trimmed)]
  return trimmed.split(/\r?\n/).filter((l) => l.trim()).map((l) => JSON.parse(l))
}

await main(async () => {
  if (!bucket) throw new Error("Set S3_BUCKET in plugin.toml")
  const hexdb = api()
  const state = loadState()
  log(`importing s3://${bucket}/${prefix} -> ${target}`)

  for (;;) {
    let token
    do {
      const page = await s3.send(new ListObjectsV2Command({ Bucket: bucket, Prefix: prefix, ContinuationToken: token }))
      token = page.NextContinuationToken
      for (const object of page.Contents ?? []) {
        const id = `${object.Key}@${object.ETag}`
        if (state.imported[id] || object.Key.endsWith("/")) continue
        try {
          const body = await (await s3.send(new GetObjectCommand({ Bucket: bucket, Key: object.Key }))).Body.transformToString()
          const docs = parse(body)
          for (let i = 0; i < docs.length; i += 1000) {
            const chunk = docs.slice(i, i + 1000)
            const idem = createHash("sha256").update(`s3:${id}:${i}`).digest("hex")
            if (keyField) await hexdb.upsert(target, [keyField], chunk, idem)
            else await hexdb.insertMany(target, chunk, idem)
          }
          state.imported[id] = new Date().toISOString()
          writeFileSync(STATE, JSON.stringify(state))
          log(`imported ${docs.length} document(s) from ${object.Key}`)
        } catch (e) {
          warn(`couldn't import ${object.Key}: ${e.message}`)
        }
      }
    } while (token)
    await new Promise((resolve) => setTimeout(resolve, pollMs))
  }
})
