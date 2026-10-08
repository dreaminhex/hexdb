// Kafka change data capture: one message per change, key = document ID,
// value = the change JSON, headers op / seq / tessellation. Changes arrive
// in order; each batch is sent before the next is read, so a crash resends at
// most the last batch (HexDB delivers at least once: deduplicate on `seq`).
import { Kafka, logLevel } from "kafkajs"

import { log, main, payloads } from "../../sdk/hexdb-plugin.mjs"

const env = process.env
const sasl = env.KAFKA_SASL_MECHANISM
  ? { mechanism: env.KAFKA_SASL_MECHANISM, username: env.KAFKA_USERNAME ?? "", password: env.KAFKA_PASSWORD ?? "" }
  : undefined
const kafka = new Kafka({
  clientId: env.KAFKA_CLIENT_ID || "hexdb",
  brokers: (env.KAFKA_BROKERS || "localhost:9092").split(",").map((b) => b.trim()),
  ssl: env.KAFKA_SSL === "true",
  sasl,
  logLevel: logLevel.WARN,
})
const topicFor = (change) => (env.KAFKA_TOPIC || "hexdb.changes").replace("{tessellation}", change.tessellation)

await main(async () => {
  const producer = kafka.producer({ idempotent: true, maxInFlightRequests: 1 })
  await producer.connect()
  log(`connected to ${env.KAFKA_BROKERS}`)
  process.on("SIGTERM", () => producer.disconnect().finally(() => process.exit(0)))

  for await (const change of payloads()) {
    await producer.send({
      topic: topicFor(change),
      messages: [
        {
          key: change.id ?? change.tessellation,
          value: JSON.stringify(change),
          headers: { op: change.op, seq: String(change.seq), tessellation: change.tessellation },
        },
      ],
    })
  }
})
