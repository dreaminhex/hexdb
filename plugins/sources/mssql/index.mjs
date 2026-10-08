// SQL Server -> HexDB sync. A rowversion column increases on every insert and
// update, so "rows with a rowversion above the last one copied" are exactly
// the changes. The position is kept in sync-state.json in this folder.
import { readFileSync, writeFileSync } from "node:fs"
import sql from "mssql"

import { api, log, main } from "../../sdk/hexdb-plugin.mjs"

const env = process.env
const table = env.MSSQL_TABLE
const key = env.MSSQL_KEY || "Id"
const rowversion = env.MSSQL_ROWVERSION || "RowVer"
const target = env.TARGET || "orders"
const pollMs = Number(env.POLL_SECONDS || 10) * 1000
const STATE = "sync-state.json"

const quote = (name) => name.split(".").map((part) => `[${part.replaceAll("]", "]]")}]`).join(".")
const loadState = () => {
  try {
    return JSON.parse(readFileSync(STATE, "utf8"))
  } catch {
    return { since: "0x0000000000000000" }
  }
}

await main(async () => {
  if (!table) throw new Error("Set MSSQL_TABLE in plugin.toml")
  const hexdb = api()
  const pool = await sql.connect(env.MSSQL_CONNECTION)
  log(`syncing ${table} -> ${target} every ${pollMs / 1000}s`)
  const state = loadState()

  for (;;) {
    for (;;) {
      const result = await pool
        .request()
        .input("since", sql.VarBinary(8), Buffer.from(state.since.slice(2), "hex"))
        .query(`SELECT TOP 500 *, CONVERT(varchar(18), ${quote(rowversion)}, 1) AS __rv FROM ${quote(table)} WHERE ${quote(rowversion)} > @since ORDER BY ${quote(rowversion)}`)
      const rows = result.recordset
      if (rows.length === 0) break
      const docs = rows.map((row) => {
        const doc = { ...row, source_id: String(row[key]) }
        delete doc[key]
        delete doc[rowversion]
        delete doc.__rv
        return JSON.parse(JSON.stringify(doc))
      })
      const written = await hexdb.upsert(target, ["source_id"], docs)
      state.since = rows.at(-1).__rv
      writeFileSync(STATE, JSON.stringify(state))
      log(`${table}: ${written.inserted} inserted, ${written.replaced} updated (through ${state.since})`)
      if (rows.length < 500) break
    }
    await new Promise((resolve) => setTimeout(resolve, pollMs))
  }
})
