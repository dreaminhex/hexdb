# HexDB

**HexDB** is a distributed, document-oriented database engine designed for performance, developer usability, and deep AI integration. Inspired by decades of systems design and forged for modern workloads, it supports dynamic indexing, ACID-compliant transactions, full-text search, GraphQL queries, and cluster discovery, among many other features.

## 🧠 Philosophy

HexDB isn't just another database — it's an attempt to reimagine what modern persistence looks like with AI-native guidance, extensibility, and performance-aware modularity at its core.

"Dream in Hex. Remember it with HexDB."

---

## 🧱 Project Structure

hexdb/
├── hexdb_core
├── hexdb_api
├── hexdb_cli
├── hexdb_query
└── plugins

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

Or you can run the API directly using `cargo`.

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

A vertex is an area of replication within a Hex. A hex has 6 vertexes (or vertices) where data is stored. Each vertex has a different memory address. Database bytes are replicated in chunks across all 6 vertices so that if any part of a Hex's memory becomes corrupted, data can be recovered from the remaining uncorrupted vertices.

### Tessellation

A tessellation is a collection of stored documents. Tessellations are used to compartmentalize data, and to limit access to specific roles and users.

## Features

### Completed

- ✅ Command-line interface
- ✅ REST API
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

### In-Progress

- WAL file rotation logic
- TTL Sweeps
- Percolation
- Hot set caching

### Planned

- Compaction
- Read/Write Endpoints
- Network Discovery
- Open Telemetry
- Aggregations
- Ingest Sources
- Roles & Users
- Indexes (Primary, Composite)
- Full text search
- Idempotency & Locking
- Change Data Streams
- Bulk APIs
- ACID Transactions
- Recovery
- Logging
- Schemas & Versioning
- User Interface
- Clients (.NET, Node)
- Community (wget, brew, chocolatey, apt-get)
