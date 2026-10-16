// Google Drive -> HexDB file import (see plugin.toml). Plain HTTPS, no SDK.
import { main } from "../../sdk/hexdb-plugin.mjs"
import { importFiles } from "../../sdk/file-import.mjs"

const env = process.env
const API = "https://www.googleapis.com/drive/v3"
const FOLDER = "application/vnd.google-apps.folder"
let token = { value: "", expires: 0 }

/** An access token from the refresh token, renewed a minute before it expires. */
async function accessToken() {
  if (token.value && Date.now() < token.expires - 60_000) return token.value
  const response = await fetch("https://oauth2.googleapis.com/token", {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({ grant_type: "refresh_token", refresh_token: env.GDRIVE_REFRESH_TOKEN, client_id: env.GDRIVE_CLIENT_ID, client_secret: env.GDRIVE_CLIENT_SECRET }),
  })
  const body = await response.json()
  if (!response.ok) throw new Error(`Google refused the refresh token (${response.status}): ${body.error_description ?? body.error}. Run connect.mjs again.`)
  token = { value: body.access_token, expires: Date.now() + body.expires_in * 1000 }
  return token.value
}

async function drive(path, params = {}) {
  const url = `${API}${path}?${new URLSearchParams({ supportsAllDrives: "true", ...params })}`
  for (let attempt = 0; ; attempt++) {
    const response = await fetch(url, { headers: { Authorization: `Bearer ${await accessToken()}` } })
    if ((response.status === 429 || response.status >= 500) && attempt < 5) {
      await new Promise((resolve) => setTimeout(resolve, 2 ** attempt * 1000))
      continue
    }
    if (!response.ok) throw new Error(`Google Drive returned ${response.status}: ${(await response.text()).slice(0, 300)}`)
    return response
  }
}

/** Every file under a folder, depth first, with its path below the root folder. */
async function* walk(folderId, prefix) {
  let pageToken
  do {
    const params = {
      q: `'${folderId.replace(/'/g, "\\'")}' in parents and trashed = false`,
      fields: "nextPageToken, files(id, name, mimeType, md5Checksum, modifiedTime, size)",
      pageSize: "1000",
      includeItemsFromAllDrives: "true",
    }
    if (pageToken) params.pageToken = pageToken
    const page = await (await drive("/files", params)).json()
    pageToken = page.nextPageToken
    for (const file of page.files ?? []) {
      // "/" can't be part of a name in HexDB; Drive allows it.
      const name = file.name.replace(/[\\/]/g, "_")
      if (file.mimeType === FOLDER) {
        yield* walk(file.id, `${prefix}${name}/`)
      } else if (file.mimeType.startsWith("application/vnd.google-apps.")) {
        continue // Docs, Sheets, Slides: not files with content
      } else {
        yield { key: file.id, path: `${prefix}${name}`, version: file.md5Checksum ?? file.modifiedTime, size: file.size ? Number(file.size) : undefined, modified: file.modifiedTime }
      }
    }
  } while (pageToken)
}

async function open(item) {
  const response = await drive(`/files/${encodeURIComponent(item.key)}`, { alt: "media" })
  return { body: response.body, contentType: response.headers.get("content-type") ?? undefined }
}

await main(async () => {
  for (const name of ["GDRIVE_CLIENT_ID", "GDRIVE_CLIENT_SECRET", "GDRIVE_REFRESH_TOKEN"]) {
    if (!env[name]) throw new Error(`Set ${name} for the HexDB process (see plugin.toml; connect.mjs makes the refresh token)`)
  }
  if (!env.GDRIVE_FOLDER_ID || env.GDRIVE_FOLDER_ID === "the-folder-id") throw new Error("Set GDRIVE_FOLDER_ID in plugin.toml")
  await importFiles({ name: "gdrive", list: () => walk(env.GDRIVE_FOLDER_ID, ""), open })
})
