// Kinesis change data capture. Records are sent in order, in batches of up to
// 500 (the PutRecords limit) or whatever arrived within 200 ms; failed records
// are retried before the next batch, so per-document order is kept. Each batch
// is acknowledged to HexDB once Kinesis accepted all of it (acks = true in
// plugin.toml), so a crash or restart resends what wasn't confirmed.
import { KinesisClient, PutRecordsCommand } from "@aws-sdk/client-kinesis"

import { ack, log, main, payloads, warn } from "../../sdk/hexdb-plugin.mjs"

const stream = process.env.KINESIS_STREAM || "hexdb-changes"
const client = new KinesisClient({})
const IDLE = Symbol("idle")
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms))

/** Send records (each `{ seq, record }`), retrying throttled ones, then acknowledge the last. */
async function put(entries) {
  if (entries.length === 0) return
  let pending = entries.map((e) => e.record)
  for (let attempt = 0; pending.length > 0; attempt++) {
    const result = await client.send(new PutRecordsCommand({ StreamName: stream, Records: pending }))
    if (!result.FailedRecordCount) break
    pending = pending.filter((_, i) => result.Records?.[i]?.ErrorCode)
    warn(`${pending.length} record(s) were throttled; retrying`)
    await sleep(Math.min(200 * 2 ** attempt, 5000))
  }
  ack(entries[entries.length - 1].seq)
}

await main(async () => {
  log(`sending changes to Kinesis stream ${stream}`)
  const changes = payloads()[Symbol.asyncIterator]()
  let next = changes.next()
  let batch = []
  for (;;) {
    // With records waiting, send them if nothing else arrives soon.
    const result = batch.length > 0 ? await Promise.race([next, sleep(200).then(() => IDLE)]) : await next
    if (result === IDLE || result.done || batch.length >= 500) {
      if (batch.length > 0) await put(batch.splice(0))
      if (result === IDLE) continue
    }
    if (result.done) break
    const change = result.value
    batch.push({ seq: change.seq, record: { PartitionKey: change.id ?? change.tessellation, Data: Buffer.from(JSON.stringify(change)) } })
    next = changes.next()
    if (batch.length >= 500) await put(batch.splice(0))
  }
})
