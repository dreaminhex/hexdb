// Sign in to Dropbox with your own app and print a refresh token for
// DROPBOX_REFRESH_TOKEN (read-only scopes).
//
//   node connect.mjs
import { createInterface } from "node:readline/promises"

const ask = createInterface({ input: process.stdin, output: process.stderr })
const appKey = (await ask.question("App key: ")).trim()
const appSecret = (await ask.question("App secret: ")).trim()
const url = new URL("https://www.dropbox.com/oauth2/authorize")
url.search = new URLSearchParams({ client_id: appKey, response_type: "code", token_access_type: "offline", scope: "files.metadata.read files.content.read" }).toString()
console.error(`\nOpen this address in a browser, allow access, and paste the code it shows:\n\n${url}\n`)
const code = (await ask.question("Code: ")).trim()
ask.close()

const response = await fetch("https://api.dropboxapi.com/oauth2/token", {
  method: "POST",
  headers: { "Content-Type": "application/x-www-form-urlencoded" },
  body: new URLSearchParams({ grant_type: "authorization_code", code, client_id: appKey, client_secret: appSecret }),
})
const body = await response.json()
if (!response.ok || !body.refresh_token) {
  console.error(`Dropbox returned no refresh token (${response.status}): ${body.error_description ?? body.error}`)
  process.exit(1)
}
console.error("\nSet these for the HexDB process (not in plugin.toml):\n")
console.log(`DROPBOX_APP_KEY=${appKey}\nDROPBOX_APP_SECRET=${appSecret}\nDROPBOX_REFRESH_TOKEN=${body.refresh_token}`)
