// Sign in to Google Drive with your own OAuth client ("Desktop app") and print
// a refresh token for GDRIVE_REFRESH_TOKEN. Read-only access to Drive.
//
//   node connect.mjs
//
// The sign-in comes back to http://127.0.0.1:<random port> on this computer,
// with a random `state` that must match, so another page can't hand it a code.
import { randomBytes } from "node:crypto"
import { createServer } from "node:http"
import { createInterface } from "node:readline/promises"

const ask = createInterface({ input: process.stdin, output: process.stderr })
const clientId = (await ask.question("OAuth client ID: ")).trim()
const clientSecret = (await ask.question("OAuth client secret: ")).trim()
ask.close()

const state = randomBytes(16).toString("hex")
const server = createServer()
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve))
const redirect = `http://127.0.0.1:${server.address().port}`
const url = new URL("https://accounts.google.com/o/oauth2/v2/auth")
url.search = new URLSearchParams({
  client_id: clientId,
  redirect_uri: redirect,
  response_type: "code",
  scope: "https://www.googleapis.com/auth/drive.readonly",
  access_type: "offline",
  prompt: "consent",
  state,
}).toString()
console.error(`\nOpen this address in a browser on this computer and allow access:\n\n${url}\n`)

const code = await new Promise((resolve, reject) => {
  server.on("request", (req, res) => {
    const query = new URL(req.url, redirect).searchParams
    if (query.get("state") !== state) {
      res.writeHead(400, { "Content-Type": "text/plain" }).end("This doesn't belong to the sign-in started here.")
      return
    }
    if (query.get("code")) {
      res.writeHead(200, { "Content-Type": "text/plain" }).end("Signed in. You can close this tab.")
      resolve(query.get("code"))
    } else {
      res.writeHead(400, { "Content-Type": "text/plain" }).end("The sign-in was cancelled.")
      reject(new Error(`sign-in refused: ${query.get("error")}`))
    }
  })
})
server.close()

const response = await fetch("https://oauth2.googleapis.com/token", {
  method: "POST",
  headers: { "Content-Type": "application/x-www-form-urlencoded" },
  body: new URLSearchParams({ grant_type: "authorization_code", code, client_id: clientId, client_secret: clientSecret, redirect_uri: redirect }),
})
const body = await response.json()
if (!response.ok || !body.refresh_token) {
  console.error(`Google returned no refresh token (${response.status}): ${body.error_description ?? body.error ?? "remove the app's access at myaccount.google.com/permissions and try again"}`)
  process.exit(1)
}
console.error("\nSet these for the HexDB process (not in plugin.toml):\n")
console.log(`GDRIVE_CLIENT_ID=${clientId}\nGDRIVE_CLIENT_SECRET=${clientSecret}\nGDRIVE_REFRESH_TOKEN=${body.refresh_token}`)
