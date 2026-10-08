// PostgreSQL -> HexDB sync. Rows changed since the last run are read in
// (updated_at, key) order, 500 at a time, and upserted by their primary key.
// The position is kept in sync-state.json in this folder, so a restart
// continues where it stopped. Deleted rows aren't detected (use soft deletes,
// or a change-data-capture tool for Postgres, for that).
import { readFileSync, writeFileSync } from "node:fs"
import pg from "pg"

import { api, log, main } from "../../sdk/hexdb-plugin.mjs"

const env = process.env
const table = env.PG_TABLE
const key = env.PG_KEY || "id"
const updatedAt = env.PG_UPDATED_AT || "updated_at"
const target = env.TARGET || table
const pollMs = Number(env.POLL_SECONDS || 10) * 1000
const STATE = "sync-state.json"

const ident = (name) => `"${String(name).replaceAll('"', '""')}"`
const loadState = () => {
  try {
    return JSON.parse(readFileSync(STATE, "utf8"))
  } catch {
    return { since: "1970-01-01T00:00:00Z", lastKey: null }
  }
}

await main(async () => {
  if (!table) throw new Error("Set PG_TABLE in plugin.toml")
  const hexdb = api()
  const db = new pg.Client({ connectionString: env.PG_CONNECTION })
  await db.connect()
  log(`syncing ${table} -> ${target} every ${pollMs / 1000}s`)
  const state = loadState()

  for (;;) {
    for (;;) {
      // Keyset paging: rows after (since, lastKey), so equal timestamps aren't skipped.
      const { rows } = await db.query(
        `SELECT * FROM ${ident(table)}
          WHERE (${ident(updatedAt)}, ${ident(key)}::text) > ($1::timestamptz, $2::text)
          ORDER BY ${ident(updatedAt)}, ${ident(key)}::text
          LIMIT 500`,
        [state.since, state.lastKey ?? ""],
      )
      if (rows.length === 0) break
      const docs = rows.map((row) => {
        const doc = { ...row, source_id: String(row[key]) }
        delete doc[key]
        return JSON.parse(JSON.stringify(doc)) // dates become ISO strings
      })
      const result = await hexdb.upsert(target, ["source_id"], docs)
      const last = rows.at(-1)
      state.since = new Date(last[updatedAt]).toISOString()
      state.lastKey = String(last[key])
      writeFileSync(STATE, JSON.stringify(state))
      log(`${table}: ${result.inserted} inserted, ${result.replaced} updated (through ${state.since})`)
      if (rows.length < 500) break
    }
    await new Promise((resolve) => setTimeout(resolve, pollMs))
  }
})
