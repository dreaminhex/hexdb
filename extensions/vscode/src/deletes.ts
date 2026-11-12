// The delete history (HexDB 2.4.2+): each delete batch (a delete, a delete by filter, a
// transaction or a folder), with what it took and its cascade, restorable from the trash or
// purged for good. Users see their own deletes and those whose tessellations they can all read;
// auditors see every one. Restoring needs write access (restore for files), purging write and delete.

import { HexDBError } from "@dreaminhex/hexdb"
import * as vscode from "vscode"

import type { Connections, SavedConnection } from "./connections"
import { describeDelete, type DeleteBatch } from "./lib"

/** Batches shown per connection, newest first. */
const SHOWN = 50

/** The newest deletes, or null when the server has no delete history (before HexDB 2.4.2). */
export async function listDeletes(connections: Connections, connection: SavedConnection): Promise<DeleteBatch[] | null> {
  const session = await connections.session(connection.id)
  try {
    const { batches } = await session.request<{ batches: DeleteBatch[] }>("GET", `/deletes?limit=${SHOWN}`)
    return batches
  } catch (e) {
    if (e instanceof HexDBError && e.status === 404) return null
    throw e
  }
}

/** Puts back everything a delete took, after a confirmation; offers to skip documents whose IDs exist again. */
export async function restoreDelete(connections: Connections, connection: SavedConnection, batch: DeleteBatch): Promise<boolean> {
  const yes = "Restore"
  const answer = await vscode.window.showWarningMessage(
    `Restore ${describeDelete(batch)}?`,
    { modal: true, detail: `Deleted by ${batch.by} at ${new Date(batch.time).toLocaleString()}. Everything this delete removed comes back in one commit, with the links it detached.` },
    yes,
  )
  if (answer !== yes) return false
  const session = await connections.session(connection.id)
  const path = `/deletes/${encodeURIComponent(batch.batch)}/restore`
  try {
    await session.request("POST", path)
  } catch (e) {
    if (!(e instanceof HexDBError && e.status === 409)) throw e
    const rest = "Restore the Rest"
    const again = await vscode.window.showWarningMessage(
      "Some of these documents exist again.",
      { modal: true, detail: `${e.message}\n\nRestore the rest and leave the documents that came back since as they are?` },
      rest,
    )
    if (again !== rest) return false
    await session.request("POST", `${path}?skip_existing=true`)
  }
  vscode.window.showInformationMessage(`Restored ${describeDelete(batch)}.`)
  return true
}

/** Deletes what a batch left in the trash for good, after a confirmation. */
export async function purgeDelete(connections: Connections, connection: SavedConnection, batch: DeleteBatch): Promise<boolean> {
  const yes = "Purge"
  const answer = await vscode.window.showWarningMessage(
    `Purge ${describeDelete(batch)} for good?`,
    { modal: true, detail: "What this delete left in the trash is removed and can no longer be restored. This can't be undone." },
    yes,
  )
  if (answer !== yes) return false
  const session = await connections.session(connection.id)
  await session.request("DELETE", `/deletes/${encodeURIComponent(batch.batch)}`)
  vscode.window.showInformationMessage(`Purged ${describeDelete(batch)}.`)
  return true
}

/** The batch's full record (counts, files, detached links, the filter, what's still in the trash), as JSON. */
export async function showDelete(connections: Connections, connection: SavedConnection, batch: DeleteBatch): Promise<void> {
  const session = await connections.session(connection.id)
  const record = await session.request<unknown>("GET", `/deletes/${encodeURIComponent(batch.batch)}`)
  const doc = await vscode.workspace.openTextDocument({ language: "json", content: JSON.stringify(record, null, 2) })
  await vscode.window.showTextDocument(doc, { preview: true })
}
