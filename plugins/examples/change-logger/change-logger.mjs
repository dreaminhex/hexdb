// Example HexDB plugin. Each line on stdin is a change:
//   {"seq", "timestamp", "op": "put" | "delete" | "drop_tessellation", "tessellation", "id", "document"}
// Lines printed to stdout and stderr appear in the HexDB log (Logs page).
import { appendFileSync } from "node:fs"
import { createInterface } from "node:readline"

console.log(`change logger started (${process.env.HEXDB_PLUGIN_ID}, API ${process.env.HEXDB_API})`)

for await (const line of createInterface({ input: process.stdin })) {
  const change = JSON.parse(line)
  appendFileSync("changes.ndjson", line + "\n")
  console.log(`#${change.seq} ${change.op} ${change.tessellation}${change.id ? "/" + change.id : ""}`)
}
