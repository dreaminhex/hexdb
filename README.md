# HexDB

**HexDB** is a distributed, document-oriented database engine designed for performance, developer usability, and deep AI integration. Inspired by decades of systems design and forged for modern workloads, it supports dynamic indexing, ACID-compliant transactions, full-text search, GraphQL queries, and cluster discovery.

---

## 🧱 Project Structure

hexdb/
├── hexdb_core # Document model, indexing, storage engine
├── hexdb_node # Server runtime (HTTP/TCP/WebSocket, discovery, RPC)
├── hexdb_cli # Command-line interface for control + queries
└── hexdb_query # GraphQL + Full-text search query layer

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
cargo run -p hexdb_cli -- health
```

🔮 Roadmap Features

- Document storage (JSON/BSON) with TTL
- Inverted and composite indexing
- RAM-aware document percolation
- Distributed transactions with rollback
- GraphQL-native query support
- WebSocket and TCP-based node discovery
- AI-assisted schema inference and query planning
- Pluggable storage engines (LSM, B-tree, log-structured)
- Encryption at rest and adaptive compression

🧠 Philosophy

HexDB isn't just another database—it's an attempt to reimagine what modern persistence looks like with AI-native guidance, extensibility, and performance-aware modularity at its core.

"Dream in Hex. Remember it in HexDB."

