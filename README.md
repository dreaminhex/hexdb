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

### 5. Access the UI

Browse to [http://localhost:7700/ui/](http://localhost:7700/ui/). The **Queries** page is a GraphQL console with schema-aware autocomplete, validation, example queries, history, and JSON or table results. Press Ctrl+Enter (⌘+Enter on macOS) to run.

For UI development, run `npm run dev` in `hexdb_admin` while a HexDB server is running; API calls are proxied to `http://127.0.0.1:7700` (override with `HEXDB_API`).

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

Set `HEXDB_TEST_KEEP=1` to keep each test's directory (config, data, and `server.log`) for inspection. Failed tests always keep theirs and print the server log tail.

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

# List in ID order, 1-1000 per page (default 100). Pass "next" from the response as ?after= for the next page.
curl "http://localhost:7700/articles?limit=50"

# Count
curl http://localhost:7700/articles/count
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

Every write accepts an `Idempotency-Key` header (1-255 printable ASCII characters). The key and the result are stored atomically with the write for 24 hours. Retrying the same request with the same key returns the original response with `Idempotent-Replayed: true` instead of writing again, even after a crash. Reusing a key for a different request returns 422; a concurrent request with a key that is still being processed returns 409.

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

The REST `_update` endpoint and GraphQL share one filter language. A filter is a JSON object; every condition must hold.

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
| `users`, `user(idOrLogin)`, `roles`, `status` | |

Documents expose their fields through the `JSON` scalar (`data`, `json`, or `field(path)`). Without `sort`, results come in ID order and `next` pages forward via `after`; with `sort`, page with `offset`. Errors include `extensions.code` (`NOT_FOUND`, `INVALID_REQUEST`, `FORBIDDEN`, `CONFLICT`, ...). System tessellations aren't reachable through document fields.

### Operations

```bash
curl http://localhost:7700/health
curl http://localhost:7700/status
curl -X POST http://localhost:7700/flush    # write unflushed data to SSTables
```

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

A lattice is a networked group of three or more hexes. A lattice must have one Overseer hex, one or more Harvester hexes, and one or more Replicant hexes.

### Hex Types

- Overseer
  - Primary, routes tasks to Harvester hexes, adds new hexes to the lattice when discovered
- Harvester
  - Serves data when requested by an Overseer
- Replicant
  - Cold data storage, can be restored by an Overseer if a dead Harverster is detected

## Features

### Completed

- ✅ Command-line interface
- ✅ REST API
  - ✅ Documents (create, read, replace, patch, delete, list, count)
  - ✅ Bulk writes (atomic insert, replace, patch, and update by filter)
  - ✅ Idempotency keys
- ✅ GraphQL API (queries, filters, sorting, paging, mutations) with an in-UI query console
  - ✅ Tessellations (list, create, read, delete)
  - ✅ Users (create, read, update, delete)
  - ✅ Roles (read)
  - Permissions (read)
  - ✅ Status (hex metrics)
  - ✅ Health
- ✅ Data replication
- ✅ Collections (Tessellations)
- ✅ Plugin Ecosystem
- ✅ Horizontal Partitioning
- ✅ Configuration (URLs, RAM/DISK usage. compression)
- ✅ Strong Typing (string, 32-, 64, 128-bit integer, boolean, datetime, binary)
- ✅ Type Introspection
- ✅ Write-Ahead Logging & Recovery
- ✅ Encryption (AES-GCM)
- ✅ Compression (zstd, default 0)
- ✅ Self-tuning flush heuristics
- ✅ SSTables integration (long-term storage)
- ✅ Compaction
- ✅ WAL file rotation logic
- ✅ TTL Sweeps
- ✅ Percolation
- ✅ Hot set caching
- ✅ Graceful task shutdown
- ✅ Recovery
- ✅ Configuration

### In-Progress

- Roles and permissions enforcement, authentication

### Planned

- Read/Write Endpoints
- Network Discovery
- Open Telemetry
- Aggregations
- Ingest Sources
- Indexes (Primary, Composite)
- Full text search
- Locking
- Change Data Streams
- ACID Transactions
- Logging
- Schemas & Versioning
- User Interface
- API Documentation (OAS 3.0 & Swagger)
- Clients (.NET, Node)
- Community (wget, brew, chocolatey, apt-get)
- Network load balancing by manager node
- IP address blocking (IP or CIDR)
- Whitelist
- Programmable stored actions and queries, with chaining

## Appendix

### Storage Layout

```text
<storage.path>/
├── catalog.json              tessellations, and the sequence number of each dropped one
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
