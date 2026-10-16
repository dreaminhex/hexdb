// Dropbox -> HexDB file import (see plugin.toml). Plain HTTPS, no SDK.
import { main } from "../../sdk/hexdb-plugin.mjs"
import { importFiles } from "../../sdk/file-import.mjs"

const env = process.env
const root = (env.DROPBOX_FOLDER ?? "").replace(/\/+$/, "")
let token = { value: "", expires: 0 }

async function accessToken() {
  if (token.value && Date.now() < token.expires - 60_000) return token.value
  const response = await fetch("https://api.dropboxapi.com/oauth2/token", {
    method: "POST",
    headers: { "Content-Type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams({ grant_type: "refresh_token", refresh_token: env.DROPBOX_REFRESH_TOKEN, client_id: env.DROPBOX_APP_KEY, client_secret: env.DROPBOX_APP_SECRET }),
  })
  const body = await response.json()
  if (!response.ok) throw new Error(`Dropbox refused the refresh token (${response.status}): ${body.error_description ?? body.error}. Run connect.mjs again.`)
  token = { value: body.access_token, expires: Date.now() + body.expires_in * 1000 }
  return token.value
}

/** Dropbox-API-Arg must be ASCII: escape everything else as \uXXXX. */
function headerJson(value) {
  return JSON.stringify(value).replace(/[\u007f-￿]/g, (c) => `\\u${c.charCodeAt(0).toString(16).padStart(4, "0")}`)
}

async function call(url, init) {
  for (let attempt = 0; ; attempt++) {
    const response = await fetch(url, { ...init, headers: { Authorization: `Bearer ${await accessToken()}`, ...init.headers } })
    if ((response.status === 429 || response.status >= 500) && attempt < 5) {
      const wait = Number(response.headers.get("retry-after")) || 2 ** attempt
      await new Promise((resolve) => setTimeout(resolve, wait * 1000))
      continue
    }
    if (!response.ok) throw new Error(`Dropbox returned ${response.status}: ${(await response.text()).slice(0, 300)}`)
    return response
  }
}

async function* list() {
  const rpc = (path, body) => call(`https://api.dropboxapi.com/2/files/${path}`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) }).then((r) => r.json())
  let page = await rpc("list_folder", { path: root, recursive: true, include_non_downloadable_files: false })
  for (;;) {
    for (const entry of page.entries ?? []) {
      if (entry[".tag"] !== "file") continue
      const path = entry.path_display.slice(root.length).replace(/^\/+/, "")
      yield { key: entry.id, path, version: entry.rev, size: entry.size, modified: entry.server_modified }
    }
    if (!page.has_more) break
    page = await rpc("list_folder/continue", { cursor: page.cursor })
  }
}

async function open(item) {
  const response = await call("https://content.dropboxapi.com/2/files/download", { method: "POST", headers: { "Dropbox-API-Arg": headerJson({ path: item.key }) } })
  return { body: response.body }
}

await main(async () => {
  for (const name of ["DROPBOX_APP_KEY", "DROPBOX_APP_SECRET", "DROPBOX_REFRESH_TOKEN"]) {
    if (!env[name]) throw new Error(`Set ${name} for the HexDB process (see plugin.toml; connect.mjs makes the refresh token)`)
  }
  await importFiles({ name: "dropbox", list, open })
})
