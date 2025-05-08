# HexDB

**HexDB** is a distributed, document-oriented database engine designed for performance, developer usability, and deep AI integration. Inspired by decades of systems design and forged for modern workloads, it supports dynamic indexing, ACID-compliant transactions, full-text search, GraphQL queries, and cluster discovery, among many other features.

---

## 🧱 Project Structure

hexdb/
├── hexdb_core
├── hexdb_node
├── hexdb_cli
└── hexdb_query

---

## 🚀 Getting Started

### 1. Install Rust

```bash
curl https://sh.rustup.rs -sSf | sh
```

### 2. Build Everything

```bash
cargo build --workspace
```

### 3. Run a Node

```bash
cargo run -p hexdb_node
```

### 4. Ping Health Endpoint

```bash
curl http://localhost:7700/health
```

## API Operations

### 1. Insert a Document

```bash
curl -X POST http://localhost:7700/doc \
  -H "Content-Type: application/json" \
  -d '{
    "id": "test123",
    "body": { "foo": "bar", "count": 42 },
    "created_at": 1720000000,
    "ttl_seconds": null
  }'
```

### 2. Fetch a Document

```bash
curl http://localhost:7700/doc/test123
```

### 3. Count Documents

```bash
curl http://localhost:7700/count
```

### 4. Delete a Document

```bash
curl -X DELETE http://localhost:7700/doc/test123
```

🔮 Features

- Document storage (JSON/BSON) with TTL
- Inverted and composite indexing
- RAM-aware document percolation
- REST API
- Management UI
- Distributed transactions with rollback
- GraphQL-native query support
- WebSocket and TCP-based node discovery
- Horizontal partitioning
- AI-assisted schema inference and query planning
- Pluggable storage engines (LSM, B-tree, log-structured)
- Encryption at rest and adaptive compression
- Paging, projections, filters, and sorting
- Full-text search
- Cross-platform

🧠 Philosophy

HexDB isn't just another database — it's an attempt to reimagine what modern persistence looks like with AI-native guidance, extensibility, and performance-aware modularity at its core.

"Dream in Hex. Remember it with HexDB."
