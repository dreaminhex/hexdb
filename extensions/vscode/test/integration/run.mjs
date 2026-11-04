// Runs suite.cjs inside VS Code with the extension loaded, against the public playground
// (or HEXDB_TEST_URI). Uses the installed VS Code if it can find it (VSCODE_PATH overrides),
// otherwise downloads one. A throwaway profile keeps your settings and extensions out of it.
import { existsSync, mkdtempSync, rmSync } from "node:fs"
import { tmpdir } from "node:os"
import { dirname, join, resolve } from "node:path"
import { fileURLToPath } from "node:url"

import { runTests } from "@vscode/test-electron"

const here = dirname(fileURLToPath(import.meta.url))
const candidates = [process.env.VSCODE_PATH, process.env.LOCALAPPDATA && join(process.env.LOCALAPPDATA, "Programs", "Microsoft VS Code", "Code.exe"), "/Applications/Visual Studio Code.app/Contents/MacOS/Electron"]
const installed = candidates.find((p) => p && existsSync(p))
const profile = mkdtempSync(join(tmpdir(), "hexdb-vscode-test-"))
// Set when this runs from a VS Code terminal or extension; it would make Code.exe act as plain Node.
delete process.env.ELECTRON_RUN_AS_NODE
// Under drivers/testing/server.mjs (a scratch server with an administrator's key), test against it.
if (!process.env.HEXDB_TEST_URI && process.env.HEXDB_URL && process.env.HEXDB_API_KEY) {
  const url = new URL(process.env.HEXDB_URL)
  process.env.HEXDB_TEST_URI = `${url.protocol === "https:" ? "hexdbs" : "hexdb"}://${url.host}?apiKey=${encodeURIComponent(process.env.HEXDB_API_KEY)}`
  process.env.HEXDB_TEST_SEED = "1"
}

try {
  await runTests({
    ...(installed ? { vscodeExecutablePath: installed } : {}),
    extensionDevelopmentPath: resolve(here, "../.."),
    extensionTestsPath: resolve(here, "suite.cjs"),
    launchArgs: ["--user-data-dir", join(profile, "user"), "--extensions-dir", join(profile, "extensions"), "--disable-workspace-trust", "--skip-welcome", "--new-window"],
  })
} catch (e) {
  console.error(e)
  process.exitCode = 1
} finally {
  rmSync(profile, { recursive: true, force: true })
}
