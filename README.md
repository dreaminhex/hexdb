# HexDB

**HexDB** is a distributed, document-oriented database engine designed for performance, developer usability, and deep AI integration. Inspired by decades of systems design and forged for modern workloads, it supports dynamic indexing, ACID-compliant transactions, full-text search, GraphQL queries, network discovery, pluggable components, and many other features.

## 🧠 Philosophy

HexDB is a reimagination of what modern persistence looks like with AI-native guidance, extensibility, and performance-aware modularity built directly into the core.

| "Dream in Hex. Remember it with HexDB."

### Core Focus

1. Performance

- Built in Rust for extremely high performance
- Compression via ZStandard, with configurable compression level
- Automatic data compaction for long-term storage
- Integrated caching & percolation
- Bulk APIs for large document loads

1. Scalability

- Network discovery for horizontal partitioning
- Configurable disk and memory settings
- Integrated network load balancing
- Supports multiple ingest streams (Amazon S3, SFTP)
- Supports multiple output streams (Amazon Kinesis, Apache Kafka)

1. Resilience

- Memory sector scaling and recovery of corrupt sectors (vertices)
- Write-ahead logs for replay (WAL)
- Disk output allows rapid recovery (SSTables)
- OpenTelemetry output
- Supports multiple log sinks (Splunk, Logstash)

1. Ease of Use

- Standards-based REST API
- Full-text search
- OAS 3.0 Specification & Swagger-style documentation
- Integrated management UI
- GraphQL Query Syntax
- Remote management
- Plugin ecosystem
- HTTP, TCP, and Websocket support
- Programmable stored actions and queries

1. Security

- Industrial grade encryption for all data in-transit and at-rest
- Configurable users, roles, and permissions for documents and collections
- IP range blocking by individual IP or CIDR blocks
- Default password hashing for all users
- Required multi-factor authentication for users
- SSL certificate support

---

## 🧱 Project Structure

hexdb/
├── hexdb_api
|   ├── .hexdb
|   ├── data
|   └── src
├── hexdb_core
|   └── src
|       └── network
├── hexdb_cli
|   └── src
├── hexdb_query
|   └── src
├── plugins
├── schemas
└── scripts

---

## 🚀 Getting Started

### 1. Install Rust

```bash
curl https://sh.rustup.rs -sSf | sh
```

On Windows, the MSVC toolchain also needs the Visual Studio Build Tools ("Desktop development with C++").

### 2. Build Everything & Install the CLI and Server

```bash
cargo build --workspace

# Installs the `hexdb` CLI and the `hexdb_api` server side by side.
cargo install --path hexdb_cli
cargo install --path hexdb_api
```

To build the admin UI (served at `/ui/`):

```bash
cd hexdb_admin
npm install
npm run build
```

### 3. Start the Node

Run `hexdb` from the directory that contains `hexdb.toml` (e.g. `hexdb_api`), or point at it with `--config` or the `HEXDB_CONFIG` environment variable. Apply the `-s` argument to run HexDB in the background; output goes to `hexdb.log` in the data directory.

```bash
hexdb --config hexdb_api/hexdb.toml start -s
```

Or you can simply run the API directly using `cargo` from the repository root.

```bash
cargo run -p hexdb_api
```

### 4. Check Health and Status

```bash
hexdb health
hexdb status

curl http://localhost:7700/health
```

To load sample data (articles, products, customers, orders, and sessions that expire after 15 minutes), run this with Node.js 18+ while the server is running. It skips tessellations that already have documents, so it's safe to re-run.

```bash
node scripts/seed.mjs
```

### 5. Access the UI

Browse to [http://localhost:7700/ui/](http://localhost:7700/ui/). The admin UI has:

- **Dashboard**: live document, memory, disk and operation stats; an activity chart (documents per tessellation, operations per minute, or storage over the last 15 minutes to 6 hours); vertex health; the lattice with each hex's role and replication state; and per-tessellation sizes. On a replica, a banner names the Overseer. "Flush to disk" writes unflushed data to SSTables.
- **Queries**: a GraphQL console with schema-aware autocomplete, validation, example queries (including aggregations, full-text search, and transactions), history, and JSON or table results. Press Ctrl+Enter (⌘+Enter on macOS) to run.
- **Tessellations**: create and delete tessellations, see their sizes, and manage their indexes.
- **Documents**: browse a tessellation with a JSON filter, sort and paging, and create, edit or delete documents in a JSON editor. The footer shows which index answered the query.
- **Users** and **Roles**: manage accounts and role grants.
- **Plugins**: loaded plugins, their state, and what they've delivered.
- **Logs**: the server's recent log, tailed live, with level, module and text filters.

The sun/moon button in the header switches between light, dark and system themes.

For UI development, run `npm run dev` in `hexdb_admin` while a HexDB server is running; every request outside `/ui/` is proxied to `http://127.0.0.1:7700` (override with `HEXDB_API`).

### 6. Stop the Node

`hexdb stop` asks the server to shut down gracefully, using a one-time token the server writes to `hexdb.pid` in its data directory. Use `--force` to kill a server that won't stop.

```bash
hexdb stop
```

## 🧪 Testing

Unit tests live next to the code they test, in `#[cfg(test)]` modules. End-to-end tests live in the `hexdb_tests` crate, which starts the real `hexdb_api` server in a temporary directory on free ports and exercises it over HTTP, including restarts and crashes.

```bash
cargo test --workspace                     # everything
cargo test -p hexdb_tests                  # end-to-end tests only
cargo test -p hexdb_tests -- --ignored     # known bugs (see TODO.md); these currently fail
```

Lattice tests start several servers that discover each other on loopback, replicate, and fail over. Set `HEXDB_TEST_KEEP=1` to keep each test's directory (config, data, and `server.log`) for inspection. Failed tests always keep theirs and print the server log tail.

## API Operations

Documents are plain JSON. Responses add `id` and, when a TTL is set, `_expires_at`; both are ignored if sent in a request body. Errors look like this:

```json
{ "error": { "code": "not_found", "message": "Document 01J... not found in 'articles'." } }
```

| Status | Code                     | Meaning                                                     |
| ------ | ------------------------ | ----------------------------------------------------------- |
| 400    | `invalid_request`        | Malformed JSON, bad parameters, or invalid values           |
| 403    | `forbidden`              | System tessellations (`users`, `roles`) via document routes |
| 404    | `not_found`              | Unknown document, tessellation, user or role                |
| 409    | `conflict`               | Already exists, or an idempotent request is in progress     |
| 413    | `payload_too_large`      | Body over 2 MB (32 MB for bulk endpoints)                   |
| 422    | `idempotency_key_reused` | The `Idempotency-Key` was used with a different request     |

### Documents

```bash
# Insert (201, returns the document and a Location header). Add ?ttl=<seconds> to expire it.
curl -X POST http://localhost:7700/articles \
  -H "Content-Type: application/json" \
  -d '{ "title": "Quantum Tessellation", "tags": [ "hexdb", "rust", "ai" ], "published": true, "views": 445 }'

# Fetch, replace, patch (a null field removes it), delete (204)
curl http://localhost:7700/articles/01JTY87RVJ9B5863KMB2YD896B
curl -X PUT   http://localhost:7700/articles/01JTY87RVJ9B5863KMB2YD896B -H "Content-Type: application/json" -d '{ "title": "New" }'
curl -X PATCH http://localhost:7700/articles/01JTY87RVJ9B5863KMB2YD896B -H "Content-Type: application/json" -d '{ "views": 446, "tags": null }'
curl -X DELETE http://localhost:7700/articles/01JTY87RVJ9B5863KMB2YD896B

# List, filter, sort and page (see "Filters" below). Up to 1000 per page (default 100); responses include "total".
# Without sort, results are in ID order and "next" goes in ?after= for the next page; with sort, page with offset.
curl "http://localhost:7700/articles?limit=50"
curl -G http://localhost:7700/articles --data-urlencode 'filter={"published":true,"views":{"$gte":100}}' \
  --data-urlencode 'sort=-views,title' -d limit=25 -d offset=25

# The same as a JSON body, for filters too long for a URL ("sort" may also be "-views,title")
curl -X POST http://localhost:7700/articles/_query -H "Content-Type: application/json" \
  -d '{ "filter": { "tags": "rust" }, "sort": [{ "field": "views", "descending": true }], "limit": 25 }'

# Count, optionally with a filter
curl http://localhost:7700/articles/count
curl -G http://localhost:7700/articles/count --data-urlencode 'filter={"published":false}'
```

Inserting into a tessellation that doesn't exist creates it.

### Bulk Writes

Each bulk request is atomic: every document is written, or none are. Up to 10,000 documents and 32 MB per request.

```bash
# Insert many (201): a JSON array, or {"documents": [...]}
curl -X POST http://localhost:7700/articles/_bulk -H "Content-Type: application/json" \
  -d '[{ "title": "One" }, { "title": "Two" }]'

# Replace or patch many by id (every item needs an "id"; a missing id fails the whole request with 404)
curl -X PATCH http://localhost:7700/articles/_bulk -H "Content-Type: application/json" \
  -d '[{ "id": "01J...", "published": true }, { "id": "01J...", "published": true }]'

# Patch every document matching a filter ({} matches all; see "Filters" below)
curl -X POST http://localhost:7700/articles/_update -H "Content-Type: application/json" \
  -d '{ "filter": { "status": "draft" }, "update": { "status": "published" } }'
# => { "matched": 10, "modified": 10 }
```

### Idempotency Keys

Every REST write accepts an `Idempotency-Key` header (1-255 printable ASCII characters), and every GraphQL document mutation accepts an `idempotencyKey` argument. The key and the result are stored atomically with the write for 24 hours. Retrying the same request with the same key returns the original response with `Idempotent-Replayed: true` instead of writing again, even after a crash. Reusing a key for a different request returns 422; a concurrent request with a key that is still being processed returns 409.

```bash
curl -X POST http://localhost:7700/orders -H "Content-Type: application/json" \
  -H "Idempotency-Key: order-7f3a" -d '{ "item": "widget", "qty": 2 }'
```

### Tessellations

```bash
curl http://localhost:7700/tessellations                          # list
curl -X POST http://localhost:7700/tessellations -H "Content-Type: application/json" -d '{ "name": "articles" }'
curl http://localhost:7700/tessellations/articles                 # details and document count
curl -X DELETE http://localhost:7700/tessellations/articles       # deletes all of its documents
```

Names are 1-64 letters, digits, `_` or `-`, unique ignoring case, and can't start with `_` or be a route name (`users`, `roles`, `health`, ...).

### Users and Roles

Passwords are hashed with Argon2 and never returned. Users can be addressed by ID or login. The last unlocked admin can't be deleted, locked or demoted.

```bash
curl http://localhost:7700/users
curl -X POST http://localhost:7700/users -H "Content-Type: application/json" -d '{
  "login": "ada", "password": "correct horse battery", "email_address": "ada@example.com",
  "roles": [{ "name": "writer", "permissions": ["articles"] }] }'
curl http://localhost:7700/users/ada
curl -X PATCH http://localhost:7700/users/ada -H "Content-Type: application/json" -d '{ "password": "a new long password" }'
curl -X DELETE http://localhost:7700/users/ada

curl http://localhost:7700/roles
curl http://localhost:7700/roles/admin
```

PUT on a user replaces its editable fields and requires `email_address` and `roles`. Roles and permissions aren't enforced yet; requests are not authenticated.

### Filters

REST (listing, `_query`, `count`, `_update`) and GraphQL share one filter language. A filter is a JSON object; every condition must hold.

```json
{
  "status": "draft",
  "views": { "$gte": 10, "$lt": 1000 },
  "author.name": "Ada",
  "tags": "rust",
  "$or": [{ "featured": true }, { "pinned": true }]
}
```

| Operator | Meaning |
| --- | --- |
| `value` or `$eq` | Equal (`1` equals `1.0`; `null` also matches a missing field) |
| `$ne` | Not equal |
| `$gt`, `$gte`, `$lt`, `$lte` | Compare numbers, strings or booleans |
| `$in`, `$nin` | One of / none of an array of values |
| `$exists` | The field is present (`true`) or absent (`false`) |
| `$contains` | Substring of a string, or element of an array |
| `$startsWith`, `$endsWith` | String prefix or suffix |
| `$not` | Negates the operators inside it |
| `$and`, `$or`, `$not` (top level) | Combine filters |
| `$text` (top level) | Full-text search: every word must appear (case-insensitive, whole words). Searches the tessellation's text index fields, or every string field when it has none |

Dotted paths reach into nested objects, and a condition on an array field matches if any element matches. Every operator can also be written with `_` instead of `$` (`_gte`, `_or`); use that form inside GraphQL query text, where `$` marks a variable.

### GraphQL

`POST /graphql` accepts standard GraphQL requests (`{"query", "variables", "operationName"}`). `GET /graphql` serves GraphiQL. The admin UI's **Queries** page is the main way to explore it.

```graphql
query Recent($filter: JSON) {
  documents(
    tessellation: "articles"
    filter: $filter
    sort: [{ field: "views", descending: true }]
    limit: 25
  ) {
    total
    next
    documents { id expiresAt data field(path: "author.name") }
  }
  count(tessellation: "articles", filter: { published: { _eq: false } })
}
```

| Queries | Mutations |
| --- | --- |
| `tessellations`, `tessellation(name)` with `documentCount` and `documents` | `insertDocument`, `insertDocuments` (atomic) |
| `document(tessellation, id)` | `replaceDocument`, `patchDocument`, `deleteDocument` |
| `documents(tessellation, filter, sort, limit, offset, after)` | `updateDocuments(tessellation, filter, update)` (atomic) |
| `count(tessellation, filter)` | `createTessellation`, `deleteTessellation` |
| `aggregate(tessellation, filter, groupBy, aggregates, sort, limit, offset)` | `transaction(operations)` |
| `changes(after, tessellation, limit)` | `createIndex`, `dropIndex` |
| `users`, `user(idOrLogin)`, `roles`, `status` | |

Document mutations take an optional `idempotencyKey` argument; replayed mutations are listed in the response's `extensions.idempotentReplays` and the HTTP response carries `Idempotent-Replayed: true`. Documents expose their fields through the `JSON` scalar (`data`, `json`, or `field(path)`). Without `sort`, results come in ID order and `next` pages forward via `after`; with `sort`, page with `offset`. Errors include `extensions.code` (`NOT_FOUND`, `INVALID_REQUEST`, `FORBIDDEN`, `CONFLICT`, ...). System tessellations aren't reachable through document fields.

### Aggregations

Group the documents that match a filter and summarize each group. Operators: `$count` (`"*"` for documents, or a field for non-null values), `$countDistinct`, `$sum`, `$avg`, `$min`, `$max`. Each row holds the group-by fields and one column per aggregate; without `group_by` there's a single row.

```bash
curl -X POST http://localhost:7700/orders/_aggregate -H "Content-Type: application/json" -d '{
  "filter": { "status": { "$ne": "cancelled" } },
  "group_by": ["customer.country"],
  "aggregates": { "orders": { "$count": "*" }, "revenue": { "$sum": "total" }, "average": { "$avg": "total" } },
  "sort": "-revenue",
  "limit": 10 }'
# => { "rows": [{ "customer.country": "US", "orders": 61, "revenue": 171234.5, "average": 2807.1 }, ...],
#      "total_groups": 10, "matched": 351 }
```

GraphQL: `aggregate(tessellation: "orders", groupBy: ["status"], aggregates: { n: { _count: "*" } }) { rows totalGroups matched }`.

### Transactions

`POST /transactions` runs operations across any user tessellations atomically: all of them are written in one WAL record, or none are (rollback). Later operations see the effects of earlier ones. Operations: `get`, `check`, `insert`, `replace`, `patch`, `delete`. Preconditions on any operation with an `id`:

- `if_version`: the document's version must equal this (`0` means it must not exist). Versions come back in every result and in the `ETag` header of `GET /{tessellation}/{id}`.
- `if_match`: the document must exist and match this filter.

```bash
curl -X POST http://localhost:7700/transactions -H "Content-Type: application/json" -d '{ "operations": [
  { "op": "patch", "tessellation": "accounts", "id": "01J...A", "data": { "balance": 70 }, "if_match": { "balance": { "$gte": 30 } } },
  { "op": "patch", "tessellation": "accounts", "id": "01J...B", "data": { "balance": 35 } },
  { "op": "insert", "tessellation": "ledger", "data": { "from": "01J...A", "to": "01J...B", "amount": 30 } } ] }'
# => { "results": [{ "op": "patch", "id": "...", "version": 812, "document": {...} }, ...], "writes": 3 }
```

A failed precondition returns 409 and a missing document 404; nothing is written. Every document a transaction reads or writes is validated at commit, so concurrent transactions behave as if they ran one at a time. Up to 1,000 operations; `Idempotency-Key` works as for other writes. GraphQL: `transaction(operations: JSON!, idempotencyKey)`.

### Indexes

Secondary indexes speed up filtered queries, counts, and aggregations; results are identical with or without them. Every document is already reachable by its `id`, a ULID that HexDB generates and that sorts by creation time.

- **Field indexes** cover one or more fields (composite) and can be `unique`. They answer equality, `$in`, ranges, and `$startsWith` on the first field, and equality on all fields together. Array values index each element.
- **Text indexes** (an inverted index of words, one per tessellation, over any string fields) answer `$text`.

```bash
curl -X POST http://localhost:7700/tessellations/orders/indexes -H "Content-Type: application/json" -d '{ "fields": ["status"] }'
curl -X POST http://localhost:7700/tessellations/orders/indexes -H "Content-Type: application/json" -d '{ "fields": ["customer.country", "status"] }'
curl -X POST http://localhost:7700/tessellations/customers/indexes -H "Content-Type: application/json" -d '{ "fields": ["email"], "unique": true }'
curl -X POST http://localhost:7700/tessellations/articles/indexes -H "Content-Type: application/json" -d '{ "fields": ["title", "body"], "kind": "text" }'
curl http://localhost:7700/tessellations/orders/indexes
curl -X DELETE http://localhost:7700/tessellations/orders/indexes/status
```

Query responses include `"plan": { "indexes": [...], "scanned": n }` (GraphQL: `indexesUsed`, `scanned`). Index definitions are stored in `catalog.json`; their contents live in memory and are rebuilt at startup. A write that would duplicate a unique key fails with 409. Manage indexes in the admin UI from the Tessellations page.

### Change Feed

Every committed write is published, in order, once it is durable: `{"seq", "timestamp", "op": "put" | "delete" | "drop_tessellation", "tessellation", "id", "document"}`. System tessellations are left out.

```bash
curl "http://localhost:7700/changes"                          # the current position: { "changes": [], "last_seq": 830 }
curl "http://localhost:7700/changes?after=830&wait=30"        # long poll; pass last_seq back as after
curl "http://localhost:7700/changes?after=830&tessellation=orders&limit=100"
curl -N "http://localhost:7700/changes/stream?after=830"      # Server-Sent Events (event: change, id: <seq>)
```

The last 10,000 changes are kept in memory, starting when the server starts. Asking for older ones returns 410 (`history_expired`): re-read what you need and continue from a fresh `last_seq`. GraphQL: `changes(after, tessellation, limit) { changes lastSeq }`.

### Plugins

Plugins receive the change feed while their hex is the Overseer. The registry (`plugins.registry`, default `plugins.json` next to the config file) lists plugin folders, each with a `plugin.toml`:

```toml
id = "@examples/change-logger"
name = "Change logger"
version = "0.1.0"
command = ["node", "change-logger.mjs"]   # a process: one change per line on stdin
# tessellations = ["orders"]              # optional filter

# ...or a webhook: batches of changes POSTed as a JSON array
# [webhook]
# url = "http://localhost:9000/hexdb"
# headers = { Authorization = "Bearer ..." }
# batch_size = 100
```

A process plugin's stdout and stderr go to the HexDB log, and it's restarted if it exits. `GET /plugins` (and the Plugins page) shows each plugin's state, deliveries, and errors. Plugins start at the current end of the feed, so changes made while a plugin is down are skipped; use `/changes` with a stored cursor when every change must be processed. See [plugins/examples/change-logger](plugins/examples/change-logger) (disabled in [plugins.json](plugins.json) by default).

### Logs

The server keeps its last 5,000 log records in memory. The admin UI's Logs page tails them with level, module, and text filters.

```bash
curl "http://localhost:7700/logs?level=warn&limit=100"        # oldest first; "last_seq" is the newest record
curl "http://localhost:7700/logs?after=1200&q=index"          # tail after a record, search messages and fields
curl "http://localhost:7700/logs?target=hexdb_core::network"  # module prefix
```

Writes, errors, and background tasks are logged at info and above; reads at debug. `RUST_LOG` sets the level for both the console and the in-memory log.

### Operations

```bash
curl http://localhost:7700/health
curl http://localhost:7700/status
curl "http://localhost:7700/status/history?minutes=60"   # metrics samples every 15 s, kept for 6 hours
curl -X POST http://localhost:7700/flush    # write unflushed data to SSTables
curl http://localhost:7700/plugins          # loaded plugins and their delivery state
```

### Lattices and Replication

Hexes with the same `network.lattice_name` that can reach each other's discovery endpoints form a lattice. Discovery runs every `network.discovery_interval_seconds` and elects one **Overseer**:

- The sitting Overseer keeps the role while it's reachable, so a returning hex doesn't take it back.
- Otherwise the hex preferring `overseer` wins, then the one with the most RAM, then the most disk, then the lowest ID.
- Hexes with `role = "harvester"` or `"replicant"` never lead.

| Role | What it does |
| --- | --- |
| Overseer | Takes all writes, publishes the change feed, and runs plugins. |
| Harvester | A read replica: keeps a full copy by following the Overseer, and serves reads. Can be elected Overseer. |
| Replicant | A standby copy that follows the Overseer like a Harvester but is never elected. Use it for backups or to rebuild Harvesters. |

A new replica copies a snapshot from the Overseer, then streams its changes; `/status` shows `replication.state` (`streaming`, `syncing`, ...) and `lag`. Writes sent to a replica fail with 421 (`read_only_replica`) and name the Overseer's address. If the Overseer stops answering for about three discovery rounds, the remaining hexes elect a new one and the others re-sync from it. Replication is asynchronous: a write the Overseer acknowledged but no replica received is lost if the Overseer fails before it comes back. Hexes in a lattice must share `storage.encryption_key`, which also authenticates replication between them.

```toml
[network]
api_endpoint = "10.0.0.11:7700"
discovery_endpoint = "10.0.0.11:7702"
lattice_name = "Nebula Prime"
peers = ["10.0.0.12:7702", "10.0.0.13:7702"]  # other hexes' discovery endpoints
discovery_interval_seconds = 10
# advertise_host = "db1.example.com"          # address others should use, if different

[identity]
role = "auto"   # auto | overseer | harvester | replicant
```

Hexes on one machine also find each other on discovery ports 7702-7709 (`scan_local_ports = true`).

### Configuration

The development configuration is [hexdb_api/hexdb.toml](hexdb_api/hexdb.toml). The config file is found in this order:

1. `--config <path>`
2. the `HEXDB_CONFIG` environment variable (set automatically for `cargo run` by [.cargo/config.toml](.cargo/config.toml))
3. `hexdb.toml` in the working directory
4. `hexdb.toml` next to the executable

Relative paths in the file (`storage.path`, `ui.path`) are resolved against the config file's directory. Missing values fall back to built-in defaults, and any value can be overridden with an environment variable named `HEXDB_<SECTION>__<FIELD>`, for example `HEXDB_NETWORK__API_ENDPOINT=0.0.0.0:7700`.

`storage.encryption_key` is required and must be 32 random bytes, base64-encoded with a `base64:` prefix:

```bash
echo "base64:$(openssl rand -base64 32)"
```

## Terminology

### Hex

A Hex is a running instance of HexDB.

### Vertex

A vertex is an area of memory (RAM) within a Hex. A hex has 6 vertices where data is stored. Each document in memory is split into 6 equal shards with Reed-Solomon coding, 4 data and 2 parity, and each vertex holds one shard. Every shard carries a BLAKE3 hash, so if a vertex's copy is corrupted, the document is rebuilt from the remaining shards. Any 2 of the 6 vertices can be lost or corrupt without losing data, and a background check repairs corrupt shards in place.

### Tessellation

A tessellation is a collection of stored documents. Tessellations are used to compartmentalize data, and to limit access to specific roles and users.

### Lattice

A lattice is a networked group of hexes with one Overseer. A single hex is the Overseer of its own lattice; add Harvesters for read capacity and failover, and Replicants for standby copies. See "Lattices and Replication" above.

### Hex Types

- Overseer
  - The primary: takes all writes, publishes the change feed that replicas follow, and runs plugins
- Harvester
  - A read replica that can be elected Overseer if the Overseer fails
- Replicant
  - A standby copy that is never elected; for backups and rebuilding Harvesters

## Features

### Completed

- ✅ Command-line interface
- ✅ REST API
  - ✅ Documents (create, read, replace, patch, delete, list, count)
  - ✅ Bulk writes (atomic insert, replace, patch, and update by filter)
  - ✅ Idempotency keys
- ✅ GraphQL API (queries, filters, sorting, paging, aggregations, transactions, mutations) with an in-UI query console
- ✅ Admin UI (live dashboard, query console, tessellations and indexes, documents, users, roles, plugins, logs, light and dark themes)
- ✅ Collections (Tessellations)
- ✅ Filters, sorting, and paging
- ✅ Aggregations (count, count distinct, sum, average, min, max, group by)
- ✅ ACID transactions across tessellations (preconditions, rollback, serializable for the documents they touch)
- ✅ Indexes: field, composite, unique, and full-text (inverted); ULID primary keys
- ✅ Full-text search (`$text`)
- ✅ Change data stream (polling, long polling, Server-Sent Events)
- ✅ Network discovery and Overseer election, with failover
- ✅ Data replication (snapshot plus change stream; Harvester and Replicant read replicas)
- ✅ Plugin loader (process and webhook plugins on the change stream)
- ✅ Logging (in-memory log with API and UI)
- ✅ Configuration (endpoints, lattice, RAM and disk budgets, compression, plugins)
- ✅ Typed storage of JSON values (strings, integers up to 128-bit, floats, booleans, RFC 3339 datetimes, arrays, objects)
- ✅ Write-ahead logging and crash recovery
- ✅ Encryption at rest for the WAL (AES-256-GCM)
- ✅ Compression (Zstandard)
- ✅ SSTables (long-term storage), compaction, WAL rotation
- ✅ TTL (per-document expiry)
- ✅ Vertex sharding with Reed-Solomon repair
- ✅ Graceful shutdown

### In-Progress

- Authentication and enforcement of roles and permissions (Phase 7). Requests are not authenticated yet.

### Planned

- Horizontal partitioning (sharding data across hexes; today every hex holds a full copy)
- Synchronous replication options and write forwarding from replicas
- Persistent change feed cursors for plugins
- Decimal and binary values through the API (the storage types exist)
- Open Telemetry
- Ingest sources
- Locking
- Schemas & versioning
- API documentation (OAS 3.0 & Swagger)
- Clients (.NET, Node)
- Community packages (wget, brew, chocolatey, apt-get)
- Network load balancing by the Overseer
- IP address blocking (IP or CIDR) and allow lists
- Programmable stored actions and queries, with chaining

## Appendix

### Storage Layout

```text
<storage.path>/
├── catalog.json              tessellations and their index definitions, and the sequence number of each dropped one
├── hexdb.pid                 runtime file of the running server (PID, endpoint, shutdown token)
├── wal/
│   └── <first-seq>.wal       write-ahead log segments, oldest first
└── <tessellation>/
    └── <ulid>.hxs            SSTables
```

Every write gets a sequence number. Deletes are recorded as tombstones. When several versions of a document exist in memory, the WAL, or SSTables, the one with the highest sequence number wins.

### WAL Record Format

Each record is `[u32 length][12-byte nonce][ciphertext]`. The ciphertext is AES-256-GCM over the Zstandard-compressed JSON record `{ "seq": ..., "op": { "Put": <document> } | { "Delete": { "tessellation", "id" } } }`.

### SSTable Header Structure (version 2)

All integers are big-endian.

| Offset | Field                   | Size     |
| ------ | ----------------------- | -------- |
| 0x00   | MAGIC (`HXDB`)          | 4 bytes  |
| 0x04   | VERSION (2)             | 2 bytes  |
| 0x06   | COMPRESSION TYPE (zstd) | 1 byte   |
| 0x07   | Reserved                | 1 byte   |
| 0x08   | Entry Count             | 8 bytes  |
| 0x10   | Created Timestamp (ms)  | 8 bytes  |
| 0x18   | Index Offset            | 8 bytes  |
| 0x20   | Index Size              | 8 bytes  |
| 0x28   | Index Checksum          | 8 bytes  |
| 0x30   | Max Sequence Number     | 8 bytes  |
| 0x38   | Reserved                | 8 bytes  |
| 0x40   | Start of data           | ...      |

The index checksum is the first 8 bytes of a BLAKE3 hash of the index block.

### SSTable Entry and Index Records

| Field        | Size     | Entry | Index | Notes                                     |
| ------------ | -------- | ----- | ----- | ----------------------------------------- |
| Document ID  | 16 bytes | ✓     | ✓     | ULID                                      |
| Flags        | 1 byte   | ✓     | ✓     | bit 0: has TTL, bit 1: tombstone          |
| Sequence     | 8 bytes  | ✓     | ✓     |                                           |
| TTL          | 8 bytes  | ✓     | ✓     | only if bit 0 is set; epoch milliseconds  |
| Entry Offset | 8 bytes  |       | ✓     |                                           |
| Length       | 4 bytes  | ✓     | ✓     | compressed body length (0 for tombstones) |
| Body         | Length   | ✓     |       | Zstandard-compressed JSON document        |
