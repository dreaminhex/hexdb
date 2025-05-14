# HexDB

**HexDB** is a distributed, document-oriented database engine designed for performance, developer usability, and deep AI integration. Inspired by decades of systems design and forged for modern workloads, it supports dynamic indexing, ACID-compliant transactions, full-text search, GraphQL queries, network discovery, pluggable components, and many other features.

## 🧠 Philosophy

HexDB isn't just another database — it's a reimagination of what modern persistence looks like with AI-native guidance, extensibility, and performance-aware modularity built directly into the core.

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

### 2. Build Everything & Install CLI

```bash
cargo build --workspace

cd hexdb_cli
cargo install --path .
```

### 3. Start the Node

Apply the `-s` argument to run HexDB in the background.

```bash
hexdb start -s
```

Or you can simply run the API directly using `cargo`.

```bash
cargo run -p hexdb_api
```

### 4. Ping Health Endpoint

```bash
curl http://localhost:7700/health
```

### 5. Access the UI

Browse to [http://localhost:7700/index.html](http://localhost:7700/index.html)

### 6. Stop the Node

```bash
hexdb stop
```

## API Operations

### 1. Insert a Document

```bash
curl -X POST http://localhost:7700/articles \
  -H "Content-Type: application/json" \
  -d '{ "title": "Quantum Tessellation", "tags": [ "hexdb", "rust", "ai" ], "published": true, "views": 445 }'
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

Configuration values can be found in the root.

[hexdb.toml](hexdb.toml)

## Terminology

### Hex

A Hex is a running instance of HexDB.

### Vertex

A vertex is an area of memory (RAM) replication within a Hex. A hex has 6 vertexes (or vertices) where data is stored. Each vertex has a different memory address. Database bytes are replicated in chunks across all 6 vertices so that if any part of a Hex's memory becomes corrupted, data can be recovered from the remaining uncorrupted vertices.

### Tessellation

A tessellation is a collection of stored documents. Tessellations are used to compartmentalize data, and to limit access to specific roles and users.

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

### SSTable Header Structure

| Offset | Field             | Size     |
| ------ | ----------------- | -------- |
| 0x00   | MAGIC             | 4 bytes  |
| 0x04   | VERSION           | 2 bytes  |
| 0x06   | COMPRESSION TYPE  | 1 byte   |
| 0x07   | Reserved          | 1 byte   |
| 0x08   | Entry Count       | 8 bytes  |
| 0x10   | Created Timestamp | 8 bytes  |
| 0x18   | Index Offset      | 8 bytes  |
| 0x20   | Index Size        | 8 bytes  |
| 0x28   | Checksum          | 8 bytes  |
| 0x30   | Reserved (16B)    | 16 bytes |
| 0x40   | Start of data     | ...      |
