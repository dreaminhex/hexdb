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
- GrahpQL Query Syntax
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

Browse to [http://localhost:7700/ui/](http://localhost:7700/ui/)

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

### 1. Insert a Document

```bash
curl -X POST http://localhost:7700/articles \
  -H "Content-Type: application/json" \
  -d '{ "title": "Quantum Tessellation", "tags": [ "hexdb", "rust", "ai" ], "published": true, "views": 445 }'
```

To have a document expire, add `ttl` (in seconds) to an insert, update or patch:

```bash
curl -X POST "http://localhost:7700/sessions?ttl=3600" \
  -H "Content-Type: application/json" \
  -d '{ "user": "ada" }'
```

### 2. Fetch a Document

```bash
curl http://localhost:7700/articles/01JTY87RVJ9B5863KMB2YD896B
```

### 3. Count Documents

```bash
curl http://localhost:7700/articles/count
```

### 4. Delete a Document

```bash
curl -X DELETE http://localhost:7700/articles/01JTY87RVJ9B5863KMB2YD896B
```

### 5. Create Tessellation

```bash
curl -X POST http://localhost:7700/tessellation \
  -H "Content-Type: application/json" \
  -d '{ "name": "articles" }'
```

### 6. Delete Tessellation

```bash
curl -X DELETE http://localhost:7700/tessellation/articles
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
  - ✅ Documents (create, read, update, delete)
  - ✅ Tessellations (create/delete)
  - Users (create, read, update, delete)
  - Roles (create, read, delete)
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

- Roles, Users, Permissions

### Planned

- Read/Write Endpoints
- Network Discovery
- Open Telemetry
- Aggregations
- Ingest Sources
- Indexes (Primary, Composite)
- Full text search
- Idempotency & Locking
- Change Data Streams
- Bulk APIs
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
