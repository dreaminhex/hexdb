# HexDB

HexDB is a document database written in Rust. Documents are JSON, grouped into collections called tessellations, and served over REST, GraphQL and read-only SQL from a single binary that also hosts an admin UI. Several HexDB servers (hexes) form a lattice: one Overseer takes writes, and the others replicate its data, serve reads, and take over if it fails.

The [technical manual](MANUAL.md) covers every feature in depth, including how the storage engine, encryption, replication and indexes work.

## Features

- **Documents and queries.** Insert, replace, patch, delete, bulk writes, upserts, update-by-filter, TTLs and idempotency keys. A JSON filter language with sorting and paging, shared by REST and GraphQL, and SQL `SELECT` (with `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY` and paging) translated onto it.
- **Indexes and search.** Field, composite and unique indexes. Full-text indexes with configurable analyzers (stemming, n-grams, autocomplete). A query advisor that suggests indexes from observed queries, with an optional second opinion from Claude.
- **Aggregations and transactions.** Group-by with count, distinct count, sum, average, min and max. Multi-document ACID transactions across tessellations, with version and filter preconditions.
- **Schemas.** Optional, versioned per tessellation, with compatibility checks, background migrations (rename, copy, remove, default, convert, or a function you write), and rollback to an earlier version.
- **Change data.** An ordered change feed (polling, long polling, Server-Sent Events) backed by an on-disk history, so consumers can resume after a restart.
- **Streams.** Named publish/subscribe logs with consumer groups and retention. They can be fed from tessellation changes and delivered to webhooks.
- **Functions and triggers.** Saved queries, aggregations, transactions and scripts (Python, TypeScript, JavaScript), run by name with typed parameters. Schedules run them on an interval or a cron expression. Triggers run them when documents change: before a write, to check, change or refuse it, or after it commits.
- **Plugins.** Change data capture (Kafka, Kinesis), ingest sources (PostgreSQL, SQL Server, S3), log and metrics sinks (OpenTelemetry, files, HTTP) and enrichers. A plugin can be a process, a webhook or built in.
- **Replication.** Automatic discovery and Overseer election, snapshot plus change-stream replication, and failover. Optional synchronous acknowledgements, a write quorum, and write forwarding from replicas.
- **Security.**
  - Authentication: sessions, API keys and TOTP multi-factor authentication.
  - Access control: built-in and custom roles granted per tessellation, with row filters (for example, only the user's region) and hidden fields per role.
  - Audit and abuse protection: a persistent audit trail and lattice-wide sign-in throttling.
  - Encryption: AES-256-GCM encryption at rest with key rotation, TLS, and mutually authenticated hex-to-hex traffic.
- **Resilience.** A write-ahead log with group commit, SSTables with compaction, and in-memory Reed-Solomon sharding that repairs corrupted memory.
- **Operations.** Online backups, an admin UI, a CLI, runtime settings, metrics history, logs, an OpenAPI description, drivers for Node.js, Python and .NET, an Entity Framework Core provider, and an ODBC driver.

## Performance

Measured with `cargo run --release -p hexdb_bench` on a laptop (Intel Core i9-12900H, Windows 11, Samsung 980 PRO NVMe SSD), with one server and its client on the same machine. Every write is in the write-ahead log and fsynced before it's acknowledged. The full report, with how each number is taken, is in [bench/results/windows-full.md](bench/results/windows-full.md); run the benchmark to measure your own hardware.

| Area | Result |
| --- | --- |
| **Size** | 38 MB server, 9 MB CLI, 3 MB ODBC driver, 2 MB admin UI (52 MB in all) |
| **Writes** | 31,000 documents/s in bulk (1,000 per request); single writes 2.5 ms median, 1,700/s from 8 clients |
| **Reads** | 0.18 ms median by ID; 13,600/s from 8 clients |
| **Queries** | Indexed lookup in 100,000 documents: 0.8 ms median. `SELECT COUNT(*)`: 0.4 ms |
| **Memory** | 1.5x the JSON size: four data and two parity shards per document |
| **Disk** | Structured JSON compresses to 16% of its size (a 10 MB document takes 1.6 MB); small documents average 81% after encryption and per-document overhead |
| **Encryption** | AES-256-GCM: 890 MB/s encrypting, 1,150 MB/s decrypting, on one core |
| **10 MB documents** | Written in 600 ms and read in 190 ms (structured JSON); 94 ms and 28 ms for data that doesn't compress |
| **Vertex failure** | Documents stay readable with two of six vertices lost; rebuilding a lost vertex for 100,000 documents takes 0.4 s |
| **Crash recovery** | Restart after a kill: 0.5 s with data in SSTables; 2.6 s replaying 100,000 unflushed writes from the WAL |
| **Failover** | A new Overseer takes writes 2.6 s after the old one dies with `network.discovery_interval_seconds = 1`, or 31 s with the default of 10 s (a hex is declared lost after three missed rounds) |

Documents over 1 MB need `limits.max_document_kb` raised (up to 64 MB).

## Quick start

From a fresh clone to a running server and the admin UI takes five steps. Every command is run from the repository root unless a step says otherwise. On Windows, use PowerShell: Git Bash ships its own `link.exe`, which hides the Visual C++ linker and breaks Rust builds.

### 1. Install the prerequisites

You need Git, Rust (stable), Node.js 22 or later, and a C toolchain.

**Windows** (PowerShell; open a new terminal after installing):

```powershell
winget install --id Git.Git -e
winget install --id Rustlang.Rustup -e
winget install --id OpenJS.NodeJS.LTS -e
winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```

**Linux** (Debian and Ubuntu; on Fedora use `sudo dnf install gcc git pkg-config openssl-devel`):

```bash
sudo apt update && sudo apt install -y build-essential git pkg-config libssl-dev curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
```

Then install Node.js 22 or later, from [nodejs.org](https://nodejs.org/) or with a version manager such as `nvm install 22`.

**macOS**:

```bash
xcode-select --install                     # the C toolchain, if it isn't installed yet
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
brew install node                          # or download Node.js 22+ from nodejs.org
```

Check with `cargo --version` and `node --version` (v22 or later).

### 2. Get the code and build it

The same on every OS:

```bash
git clone https://github.com/dreaminhex/hexdb.git
cd hexdb
cd hexdb_admin
npm ci
npm run build
cd ..
cargo build --release -p hexdb_api -p hexdb_cli
```

The first Rust build takes a few minutes. It produces two programs in `target/release`: `hexdb_api` (the server) and `hexdb` (the CLI that starts, stops and inspects it). On Windows they end in `.exe`.

### 3. Put `hexdb` on your PATH

Pick one of the two options.

**Option A: install into Cargo's bin folder (recommended).** Rustup already added `~/.cargo/bin` (`%USERPROFILE%\.cargo\bin` on Windows) to your PATH, so this works the same on every OS:

```bash
cargo install --path hexdb_cli
cargo install --path hexdb_api
```

`hexdb start` runs the `hexdb_api` installed next to it. Run both commands again after pulling changes. Because the running server is the installed copy, `cargo build` in the repository still works while it runs.

**Option B: add `target/release` to your PATH.** Nothing is copied, but on Windows you must stop the server before rebuilding: Windows locks a running `.exe`, and `cargo build` fails with "failed to remove file".

Windows (PowerShell; open a new terminal afterwards):

```powershell
$bin = (Resolve-Path .\target\release).Path
[Environment]::SetEnvironmentVariable("Path", [Environment]::GetEnvironmentVariable("Path", "User") + ";$bin", "User")
```

Linux (bash):

```bash
echo "export PATH=\"$(pwd)/target/release:\$PATH\"" >> ~/.bashrc && . ~/.bashrc
```

macOS (zsh, the default shell):

```bash
echo "export PATH=\"$(pwd)/target/release:\$PATH\"" >> ~/.zshrc && . ~/.zshrc
```

Check with `hexdb --help`.

### 4. Create your encryption key

HexDB encrypts everything it stores, and every developer creates their own key. It goes in `hexdb_api/hexdb.local.toml`, which git ignores. Keep the key: data written with it can't be read without it.

Windows (PowerShell):

```powershell
$key = hexdb secret
Set-Content -Path hexdb_api\hexdb.local.toml -Encoding ascii -Value "[storage]`nencryption_key = `"$key`""
```

Linux and macOS:

```bash
printf '[storage]\nencryption_key = "%s"\n' "$(hexdb secret)" > hexdb_api/hexdb.local.toml
```

### 5. Start the server and sign in

```bash
cd hexdb_api
hexdb start
```

`hexdb` reads `hexdb.toml` from the current folder, which is why this step starts in `hexdb_api`. From anywhere else, run `hexdb --config <path to hexdb.toml> start`.

The server runs in this terminal; Ctrl+C stops it gracefully. On the first start it creates the administrator `hexdbadmin` and prints a generated password. The password is also saved to `hexdb_api/.hexdb/initial-admin-password.txt`, readable only by you.

Open http://localhost:7700/ui/ and sign in. Then change the password on the Account page and delete that file.

To run the server in the background instead, use `hexdb start -s` (output goes to `hexdb_api/.hexdb/hexdb.log`), and stop it with `hexdb stop` from the same folder. Data lives in `hexdb_api/.hexdb`. Delete that folder to start over; the key in `hexdb.local.toml` can stay.

### First requests

Create an API key on the Account page, then:

```bash
export HEXDB_TOKEN=hxk_...                 # PowerShell: $env:HEXDB_TOKEN = "hxk_..."
curl -X POST http://localhost:7700/orders -H "Authorization: Bearer $HEXDB_TOKEN" \
  -H "Content-Type: application/json" -d '{ "customer": "ada", "total": 42, "status": "paid" }'

curl -G http://localhost:7700/orders -H "Authorization: Bearer $HEXDB_TOKEN" \
  --data-urlencode 'filter={"status":"paid","total":{"$gte":10}}' --data-urlencode 'sort=-total'
```

In PowerShell, call `curl.exe` (plain `curl` is an alias for `Invoke-WebRequest`). On Windows PowerShell 5.1, write the JSON with escaped quotes, for example `-d '{\"customer\": \"ada\"}'`.

To load sample data (articles, products, customers, orders and short-lived sessions), run `node scripts/seed.mjs` with `HEXDB_TOKEN` set.

### While developing

- `cargo run -p hexdb_api` from the repository root runs the server straight from source, using `hexdb_api/hexdb.toml` (set by [.cargo/config.toml](.cargo/config.toml)). Stop any `hexdb start` server first: both use port 7700.
- `cd hexdb_admin && npm run dev` serves the admin UI with live reload at http://localhost:5173/ui/, sending API calls to the server on port 7700.
- `node scripts/lattice-demo.mjs` starts a lattice; see [See a lattice](#see-a-lattice) below.
- `hexdb lattice spawn --count 2` (from `hexdb_api`) adds two hexes to your own server's lattice instead.

### See a lattice

To see replication and failover in the UI, run the demo script from the repository root after step 2 (it needs the release build and the built UI):

```bash
node scripts/lattice-demo.mjs
```

It starts three hexes on ports 7800, 7810 and 7820, separate from your own server on 7700, and loads sample data into the first. Then it prints a UI link for each hex and an admin password. Sign in on any of them:
- The Dashboard's Lattice card lists all three hexes, with one Overseer and two Harvesters, and their replication lag.
- Each hex's vertex hexagon shows its six memory vertices.
- The Documents page on any hex shows the replicated data.

The script takes commands while it runs:
- `stop 1` stops the Overseer; within seconds another hex takes over, and the lost one is marked as such.
- `start 1` brings it back as a replica.
- `list` shows the roles; `quit` (or Ctrl+C) stops every hex.

`--hexes 5` starts up to seven hexes and `--port 9000` moves them. The data lives in `.hexdb-demo/`, which git ignores; `--keep` reuses it on the next run.

### With Docker

No Rust or Node.js needed:

```bash
docker build -t hexdb .
docker run -p 7700:7700 -v hexdb-data:/var/lib/hexdb \
  -e HEXDB_STORAGE__ENCRYPTION_KEY="base64:$(openssl rand -base64 32)" hexdb
```

The first start prints a generated administrator password in the container log (`docker logs <container> | grep password`). Open http://localhost:7700/ui/ and sign in. Keep the encryption key: the data can't be read without it.

`docker compose up -d` starts a three-hex lattice instead; see the comments at the top of [docker-compose.yml](docker-compose.yml).

## Command-line interface

```text
hexdb start [-s]                 start the server (-s: in the background, logging to hexdb.log)
hexdb stop [--force]             stop it gracefully
hexdb health                     is it up?
hexdb status                     status and metrics (needs HEXDB_TOKEN)
hexdb secret                     print a new random key
hexdb backup [--name N] [--list] back up the running server (needs HEXDB_TOKEN)
hexdb sql "SELECT ..." [-p V]   run a SQL query and print a table (needs HEXDB_TOKEN)
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
| Tessellations | Create and delete tessellations; manage indexes (with analyzer choice and index suggestions) and schemas (with version history and rollback) |
| Documents | Browse with filters and sorting; create, edit and delete documents |
| Streams | Create streams, publish, read messages, and watch consumer groups and deliveries |
| Functions | Create and run functions; manage schedules and triggers |
| Users, Roles | Accounts, role grants, user attributes, custom roles with row filters and hidden fields, MFA resets |
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
| Entity Framework Core | [drivers/dotnet/HexDB.EntityFrameworkCore](drivers/dotnet/HexDB.EntityFrameworkCore) | .NET 8+, EF Core 8 |
| ODBC | [drivers/odbc](drivers/odbc) | Windows, Linux (unixODBC); 64-bit |

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
- SQL is read-only and covers single-tessellation `SELECT`s: no joins, subqueries or expressions over fields. The ODBC driver inherits those limits. It's tested with pyodbc (unixODBC and the Windows driver manager), not yet with desktop tools such as Excel or Power BI. There's no JDBC driver.
- The Entity Framework Core provider maps one entity type to one tessellation. It doesn't support relationships, owned types, inheritance or explicit transactions (see [MANUAL.md](MANUAL.md#entity-framework-core)).

## License

HexDB is licensed under the [Apache License 2.0](LICENSE).
