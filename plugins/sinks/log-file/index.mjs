// Daily log files: each log record HexDB sends is appended, as one JSON line,
// to LOG_DIR/hexdb-<date>.ndjson. Old files are removed after LOG_RETAIN_DAYS.
import { appendFileSync, mkdirSync, readdirSync, unlinkSync } from "node:fs"
import { join } from "node:path"

import { log, main, payloads } from "../../sdk/hexdb-plugin.mjs"

const dir = process.env.LOG_DIR || "./logs"
const retainDays = Number(process.env.LOG_RETAIN_DAYS || 14)
mkdirSync(dir, { recursive: true })

function prune() {
  const cutoff = Date.now() - retainDays * 86_400_000
  for (const name of readdirSync(dir)) {
    const match = /^hexdb-(\d{4}-\d{2}-\d{2})\.ndjson$/.exec(name)
    if (match && Date.parse(match[1]) < cutoff) unlinkSync(join(dir, name))
  }
}

await main(async () => {
  log(`writing logs to ${dir} (keeping ${retainDays} days)`)
  prune()
  let day = ""
  for await (const record of payloads()) {
    const date = String(record.timestamp).slice(0, 10)
    if (date !== day) {
      day = date
      prune()
    }
    appendFileSync(join(dir, `hexdb-${date}.ndjson`), JSON.stringify(record) + "\n")
  }
})
