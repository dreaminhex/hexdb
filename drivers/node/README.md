# @dreaminhex/hexdb

The JavaScript and TypeScript client for [HexDB](https://github.com/dreaminhex/hexdb), a document database with REST, GraphQL and read-only SQL, encryption at rest and replication with automatic failover.

It covers documents (insert, get, replace, patch, delete), queries with filters, sorting and paging, counts, aggregations, upserts, transactions, idempotency keys, the change feed, streams, functions and GraphQL. It retries `429` and `503` answers for reads, and for writes that carry an idempotency key. Runs on Node.js 18+, Deno, Bun and in browsers, with no dependencies.

```sh
npm install @dreaminhex/hexdb
```

```ts
import { HexDB } from "@dreaminhex/hexdb"

const db = new HexDB({ url: "http://127.0.0.1:7700", apiKey: process.env.HEXDB_API_KEY })
const orders = db.tessellation("orders")
await orders.insert({ customer: "ada", total: 12 })
const page = await orders.query({ filter: { total: { $gt: 10 } }, sort: "-total" })
```

Create an API key on the admin UI's Account page. The [driver documentation](https://github.com/dreaminhex/hexdb/tree/main/drivers) and the [manual](https://github.com/dreaminhex/hexdb/blob/main/MANUAL.md) cover the rest. Licensed under the Apache License 2.0.
