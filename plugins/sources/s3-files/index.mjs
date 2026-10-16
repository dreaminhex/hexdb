// S3 -> HexDB file import (see plugin.toml).
import { GetObjectCommand, ListObjectsV2Command, S3Client } from "@aws-sdk/client-s3"

import { main } from "../../sdk/hexdb-plugin.mjs"
import { importFiles } from "../../sdk/file-import.mjs"

const env = process.env
const bucket = env.S3_BUCKET
const prefix = env.S3_PREFIX || ""
const s3 = new S3Client(env.S3_ENDPOINT ? { endpoint: env.S3_ENDPOINT, forcePathStyle: true, region: env.AWS_REGION || "auto" } : { region: env.AWS_REGION })

async function* list() {
  let token
  do {
    const page = await s3.send(new ListObjectsV2Command({ Bucket: bucket, Prefix: prefix, ContinuationToken: token }))
    token = page.NextContinuationToken
    for (const object of page.Contents ?? []) {
      // Folder placeholders ("photos/") aren't files.
      if (object.Key.endsWith("/")) continue
      const path = object.Key.slice(prefix.length).replace(/^\/+/, "")
      if (!path) continue
      yield { key: object.Key, path, version: object.ETag, size: object.Size, modified: object.LastModified }
    }
  } while (token)
}

async function open(item) {
  const object = await s3.send(new GetObjectCommand({ Bucket: bucket, Key: item.key }))
  return { body: object.Body.transformToWebStream(), contentType: object.ContentType }
}

await main(async () => {
  if (!bucket) throw new Error("Set S3_BUCKET in plugin.toml")
  await importFiles({ name: "s3", list, open })
})
