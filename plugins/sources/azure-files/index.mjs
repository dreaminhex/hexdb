// Azure Blob Storage -> HexDB file import (see plugin.toml).
import { BlobServiceClient, StorageSharedKeyCredential } from "@azure/storage-blob"

import { main } from "../../sdk/hexdb-plugin.mjs"
import { importFiles } from "../../sdk/file-import.mjs"

const env = process.env
const prefix = env.AZURE_PREFIX || ""

function service() {
  if (env.AZURE_STORAGE_CONNECTION_STRING) return BlobServiceClient.fromConnectionString(env.AZURE_STORAGE_CONNECTION_STRING)
  if (env.AZURE_STORAGE_ACCOUNT && env.AZURE_STORAGE_KEY) {
    const url = env.AZURE_ENDPOINT || `https://${env.AZURE_STORAGE_ACCOUNT}.blob.core.windows.net`
    return new BlobServiceClient(url, new StorageSharedKeyCredential(env.AZURE_STORAGE_ACCOUNT, env.AZURE_STORAGE_KEY))
  }
  throw new Error("Set AZURE_STORAGE_CONNECTION_STRING, or AZURE_STORAGE_ACCOUNT and AZURE_STORAGE_KEY, for the HexDB process")
}

await main(async () => {
  if (!env.AZURE_CONTAINER) throw new Error("Set AZURE_CONTAINER in plugin.toml")
  const container = service().getContainerClient(env.AZURE_CONTAINER)

  async function* list() {
    for await (const blob of container.listBlobsFlat({ prefix })) {
      const path = blob.name.slice(prefix.length).replace(/^\/+/, "")
      if (!path || path.endsWith("/")) continue
      yield { key: blob.name, path, version: blob.properties.etag, size: blob.properties.contentLength, modified: blob.properties.lastModified }
    }
  }

  async function open(item) {
    const download = await container.getBlobClient(item.key).download()
    return { body: download.readableStreamBody, contentType: download.contentType }
  }

  await importFiles({ name: "azure", list, open })
})
