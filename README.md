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

Or you can run the node directly using `cargo`.

```bash
cargo run -p hexdb_api
```

### 4. Ping Health Endpoint

```bash
curl http://localhost:7700/health
```

### 5. Access UI

Browse to [http://localhost:7700/index.html](http://localhost:7700/index.html)

### 6. Stop the Node

```bash
hexdb stop
```

## API Operations

### 1. Insert a Document

```bash
curl -X POST http://localhost:7700/api \
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
curl http://localhost:7700/api/test123
```

### 3. Count Documents

```bash
curl http://localhost:7700/count
```

### 4. Delete a Document

```bash
curl -X DELETE http://localhost:7700/api/test123
```

### Configuration

Configuration values can be found in the root.

[hexdb.toml](hexdb.toml)

By default, HexDB uses the following values by default:

```toml
engine_endpoint = "127.0.0.1:7700"
query_endpoint = "127.0.0.1:7701"
discovery_endpoint = "127.0.0.1:7702"
ram_mb = 1024
disk_mb = 16384
```
