# HexDB

HexDB is a document database written in Rust. Documents are JSON, grouped into collections called tessellations, and served over REST and GraphQL from a single binary that also hosts an admin UI. Several HexDB servers (hexes) form a lattice: one Overseer takes writes, and the others replicate its data, serve reads, and take over if it fails.

The [technical manual](MANUAL.md) covers every feature in depth, including how the storage engine, encryption, replication and indexes work.

## Features

- **Documents and queries.** Insert, replace, patch, delete, bulk writes, upserts, update-by-filter, TTLs and idempotency keys. A JSON filter language with sorting and paging, shared by REST and GraphQL.
- **Indexes and search.** Field, composite and unique indexes. Full-text indexes with configurable analyzers (stemming, n-grams, autocomplete). A query advisor that suggests indexes from observed queries, with an optional second opinion from Claude.
- **Aggregations and transactions.** Group-by with count, distinct count, sum, average, min and max. Multi-document ACID transactions across tessellations, with version and filter preconditions.
- **Schemas.** Optional, versioned per tessellation, with compatibility checks and background migrations (rename, copy, remove, default, convert).
- **Change data.** An ordered change feed (polling, long polling, Server-Sent Events) backed by an on-disk history, so consumers can resume after a restart.
- **Streams.** Named publish/subscribe logs with consumer groups and retention. They can be fed from tessellation changes and delivered to webhooks.
- **Functions.** Saved queries, aggregations, transactions and scripts (Python, TypeScript, JavaScript), run by name with typed parameters. Schedules run them on an interval or a cron expression.
- **Plugins.** Change data capture (Kafka, Kinesis), ingest sources (PostgreSQL, SQL Server, S3), log and metrics sinks (OpenTelemetry, files, HTTP) and enrichers. A plugin can be a process, a webhook or built in.
- **Replication.** Automatic discovery and Overseer election, snapshot plus change-stream replication, and failover. Optional synchronous acknowledgements, a write quorum, and write forwarding from replicas.
- **Security.**
  - Authentication: sessions, API keys and TOTP multi-factor authentication.
  - Access control: built-in and custom roles granted per tessellation.
  - Audit and abuse protection: a persistent audit trail and lattice-wide sign-in throttling.
  - Encryption: AES-256-GCM encryption at rest with key rotation, TLS, and mutually authenticated hex-to-hex traffic.
- **Resilience.** A write-ahead log with group commit, SSTables with compaction, and in-memory Reed-Solomon sharding that repairs corrupted memory.
- **Operations.** Online backups, an admin UI, a CLI, runtime settings, metrics history, logs, an OpenAPI description, and drivers for Node.js, Python and .NET.

## Quick start

### With Docker

```bash
docker build -t hexdb .
docker run -p 7700:7700 -v hexdb-data:/var/lib/hexdb \
  -e HEXDB_STORAGE__ENCRYPTION_KEY="base64:$(openssl rand -base64 32)" hexdb
```

The first start creates an administrator and prints a generated password in the container log (`docker logs <container> | grep password`). Open http://localhost:7700/ui/ and sign in. Keep the encryption key: the data can't be read without it.

`docker compose up -d` starts a three-hex lattice instead; see the comments at the top of [docker-compose.yml](docker-compose.yml).

### From source

You need Rust (stable) and Node.js 22 or later. On Windows, also install the Visual Studio Build Tools ("Desktop development with C++").

```bash
cargo build --release -p hexdb_api -p hexdb_cli
(cd hexdb_admin && npm ci && npm run build)

# A key for encryption at rest, kept out of version control:
./target/release/hexdb secret                  # prints base64:...
# then put it in hexdb_api/hexdb.local.toml:
#   [storage]
#   encryption_key = "base64:..."

./target/release/hexdb --config hexdb_api/hexdb.toml start
```

During development, `cargo run -p hexdb_api` from the repository root uses [hexdb_api/hexdb.toml](hexdb_api/hexdb.toml).

On first start with no users, HexDB creates the administrator named by `security.admin_login`. It prints a generated password once and saves it to `initial-admin-password.txt` in the data directory, readable by your user only. Sign in at http://localhost:7700/ui/, change the password on the Account page, and delete that file.

### First requests

Create an API key on the Account page, then:

```bash
export HEXDB_TOKEN=hxk_...
curl -X POST http://localhost:7700/orders -H "Authorization: Bearer $HEXDB_TOKEN" \
  -H "Content-Type: application/json" -d '{ "customer": "ada", "total": 42, "status": "paid" }'

curl -G http://localhost:7700/orders -H "Authorization: Bearer $HEXDB_TOKEN" \
  --data-urlencode 'filter={"status":"paid","total":{"$gte":10}}' --data-urlencode 'sort=-total'
```

To load sample data (articles, products, customers, orders and short-lived sessions), run `HEXDB_TOKEN=hxk_... node scripts/seed.mjs`.

## Command-line interface

```text
hexdb start [-s]                 start the server (-s: in the background, logging to hexdb.log)
hexdb stop [--force]             stop it gracefully
hexdb health                     is it up?
hexdb status                     status and metrics (needs HEXDB_TOKEN)
hexdb secret                     print a new random key
hexdb backup [--name N] [--list] back up the running server (needs HEXDB_TOKEN)
hexdb plugins add|remove|list    manage the plugin registry
hexdb lattice spawn --count 2    start more hexes on this machine that join its lattice
hexdb lattice list|stop          list or stop them (stop --remove deletes their data)
```

Every command takes `--config <path>`. The config is otherwise found through `HEXDB_CONFIG`, `./hexdb.toml`, or `hexdb.toml` next to the executable.

## Admin UI

The admin UI is served at `/ui/`. Users see only the pages their roles allow.

| Page | What it does |
| --- | --- |
| Dashboard | Live document, memory, disk and operation figures; activity charts; vertex health; the lattice, and "Add a hex" for joining new servers; flush and back up |
| Queries | A GraphQL console with schema-aware completion, examples and history |
| Tessellations | Create and delete tessellations; manage indexes (with analyzer choice and index suggestions) and schemas |
| Documents | Browse with filters and sorting; create, edit and delete documents |
| Streams | Create streams, publish, read messages, and watch consumer groups and deliveries |
| Functions | Create and run functions; manage schedules |
| Users, Roles | Accounts, role grants, custom roles, MFA resets |
| Audit Trail | Security events, filterable by user, action and time |
| Plugins | Loaded plugins, their state and deliveries |
| Logs | The server log, tailed live |
| Settings | Runtime settings and the effective configuration |
| Account | Your password, MFA and API keys |

## Drivers

| Language | Folder | Requirements |
| --- | --- | --- |
| JavaScript / TypeScript | [drivers/node](drivers/node) | Node.js 18+, Deno, Bun or a browser |
| Python | [drivers/python](drivers/python) | Python 3.9+, standard library only |
| .NET | [drivers/dotnet](drivers/dotnet) | .NET 8+ |

See [drivers/README.md](drivers/README.md). The REST API is described by `GET /openapi.json` ([hexdb_api/openapi.json](hexdb_api/openapi.json)).

## Installing

Release builds come from [.github/workflows/release.yml](.github/workflows/release.yml), which produces archives for Linux, macOS and Windows plus a Debian package. The [packaging](packaging) folder has an install script, a systemd unit, and Homebrew, Chocolatey and winget manifests. Each manifest's version and checksums are filled in at release time.

## Repository layout

```text
hexdb_core/     storage engine, replication, security, plugins, streams, functions
hexdb_api/      HTTP server (REST, GraphQL, admin UI hosting) and hexdb.toml
hexdb_query/    GraphQL schema
hexdb_cli/      the hexdb command
hexdb_admin/    admin UI (React, Vite)
hexdb_tests/    end-to-end tests that run real servers
drivers/        client libraries
plugins/        plugin SDK and example plugins
packaging/      installers and service files
docker/         container configuration
scripts/        seed data and the OpenAPI generator
```

## Development

```bash
cargo test --workspace                      # unit and end-to-end tests
cargo clippy --workspace --all-targets -- -D warnings
cd hexdb_admin && npm run dev               # UI dev server; proxies the API at 127.0.0.1:7700
```

The end-to-end tests in `hexdb_tests` start real servers on free ports in temporary directories. Some of them run several hexes and exercise failover. Set `HEXDB_TEST_KEEP=1` to keep each test's directory and server log. CI ([.github/workflows/ci.yml](.github/workflows/ci.yml)) runs the tests, clippy, `cargo audit`, `npm audit`, the UI build and the driver tests.

## Limitations

- Every hex holds a full copy of the data. Horizontal partitioning (sharding across hexes) isn't implemented.
- There's no SQL interface, so no ODBC or JDBC driver yet.
