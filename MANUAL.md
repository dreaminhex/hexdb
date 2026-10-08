# HexDB Technical Manual

This manual explains what HexDB does, how to use each feature, and how the parts work inside. The [README](README.md) has the short version and the quick start.

Examples use `curl` against `http://localhost:7700` and leave out credentials. Add `-H "Authorization: Bearer <API key>"` to each one.

## Contents

1. [Overview](#1-overview)
2. [Terminology](#2-terminology)
3. [Running HexDB](#3-running-hexdb)
4. [The admin UI](#4-the-admin-ui)
5. [Documents](#5-documents)
6. [Querying](#6-querying)
7. [Full-text search and analyzers](#7-full-text-search-and-analyzers)
8. [GraphQL](#8-graphql)
9. [Aggregations](#9-aggregations)
10. [Transactions](#10-transactions)
11. [Indexes and the query advisor](#11-indexes-and-the-query-advisor)
12. [Schemas](#12-schemas)
13. [Change data](#13-change-data)
14. [Streams](#14-streams)
15. [Functions, schedules and triggers](#15-functions-schedules-and-triggers)
16. [Plugins](#16-plugins)
17. [Security](#17-security)
18. [Lattices and replication](#18-lattices-and-replication)
19. [Storage internals](#19-storage-internals)
20. [Limits and configuration reference](#20-limits-and-configuration-reference)
21. [Operations](#21-operations)
22. [Drivers](#22-drivers)
23. [FAQ](#23-faq)

---

## 1. Overview

HexDB stores JSON documents in named collections (tessellations) and serves them over HTTP: a REST API, a GraphQL endpoint, and an admin UI, all from one server binary (`hexdb_api`). A command-line tool (`hexdb`) starts, stops and inspects servers.

What it provides:

| Area | Features |
| --- | --- |
| Data | Documents with generated, time-sortable IDs; replace, merge-patch, delete; bulk writes; upserts; update-by-filter; per-document TTLs; idempotency keys; optional versioned schemas |
| Reading | A JSON filter language, sorting, cursor and offset paging, counts, aggregations, full-text search with analyzers, GraphQL |
| Consistency | Every write is atomic and durable before it's acknowledged; multi-document transactions are serializable for the documents they touch |
| Performance | Field, composite, unique and text indexes; an index advisor; an LRU memory cache; SSTables with Bloom filters; Zstandard compression |
| Change data | An ordered change feed with an on-disk history; streams (publish/subscribe) with consumer groups; plugins for Kafka, Kinesis, OpenTelemetry and more |
| Logic | Functions (saved queries, aggregations, transactions, scripts in Python, TypeScript or JavaScript), schedules, and triggers that run code before or after writes |
| Distribution | Several servers form a lattice with automatic discovery, election, replication and failover. Synchronous acknowledgements, a quorum, and write forwarding are optional |
| Security | Sessions, API keys, TOTP MFA, built-in and custom roles with row filters and field masks, an audit trail, AES-256-GCM encryption at rest, TLS, authenticated hex-to-hex traffic |

What it doesn't do (yet):

- Partition data across hexes. Every hex holds a full copy.
- Speak SQL. Use filters, aggregations or GraphQL.

## 2. Terminology

**Hex.** One running HexDB server.

**Vertex.** One of the six regions of a hex's memory. Every document held in memory is split into six shards with Reed-Solomon coding (four data, two parity), one shard per vertex. Any two vertices can be lost or corrupted and the document is still rebuilt. See [Memory and vertices](#memory-and-vertices).

**Tessellation.** A collection of documents, like a table or a MongoDB collection. Roles are granted per tessellation. Names are 1-64 letters, digits, `_` or `-`, unique ignoring case. They can't start with `_` (reserved for system tessellations) or match an API route name (`users`, `roles`, `streams`, `functions`, `settings`, ...).

**Document.** A JSON object with an `id`. HexDB generates IDs as ULIDs: 26-character strings that sort by creation time.

**Lattice.** A group of hexes with the same `network.lattice_name` and lattice secret that can reach each other. A single hex is a lattice of one.

**Hex roles:**

| Role | What it does |
| --- | --- |
| Overseer | The one hex that takes writes. It publishes the change feed and runs plugins, stream sources and destinations, and schedules |
| Harvester | A read replica that follows the Overseer and can be elected Overseer if it fails |
| Replicant | A standby copy that follows the Overseer but is never elected; for backups and rebuilding Harvesters |

**System tessellation.** A tessellation HexDB manages itself, such as `users`, `roles`, `_audit`, `_streams` or `_functions`. The generic document routes refuse them. They're stored, encrypted and replicated like user data.

## 3. Running HexDB

### Configuration files

A hex reads its configuration from:

1. `hexdb.toml`, found through `--config <path>`, then the `HEXDB_CONFIG` variable, then `./hexdb.toml`, then `hexdb.toml` next to the executable. Without one, built-in defaults apply.
2. `hexdb.local.toml` next to it, if present. Values here override `hexdb.toml`. This is where secrets go; the repository ignores the file.
3. Environment variables named `HEXDB_<SECTION>__<FIELD>` (two underscores), for example `HEXDB_NETWORK__API_ENDPOINT=0.0.0.0:7700`. List settings (`peers`, `trusted_proxies`, `previous_lattice_secrets`, `previous_encryption_keys`) take comma-separated values.
4. Runtime settings saved from the admin UI (`settings.hxe` in the data directory). These apply on top of everything else; see [Runtime settings](#runtime-settings).

Relative paths in the file (`storage.path`, `ui.path`, `plugins.registry`) resolve against the config file's folder.

The one required setting is `storage.encryption_key`: 32 random bytes, base64-encoded, with a `base64:` prefix. Generate one with `hexdb secret` or `echo "base64:$(openssl rand -base64 32)"`. Without the key, the data can't be read. Back it up separately from the data.

```toml
# hexdb.local.toml
[storage]
encryption_key = "base64:..."

[network]
lattice_secret = "base64:..."   # optional; see section 18
```

### Starting and stopping

```bash
hexdb start            # in the foreground
hexdb start -s         # in the background; output goes to hexdb.log in the data directory
hexdb stop             # graceful: drains the WAL and flushes to SSTables
hexdb stop --force     # kill a server that won't stop (unflushed writes come back from the WAL)
hexdb health
HEXDB_TOKEN=hxk_... hexdb status
HEXDB_TOKEN=hxk_... hexdb backup        # a consistent backup while running; --list shows them
```

`hexdb start` runs the `hexdb_api` binary found next to the CLI (or on `PATH`, or `--server-bin`). The server writes `hexdb.pid` to its data directory: its PID, endpoint, executable, start time and a one-time shutdown token. The file is readable by its owner only. `hexdb stop` posts the token to `POST /shutdown`. `--force` kills the process only if its executable and start time match the file, so a stale file can't kill an unrelated process that reused the PID.

### The first administrator

With no users, the first start creates the administrator named by `security.admin_login` (default `hexdbadmin`). If `security.admin_password` is empty (recommended), HexDB generates a password, prints it once on the console, and writes it to `initial-admin-password.txt` in the data directory, readable by the owner only. It's never written to the log. Sign in, change the password on the Account page, and delete the file.

### Docker

The [Dockerfile](Dockerfile) builds an image with the server, the CLI, the admin UI, and Python and Node.js for script functions. It runs as the unprivileged user `hexdb`. Configuration lives in `/etc/hexdb/hexdb.toml` ([docker/hexdb.toml](docker/hexdb.toml)) and data in the volume `/var/lib/hexdb`.

```bash
docker run -p 7700:7700 -v hexdb-data:/var/lib/hexdb \
  -e HEXDB_STORAGE__ENCRYPTION_KEY="base64:..." hexdb
```

[docker-compose.yml](docker-compose.yml) runs a three-hex lattice (two Harvester candidates and a Replicant) on ports 7700, 7710 and 7720.

### As a service

[packaging/systemd/hexdb.service](packaging/systemd/hexdb.service) runs HexDB under systemd. The Debian package (built by `cargo deb -p hexdb_api` in the release workflow) installs it and creates a `hexdb` user. [packaging/install.sh](packaging/install.sh) installs a release into `~/.local`. Homebrew, Chocolatey and winget manifests are in [packaging](packaging).

## 4. The admin UI

The admin UI is at `/ui/` on every hex (`/` redirects there). It's built from [hexdb_admin](hexdb_admin) into `hexdb_admin/dist`, which `ui.path` points at. Everything it does goes through the same API as any client, with the signed-in user's permissions. Pages and buttons a user can't use are hidden, and the server refuses them anyway.

**Dashboard** (needs `status`)
- Cards: documents, tessellations, memory and disk against their budgets, operation rates, uptime.
- An activity chart: documents per tessellation, operations per minute, or storage, over 15 minutes to 6 hours.
- Vertex health: a hexagon of the six shards.
- The lattice card: each hex's role, address, replication state and lag. Administrators get "Add a hex", which shows the settings a new server needs to join; see [Adding a hex](#adding-a-hex).
- A per-tessellation table. On a replica, a banner names the Overseer. For users with the `maintenance` permission, "Flush to disk" writes unflushed data to SSTables and "Back up" writes a backup (see [Operations](#21-operations)).

**Queries.** A GraphQL console with schema-aware completion and validation, variables, examples, history, and JSON or table results. Ctrl+Enter (Cmd+Enter on macOS) runs the query.

**Tessellations**
- Create and delete tessellations and see their sizes.
- The key button opens the indexes dialog. It lists indexes, creates field or text indexes (choosing an analyzer for text), and has a Suggestions section. "From recent queries" runs the rule-based advisor; "Ask Claude" adds the AI's suggestions. Each suggestion has a button to apply it.
- The document button opens the schema dialog. It shows the version history and any migration in progress, and edits the next version. Check reports compatibility and how many existing documents wouldn't fit (running migration functions on them); Save registers the version. "Roll back to" restores an earlier version's fields.

**Documents.** Pick a tessellation, filter with JSON, sort and page. A side panel creates, edits (with an optional TTL) and deletes documents. The footer shows which index answered the query.

**Streams**
- Lists the streams you can read. Administrators and stream managers create and edit them: retention, sources and destinations as JSON.
- A stream's panel shows delivery status for each source and destination, consumer groups with their pending counts, a publish box, and the messages.

**Functions**
- Functions: create and edit with a template for each kind, run with parameters, see the result and timing.
- Schedules (administrators): interval or cron, last and next run, last status, run now.
- Triggers (administrators): the tessellation, events and timing of each trigger, its runs, failures and refusals, and create, edit, disable and delete.

**Users.** Create, edit, lock, reset passwords, reset MFA, grant roles with the tessellations (or `stream:<name>`) they apply to, and set attributes that role filters refer to.

**Roles.** The built-in roles and who holds them. Create, edit and delete custom roles by picking permissions and, optionally, restrictions (row filters and hidden fields) as JSON.

**Audit Trail** (needs `audit`). Security events, newest first, filtered by user, action area, target, outcome and time.

**Plugins** (needs `plugins`). Each plugin's type, runtime, state, deliveries, restarts and last error.

**Logs** (needs `logs`). The server's recent log, tailed live, with level, module and text filters.

**Settings** (administrators)
- Runtime settings grouped by area, each marked "live" (applies at once) or "restart".
- Saving shows which changes are waiting for a restart. "Use the config file's value" removes an override.
- The effective configuration, with secrets hidden.

**Account.** Change your password, set up or turn off MFA, regenerate backup codes, and create or revoke API keys.

The header's sun/moon button switches between light, dark and system themes.

## 5. Documents

Documents are JSON objects. Responses add `id`, `_expires_at` when a TTL is set, and `_schema` when the tessellation has a schema. All three are ignored if sent in a body. A write to a tessellation that doesn't exist creates it.

### Errors

Errors have one shape:

```json
{ "error": { "code": "not_found", "message": "Document 01J... not found in 'articles'." } }
```

| Status | Code | Meaning |
| --- | --- | --- |
| 400 | `invalid_request` | Malformed JSON, bad parameters or invalid values |
| 401 | `unauthorized` | Missing, expired, revoked or invalid credentials |
| 401 | `mfa_required` | Sign-in needs an MFA code (or the code was wrong) |
| 403 | `forbidden` | Your roles don't allow it, or a system tessellation via document routes |
| 403 | `cross_origin` | A cookie-authenticated change from another site (CSRF protection) |
| 404 | `not_found` | Unknown document, tessellation, user, role, stream or function |
| 409 | `conflict` | Already exists, a failed precondition or unique index, or an idempotent request in progress |
| 410 | `history_expired` | A change feed position older than the kept history |
| 413 | `document_too_large` | A document over `limits.max_document_kb` |
| 413 | `too_large` | A request body over its limit |
| 421 | `read_only_replica` | A write sent to a replica with forwarding off |
| 422 | `idempotency_key_reused` | An `Idempotency-Key` reused with a different request |
| 422 | `schema_violation` | A document that doesn't fit the tessellation's schema |
| 422 | `trigger_rejected` | A before trigger refused the write, or failed |
| 429 | `rate_limited` | Too many failed sign-ins; see `Retry-After` |
| 502 | `overseer_unreachable` | A replica couldn't forward a write to the Overseer |
| 503 | `replication_timeout` | Committed, but fewer than `replication.min_acks` replicas confirmed in time |
| 503 | `no_quorum` | The Overseer can't see `replication.quorum` hexes, so it refuses writes |
| 507 | `disk_full` | The data directory is over `storage.disk_mb`; deletes still work |

### Single documents

```bash
# Insert: 201 with the document and a Location header. ?ttl=<seconds> sets an expiry.
curl -X POST http://localhost:7700/articles -H "Content-Type: application/json" \
  -d '{ "title": "Quantum Tessellation", "tags": ["hexdb", "rust"], "published": true, "views": 445 }'

curl http://localhost:7700/articles/01JTY87RVJ9B5863KMB2YD896B             # the ETag header holds its version
curl -X PUT   http://localhost:7700/articles/01JTY... -H "Content-Type: application/json" -d '{ "title": "New" }'
curl -X PATCH http://localhost:7700/articles/01JTY... -H "Content-Type: application/json" -d '{ "views": 446, "tags": null }'
curl -X DELETE http://localhost:7700/articles/01JTY...                      # 204
```

PUT replaces the whole document. PATCH is a JSON merge patch at the top level: fields are set, and a `null` field is removed. PUT and PATCH also accept `?ttl=`.

### Bulk writes, updates and upserts

Each of these is one atomic write: every document is written, or none is. Up to 10,000 documents and `limits.max_request_mb` (32 MB) per request.

```bash
# Insert many: a JSON array, or {"documents": [...]}
curl -X POST http://localhost:7700/articles/_bulk -H "Content-Type: application/json" -d '[{ "title": "One" }, { "title": "Two" }]'

# Replace (PUT) or patch (PATCH) many by id. A missing id fails the whole request with 404.
curl -X PATCH http://localhost:7700/articles/_bulk -H "Content-Type: application/json" \
  -d '[{ "id": "01J...", "published": true }, { "id": "01J...", "published": true }]'

# Patch every document matching a filter ({} matches all)
curl -X POST http://localhost:7700/articles/_update -H "Content-Type: application/json" \
  -d '{ "filter": { "status": "draft" }, "update": { "status": "published" } }'
# => { "matched": 10, "modified": 10 }

# Insert or replace by key fields: a document whose key fields equal an existing one's replaces it
curl -X POST http://localhost:7700/customers/_upsert -H "Content-Type: application/json" \
  -d '{ "key": ["source_id"], "documents": [{ "source_id": 17, "name": "Ada" }] }'
# => { "inserted": 1, "replaced": 0, "ids": ["01J..."] }
```

Upserts on one hex run one at a time, so two concurrent upserts with the same key can't both insert. A unique index on the key fields adds a guarantee that holds for every kind of write.

### Idempotency keys

Every REST write accepts an `Idempotency-Key` header (1-255 printable ASCII characters). Every GraphQL document mutation accepts an `idempotencyKey` argument.

- The result is stored with the write, in the same atomic WAL record, for 24 hours.
- A retry with the same key and the same request returns the original response with `Idempotent-Replayed: true` instead of writing again, even after a crash.
- Reusing a key for a different request returns 422. A retry while the first request is still running returns 409.
- Keys are scoped to the user, so one user can't replay or block another's.

Implementation: the record lives in the `_idempotency` system tessellation, keyed by user and key. It holds a hash of the method, path and body, plus the response. Writing it in the same batch as the data is what makes "written, but the response was lost" impossible to observe.

### TTLs

`?ttl=<seconds>` on POST, PUT or PATCH sets `_expires_at`. Expired documents read as not found immediately. A sweep (`memory.ttl_scan_frequency`) evicts them from memory, and compaction drops them from disk.

## 6. Querying

### Filters

REST (listing, `_query`, `count`, `_update`), GraphQL, functions, stream sources and plugins share one filter language. A filter is a JSON object, and every condition in it must hold.

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
| a value, or `$eq` | Equal (`1` equals `1.0`; `null` also matches a missing field) |
| `$ne` | Not equal |
| `$gt`, `$gte`, `$lt`, `$lte` | Compare numbers, strings or booleans |
| `$in`, `$nin` | One of / none of an array of values |
| `$exists` | The field is present (`true`) or absent (`false`) |
| `$contains` | Substring of a string, or element of an array |
| `$startsWith`, `$endsWith` | String prefix or suffix |
| `$not` | Negates the operators inside it |
| `$and`, `$or`, `$not` (top level) | Combine filters |
| `$text` (top level) | Full-text search; see [section 7](#7-full-text-search-and-analyzers) |

Dotted paths reach into nested objects. A condition on an array field matches if any element matches. Every operator can also be written with `_` instead of `$` (`_gte`, `_or`). Use that form inside GraphQL query text, where `$` marks a variable.

### Listing, sorting and paging

```bash
curl "http://localhost:7700/articles?limit=50"
curl -G http://localhost:7700/articles --data-urlencode 'filter={"published":true}' \
  --data-urlencode 'sort=-views,title' -d limit=25 -d offset=25

# The same as a JSON body, for long filters
curl -X POST http://localhost:7700/articles/_query -H "Content-Type: application/json" \
  -d '{ "filter": { "tags": "rust" }, "sort": [{ "field": "views", "descending": true }], "limit": 25 }'

curl -G http://localhost:7700/articles/count --data-urlencode 'filter={"published":false}'
```

- Pages hold up to 1,000 documents (default 100). Responses include `total`, the number of matches across all pages.
- `total=false` (query string) or `"total": false` (body) skips counting every match: `total` is `null`, and a filtered or sorted query stops reading once its page is full. Use it for large tessellations when you only need the page.
- Without `sort`, results come in ID order (creation order), and `next` goes in `after` for the next page. This cursor paging is stable under concurrent writes.
- With `sort`, page with `offset`. `sort` is `"-views,title"` (a leading `-` means descending) or an array of `{field, descending}`.
- Responses include `plan`: `{"indexes": [...], "scanned": n}` shows which indexes were used and how many documents were examined.

### How a query runs

1. The planner looks for indexes that can narrow the candidates: equality, `$in`, ranges and `$startsWith` on field indexes, and `$text` on the text index. It intersects (for `$and`) or unions (for `$or`) their ID sets. With no usable index, every document in the tessellation is a candidate.
2. Each candidate is read (from memory, or from disk through the SSTable index) and the full filter is checked. Indexes only ever produce a superset, so results are identical with or without them.
3. Matches are sorted, then paged.

Sorting uses an index when the first sort key has a single-field index. HexDB walks that index in order, reading only planner candidates, and stops once the page is full; documents tied on the first key are ordered by the remaining keys. That applies to unfiltered queries, and to filtered ones run with `total=false`. A filtered query that needs `total` reads every match anyway, so it sorts in memory. Unfiltered pages without a sort read only the page itself, and their `total` comes from the maintained count.

The advisor records each query's shape for index suggestions; see [section 11](#11-indexes-and-the-query-advisor).

## 7. Full-text search and analyzers

`$text` takes a string. A document matches when every word of the query appears in it.

```bash
curl -X POST http://localhost:7700/articles/_query -H "Content-Type: application/json" \
  -d '{ "filter": { "$text": "replicated storage", "published": true } }'
```

With a text index, `$text` searches the indexed fields, and both the documents and the query go through the index's analyzer. Without one, it searches every string field with the standard analyzer (slower: every document is scanned). A tessellation has at most one text index, over one or more fields:

```bash
curl -X POST http://localhost:7700/tessellations/articles/indexes -H "Content-Type: application/json" \
  -d '{ "fields": ["title", "body"], "kind": "text", "analyzer": "english" }'
```

### Analyzers

An analyzer is a tokenizer followed by filters.

| Analyzer | Pipeline | Use it for |
| --- | --- | --- |
| `standard` (default) | letters and digits, lowercased | General text |
| `simple` | standard plus accent folding (café = cafe) | Text with accents |
| `whitespace` | split on spaces, lowercased | Codes like `a-1/b` |
| `keyword` | the whole value as one token, lowercased | Exact matching of short values |
| `english` | simple, minus common English words, stemmed | Prose: "running", "runs" and "run" all match |
| `ngram` | 3-letter pieces of each word | Matching parts of words |
| `autocomplete` | word prefixes of 2 to 15 letters | Search as you type |

Define more in `hexdb.toml` without code:

```toml
[analyzers.product_code]
description = "SKUs: the whole code and its prefixes"
tokenizer = "whitespace"                  # standard, whitespace or keyword
filters = ["lowercase", "edge_ngram:3:12"]
```

Filters: `lowercase`, `ascii_folding`, `stopwords`, `stem`, `ngram:MIN:MAX`, `edge_ngram:MIN:MAX`, `min_length:N`, `max_length:N`. Every hex in a lattice needs the same custom analyzers, because indexes replicate as definitions and each hex builds its own.

`GET /analyzers` lists analyzers. To try one:

```bash
curl -X POST http://localhost:7700/analyzers/_analyze -H "Content-Type: application/json" \
  -d '{ "analyzer": "english", "text": "Running the replicas" }'
# => { "index_tokens": ["run", "replica"], "query_tokens": [...], "pipeline": "..." }
```

The text index is an inverted index: each token maps to the set of document IDs that contain it. The index fingerprints its analyzer's definition, so changing a custom analyzer rebuilds the index at the next start instead of mixing old and new tokens.

## 8. GraphQL

`POST /graphql` takes standard requests (`{"query", "variables", "operationName"}`) with the same credentials and permissions as REST. The admin UI's Queries page is the console.

```graphql
query Recent($filter: JSON) {
  documents(tessellation: "articles", filter: $filter, sort: [{ field: "views", descending: true }], limit: 25) {
    total
    next
    documents { id expiresAt data field(path: "author.name") }
  }
  count(tessellation: "articles", filter: { published: { _eq: false } })
}
```

| Queries | Mutations |
| --- | --- |
| `tessellations`, `tessellation(name)` with `documentCount`, `indexes` and `documents` | `insertDocument`, `insertDocuments` (atomic) |
| `document(tessellation, id)` | `replaceDocument`, `patchDocument`, `deleteDocument` |
| `documents(tessellation, filter, sort, limit, offset, after)` | `updateDocuments(tessellation, filter, update)` (atomic) |
| `count(tessellation, filter)` | `createTessellation`, `deleteTessellation` |
| `aggregate(tessellation, filter, groupBy, aggregates, sort, limit, offset)` | `transaction(operations, idempotencyKey)` |
| `changes(after, tessellation, limit)` | `createIndex`, `dropIndex` |
| `users`, `user(idOrLogin)`, `roles`, `status` | |

- Document fields are exposed through the `JSON` scalar: `data`, `json`, or `field(path)`.
- Document mutations take an optional `idempotencyKey`. Replays are listed in `extensions.idempotentReplays`, and the HTTP response carries `Idempotent-Replayed: true`.
- Errors carry `extensions.code` (`UNAUTHENTICATED`, `FORBIDDEN`, `NOT_FOUND`, `INVALID_REQUEST`, `CONFLICT`, ...).
- `total` is counted only when the query selects it, so leave it out to make sorted and filtered pages cheaper.
- Requests are limited to depth 16 and complexity 10,000.
- Streams, functions, schemas and settings are REST-only.

## 9. Aggregations

An aggregation groups the documents that match a filter and summarizes each group.

```bash
curl -X POST http://localhost:7700/orders/_aggregate -H "Content-Type: application/json" -d '{
  "filter": { "status": { "$ne": "cancelled" } },
  "group_by": ["customer.country"],
  "aggregates": {
    "orders":  { "$count": "*" },
    "buyers":  { "$countDistinct": "customer.id" },
    "revenue": { "$sum": "total" },
    "average": { "$avg": "total" },
    "largest": { "$max": "total" }
  },
  "sort": "-revenue",
  "limit": 10 }'
# => { "rows": [{ "customer.country": "US", "orders": 61, "buyers": 40, "revenue": 171234.5, ... }, ...],
#      "total_groups": 10, "matched": 351 }
```

| Operator | Result |
| --- | --- |
| `$count` | `"*"` counts documents; a field name counts documents where it isn't null |
| `$countDistinct` | Distinct values of a field |
| `$sum`, `$avg` | Over numeric values; others are ignored |
| `$min`, `$max` | Over numbers, strings or dates |

Each row holds the group-by fields and one column per aggregate. Without `group_by` there's a single row. `sort`, `limit` and `offset` apply to the groups. An aggregate over an array field uses each element (`$sum` adds them; `$countDistinct` counts distinct elements). An aggregation can produce up to 100,000 groups before `limit` and `offset` apply.

The filter uses indexes like a query. Grouping then runs in memory over the matches.

GraphQL: `aggregate(tessellation: "orders", groupBy: ["status"], aggregates: { n: { _count: "*" } }) { rows totalGroups matched }`.

## 10. Transactions

`POST /transactions` runs up to 1,000 operations across any user tessellations as one atomic write.

```bash
curl -X POST http://localhost:7700/transactions -H "Content-Type: application/json" -d '{ "operations": [
  { "op": "patch", "tessellation": "accounts", "id": "01J...A", "data": { "balance": 70 }, "if_match": { "balance": { "$gte": 30 } } },
  { "op": "patch", "tessellation": "accounts", "id": "01J...B", "data": { "balance": 35 } },
  { "op": "insert", "tessellation": "ledger", "data": { "from": "01J...A", "to": "01J...B", "amount": 30 } } ] }'
# => { "results": [{ "op": "patch", "id": "...", "version": 812, "document": {...} }, ...], "writes": 3 }
```

**Operations.** `get`, `check` (a precondition only), `insert`, `replace`, `patch` and `delete`. Later operations see the effects of earlier ones.

**Preconditions,** on any operation with an `id`:
- `if_version`: the document's version must equal this. `0` means it must not exist. Versions come back in every result and in the `ETag` header of `GET /{tess}/{id}`.
- `if_match`: the document must exist and match this filter.

**Outcomes.** A failed precondition returns 409 and a missing document 404. In both cases nothing is written. `Idempotency-Key` works as for other writes. A transaction needs read or write permission on each tessellation it touches. GraphQL: `transaction(operations: JSON!, idempotencyKey)`.

### How transactions work

HexDB uses optimistic concurrency control.

1. The operations run against a private view of the data, recording the version of every document they read and the writes they'd make.
2. At commit, under the engine's state lock, HexDB checks that none of the documents read has changed since. If one has, the transaction is planned again from step 1, up to a retry limit, after which it returns 409.
3. The writes get consecutive sequence numbers and go into the WAL as a single record. Recovery therefore replays all of a transaction or none of it.

Validating every read at commit makes transactions serializable for the documents they touch. Two concurrent read-modify-write transactions on the same document can't both succeed with stale data. Every other write path (replace, patch, delete, `_update`) uses the same mechanism.

## 11. Indexes and the query advisor

### Indexes

Indexes speed up filtered queries, counts, aggregations and `_update`. They never change results.

| Kind | Covers | Answers |
| --- | --- | --- |
| Field | One field or several (composite), optionally `unique` | Equality, `$in`, ranges and `$startsWith` on the first field; equality on all fields together. Array values index each element |
| Text | One or more string fields, with an analyzer; at most one per tessellation | `$text` |

```bash
curl -X POST http://localhost:7700/tessellations/orders/indexes -H "Content-Type: application/json" -d '{ "fields": ["status"] }'
curl -X POST http://localhost:7700/tessellations/orders/indexes -H "Content-Type: application/json" -d '{ "fields": ["customer.country", "status"] }'
curl -X POST http://localhost:7700/tessellations/customers/indexes -H "Content-Type: application/json" -d '{ "fields": ["email"], "unique": true }'
curl http://localhost:7700/tessellations/orders/indexes
curl -X DELETE http://localhost:7700/tessellations/orders/indexes/status
```

Creating an index builds it from every document before the request returns. A write that would duplicate a unique key fails with 409. The `id` (primary key) needs no index.

**How they work.**
- A field index is an ordered map from encoded values to sets of document IDs. The encoding sorts numbers, strings and booleans correctly, which is what makes range scans possible.
- Writes update indexes inside the commit, under the same lock, so a query always sees its own writes.
- Definitions are stored in the encrypted catalog. Contents live in memory.
- After each flush and at shutdown, each index is saved as an encrypted snapshot (`indexes/<tess>/<name>.hxi`) with the sequence number it reflects. At startup the snapshot is loaded and brought up to date from SSTables written after it. An index without a usable snapshot is rebuilt from every document.
- Indexes replicate as definitions; each hex maintains its own contents.

### The query advisor

HexDB keeps a summary of the queries each tessellation receives. For each query shape (which fields are tested for equality or ranges, sorted on, or searched as text) it records how often the shape runs, how many documents it reads, how many it returns, and which indexes it used.

```bash
curl http://localhost:7700/tessellations/orders/advice
curl "http://localhost:7700/tessellations/orders/advice?ai=true"
```

The rule-based suggestions are:

- An index for a frequent query that reads many more documents than it returns. Equality fields come first, then one range field, in the order the planner uses them.
- An index for a frequent query that sorts without one.
- A text index for `$text` searches that run without one.
- Dropping indexes no query has used. Unused indexes still cost write time.

Small tessellations get no index suggestions, because scanning them is already cheap.

With `?ai=true` and an Anthropic API key configured, HexDB also asks Claude for a second opinion and returns its suggestions separately (`source: "ai"`). It sends:
- the query shapes;
- the existing indexes;
- the field names and types of a sample of documents.

It never sends field values.

```toml
[ai]
api_key_env = "ANTHROPIC_API_KEY"   # the environment variable holding the key
model = "claude-sonnet-5-5"
```

Query statistics are kept per hex and saved with the metrics history (`query-stats.hxe`), so they survive restarts. Shapes not seen for 30 days are dropped. Needs `manage` on the tessellation.

## 12. Schemas

A tessellation is schemaless until a schema is registered. A schema is a numbered version: the fields documents may or must have, and, from version 2 on, the migration from the previous version.

```bash
curl -X POST http://localhost:7700/tessellations/orders/schemas -H "Content-Type: application/json" -d '{
  "fields": {
    "customer": { "type": "string", "required": true },
    "total":    { "type": "number", "required": true, "min": 0 },
    "status":   { "type": "string", "default": "new", "enum": ["new", "paid", "shipped"] }
  },
  "additional_fields": true
}'
```

**Field rules:**
- `type`: `string`, `number`, `integer`, `boolean`, `object`, `array` or `any`.
- `required`, `nullable`, `default` and `enum`.
- `min` and `max` (numbers), `min_length` and `max_length` (strings and arrays).
- `description`.

Field names are top-level names or dotted paths. With `"additional_fields": false`, documents can't have other fields.

**Writes.**
- Each write is validated against the current version. A write that doesn't fit fails with 422 `schema_violation`, listing every problem.
- Missing fields with a `default` are filled in.
- Each document records the version it was written with in `_schema`.

**New versions** carry a migration, a list of steps:

```json
{
  "fields": { "...": "..." },
  "migration": [
    { "rename": { "from": "amount", "to": "total" } },
    { "copy": { "from": "customer", "to": "billing_name" } },
    { "remove": "legacy_flag" },
    { "set_default": { "field": "currency", "value": "USD" } },
    { "convert": { "field": "zip", "to": "string" } },
    { "function": { "name": "split_name", "undo": "join_name" } }
  ]
}
```

**Function steps** run a function over the documents, for changes the built-in steps can't express:
- It runs once per batch (up to 200 documents in the background migration, one when a document is upgraded on write).
- It gets `documents` (the documents as they are at that step) and returns them migrated, in order: `{"documents": [...]}` or the array itself.
- A script reads them as `migration.documents` on stdin, along with `tessellation`, `from_version` and `to_version`. Other kinds receive them through a `documents` parameter.
- It runs as the user who registered the version.
- `undo` names the function that reverses it, which a rollback needs.

```python
# split_name: {"name": "Ada Lovelace"} -> {"first": "Ada", "last": "Lovelace"}
import json, sys
docs = json.load(sys.stdin)["migration"]["documents"]
for d in docs:
    d["first"], _, d["last"] = d.pop("name").partition(" ")
print(json.dumps({"documents": docs}))
```

A function can change anything, so HexDB can't prove a version with a function step compatible up front. Run `POST .../schemas/check` first: it runs the migration, function included, on the existing documents and reports any that wouldn't fit. The background migration validates every document too, and reports those that don't fit.

**Compatibility.** Like a schema registry, HexDB requires each version to be compatible with the previous one: every document valid under the old version, once migrated, must be valid under the new one. Changing a field's type needs a `convert` step. Adding a required field needs a `default` or a `set_default` step. An incompatible version is refused with an explanation. `POST .../schemas/check` checks a candidate without registering it. It reports compatibility and how many existing documents wouldn't fit after migrating (checking up to 100,000), with examples.

**Migration.** After a version is registered, HexDB rewrites existing documents in the background, in batches. `GET /tessellations/{name}/schemas` shows all versions and the migration's progress (migrated, unchanged, failed, with examples of failures). A migration interrupted by a restart resumes. Until it finishes, a document not yet migrated is upgraded when it's written, and reads can return documents of either version (check `_schema`). Documents written without `_schema` count as the current version.

**Rollback.** `POST /tessellations/{name}/schemas/rollback` with `{"to": 2}` registers a new version that restores version 2's fields. Its migration is the inverse of every version after 2, newest first:

| Step | Undone by |
| --- | --- |
| `rename {from, to}` | `rename {to, from}` |
| `copy {from, to}` | `remove to` (unless version 2 has that field) |
| `convert {field, to}` | `convert` back to the field's type before that version |
| `function {name, undo}` | `function {undo, name}`; a function step without `undo` can't be rolled back automatically |
| `remove`, `set_default` | nothing: removed data is gone |

History stays linear: the rollback is a new version (`"restores": 2`), checked for compatibility like any other, and existing documents migrate to it in the background. When a removed field is required again, the rollback is refused with an explanation. Add steps that fill the gap: `{"to": 2, "migration": [{"set_default": {"field": "name", "value": "unknown"}}]}`. The extra steps run after the inverse ones.

`DELETE /tessellations/{name}/schemas` removes the schema, and the tessellation becomes schemaless. Schema changes need `manage` on the tessellation.

## 13. Change data

### The change feed

Every committed write to a user tessellation is published, in order, once it's durable:

```json
{ "seq": 831, "timestamp": 1760000000000, "op": "put", "tessellation": "orders", "id": "01J...", "document": { "...": "..." } }
```

`op` is `put`, `delete` or `drop_tessellation`. `event` says more: `insert` (the put created the document), `update`, `delete` or `drop_tessellation`. `seq` is the write's sequence number, which increases across the whole hex. A change made by a trigger has `trigger` (its name).

```bash
curl "http://localhost:7700/changes"                                 # current position: {"changes": [], "last_seq": 830, ...}
curl "http://localhost:7700/changes?after=830&wait=30"               # long poll (up to 60 s); pass last_seq back as after
curl "http://localhost:7700/changes?after=830&tessellation=orders&limit=100"
curl -N "http://localhost:7700/changes/stream?after=830"             # Server-Sent Events: event: change, id: <seq>
```

SSE clients resume after a reconnect from the `Last-Event-ID` header. A user sees only changes to tessellations they can read; system tessellations are left out. GraphQL: `changes(after, tessellation, limit) { changes lastSeq }`.

### Change history

The most recent 10,000 changes are kept in memory. Older ones come from the change history on disk. When the WAL retires a segment after a flush, the segment moves to `wal/archive/` instead of being deleted. The archive is never replayed into the database; it's read only to answer older positions. It is pruned to `storage.change_history_hours` (24) and `storage.change_history_mb` (512). Set `change_history_hours = 0` to keep none.

The response's `available_after` is the oldest position that can still be resumed. A position older than the history returns 410 `history_expired`. When that happens, re-read the data you need and continue from a fresh `last_seq`.

The change history serves:
- replicas catching up after being away;
- plugins resuming from their saved position;
- `/changes` clients that store their own cursor.

### Choosing a mechanism

| Need | Use |
| --- | --- |
| A script that reacts to changes and remembers where it was | `/changes` with a stored `last_seq`, or SSE |
| Several independent consumers, each with a server-side position | A stream with a tessellation source and consumer groups |
| Push to Kafka, Kinesis, a webhook, a SIEM | A plugin, or a stream destination for webhooks |

## 14. Streams

A stream is a named, ordered log of messages kept for `retention_hours`.
- Producers publish messages.
- Consumers read them in order by offset (the message ID, a ULID). A consumer can track its own offset, belong to a consumer group whose committed offset HexDB keeps, or subscribe live.

```bash
curl -X POST http://localhost:7700/streams -H "Content-Type: application/json" -d '{
  "name": "order-events",
  "description": "Paid orders",
  "retention_hours": 72,
  "sources": [{ "tessellation": "orders", "filter": { "status": "paid" }, "ops": ["put"] }],
  "destinations": [{ "kind": "webhook", "url": "https://example.com/hooks/orders", "headers": { "Authorization": "Bearer ..." }, "batch_size": 100 }]
}'

# Publish one message or an array; returns their offsets
curl -X POST http://localhost:7700/streams/order-events/messages -H "Content-Type: application/json" \
  -d '{ "payload": { "order": "01J..." }, "key": "customer-17", "headers": { "source": "checkout" } }'

# Read in order; long-poll with wait (seconds)
curl "http://localhost:7700/streams/order-events/messages?after=01J...&limit=100&wait=20"

# As a consumer group: read from the group's committed offset, then commit
curl "http://localhost:7700/streams/order-events/messages?group=billing&limit=100"
curl -X POST http://localhost:7700/streams/order-events/groups/billing/commit -H "Content-Type: application/json" -d '{ "offset": "01J..." }'

# Live, as Server-Sent Events
curl -N "http://localhost:7700/streams/order-events/subscribe?group=billing"
```

**Sources** turn a tessellation's committed changes into messages, optionally filtered and limited to some operations. They start at the stream's creation and keep their position across restarts.

**Destinations** POST batches of messages to a webhook. Each destination has its own offset and retries until the webhook answers 2xx. Delivery is at-least-once, so receivers should be idempotent (use the offset).

`GET /streams/{name}` shows the configuration, each source's and destination's deliveries and last error, and each consumer group's offset and pending count. `PUT` changes the configuration and `DELETE` removes the stream with its messages.

**Permissions** use the resource name `stream:<name>` in role grants:
- `read` to consume;
- `write` to publish;
- `manage` to configure.

Creating a stream needs `manage` on its name. A grant on `*` covers every tessellation and every stream.

**Storage.** Messages are documents in the system tessellation `_stream_<name>`, so they're encrypted, replicated, and expire by TTL. Configurations live in `_streams` and offsets in `_stream_offsets`. Sources and destinations run on the Overseer.

## 15. Functions, schedules and triggers

A function is saved on the server and run by name with parameters.

| Kind | Body |
| --- | --- |
| `query` | A filtered, sorted query on `tessellation` (the `_query` body) |
| `aggregate` | An aggregation on `tessellation` (the `_aggregate` body) |
| `transaction` | `{"operations": [...]}` across tessellations, atomic |
| `script` | `code` in Python, TypeScript or JavaScript, run in a separate process |

```bash
curl -X POST http://localhost:7700/functions -H "Content-Type: application/json" -d '{
  "name": "orders_by_status",
  "kind": "query",
  "tessellation": "orders",
  "description": "Recent orders with a status",
  "params": [{ "name": "status", "type": "string", "required": true },
             { "name": "limit", "type": "integer", "default": 50 }],
  "body": { "filter": { "status": { "$param": "status" } }, "sort": "-created", "limit": { "$param": "limit" } }
}'

curl -X POST http://localhost:7700/functions/orders_by_status/run -H "Content-Type: application/json" \
  -d '{ "params": { "status": "paid" } }'
# => { "result": { "documents": [...], "total": 12 }, "ms": 3 }
```

**Parameters** are declared with:
- `name`;
- `type`: `string`, `number`, `integer`, `boolean`, `object`, `array` or `any`;
- `required`, `default` and `description`.

The body references a parameter as `{"$param": "name"}`, anywhere in the JSON. HexDB checks the types, fills in defaults, and substitutes the values. Values are never interpolated into text, so there's nothing to inject.

**Permissions.** A function always runs with the permissions of whoever runs it: a query needs read access to its tessellation, a transaction whatever its operations need. Anyone signed in can run a function. Only administrators create, change or delete functions, because scripts run code on the server.

### Scripts

```json
{
  "name": "order_count",
  "kind": "script",
  "runtime": "python",
  "timeout_seconds": 30,
  "params": [{ "name": "tessellation", "type": "string", "default": "orders" }],
  "code": "import json, os, sys, urllib.request\nparams = json.load(sys.stdin)[\"params\"]\n..."
}
```

**Input.** A script reads `{"params", "function", "caller"}` as JSON on stdin and prints its result as JSON on stdout.

**Environment.** It gets `HEXDB_API` (the server's URL) and `HEXDB_TOKEN`, a session of the user who ran it. The session is revoked when the script ends. The script runs in a clean environment in a temporary folder.

**Timeout.** It's stopped after `timeout_seconds`. If the server exits, the script dies with it.

**Runtimes:**
- `python` runs the `python` (Windows) or `python3` interpreter.
- `javascript` runs `node`.
- `typescript` runs Node.js 22.6 or later with type stripping.

```toml
[functions]
scripts = true          # false refuses script functions entirely
python = "python3"
node = "node"
```

### Schedules

```bash
curl -X POST http://localhost:7700/schedules -H "Content-Type: application/json" -d '{
  "name": "nightly-report", "function": "order_count", "params": {}, "cron": "0 2 * * *", "enabled": true }'
# or "every_seconds": 3600 (at least 10)
curl -X POST http://localhost:7700/schedules/nightly-report/run      # run now
```

**Timing.**
- Cron expressions are in UTC: minute, hour, day of month, month, day of week. `@hourly`, `@daily`, `@weekly` and `@monthly` also work.
- A schedule runs the function as the administrator who created it.
- Schedules run on the Overseer only, so a lattice runs each one once.

**Status.** Each schedule records its next run, last run, last status and error, last duration, and run count. Functions and schedules are stored in `_functions` and `_schedules`.

### Triggers

A trigger runs a function when documents in a tessellation are inserted, updated or deleted. Administrators manage them at `/triggers` (and on the Functions page).

```bash
curl -X POST http://localhost:7700/triggers -H "Content-Type: application/json" -d '{
  "name": "check-orders", "tessellation": "orders", "events": ["insert", "update"],
  "timing": "before", "function": "check_order", "filter": { "status": { "$ne": "draft" } } }'
```

| Field | Meaning |
| --- | --- |
| `tessellation` | The tessellation it watches |
| `events` | Any of `insert`, `update`, `delete` (default: all) |
| `timing` | `before` or `after` (default) |
| `function` | The function to run |
| `filter` | Only documents matching it: the new version, or for a before trigger on a delete, the stored one. After triggers see deletes without a document, so a filtered after trigger doesn't fire on deletes |
| `enabled` | `false` pauses it |

**Before triggers** run inside the write, before it commits, and must be script functions. The script prints one of:
- `null` or `{}` to let the write through;
- `{"document": {...}}` to store this document instead (the schema is checked after the trigger);
- `{"reject": "why"}` to refuse the write with 422 `trigger_rejected`.

A script that fails, or times out, refuses the write too. Before triggers apply to every write path: single documents, bulk writes, update-by-filter, upserts and transactions. The response shows the document as stored. A write that conflicts with another is planned again, so a before trigger may run more than once for one write; it must not have side effects. It adds the script's run time to every matching write, so keep it fast.

```python
# check_order: refuse negative totals, stamp the rest
import json, sys
t = json.load(sys.stdin)["trigger"]
doc = t["document"]
if doc.get("total", 0) < 0:
    print(json.dumps({"reject": "totals can't be negative"}))
else:
    doc["checked_by"] = t["user"]
    print(json.dumps({"document": doc}))
```

**After triggers** run once the write is committed, on the Overseer, from the change feed. They can use any kind of function: a transaction that writes a log entry, a script that calls a webhook. The runner's position in the feed is saved (`_trigger_cursors`), so after a restart it carries on where it stopped. A change may run twice after a crash, so make after triggers idempotent. A failure is recorded in the trigger's status, and the runner moves on to the next change.

**What the function gets.** A script reads `trigger` on stdin:

| Field | Meaning |
| --- | --- |
| `event` | `insert`, `update` or `delete` |
| `tessellation`, `id` | The document written |
| `document` | The new version (none for deletes) |
| `previous` | The stored version, for updates and deletes (before triggers) |
| `user` | Who wrote (before triggers) |

Other kinds of function receive the values their declared parameters name; for example, a transaction function with `id` and `event` parameters.

**Rules:**
- Triggers run as the administrator who created or last changed them.
- Writes made while a trigger runs, directly or through its script's API session, don't fire triggers, so triggers can't cascade or loop.
- Internal writes (schema migrations, replication) don't fire triggers either.
- `GET /triggers` shows each trigger with its status on this hex: runs, failures, refused writes, last run and last error.

## 16. Plugins

Plugins extend HexDB without changing it. Each is a folder with a `plugin.toml` manifest, listed in the registry (`plugins.registry`, default `plugins.json` next to the config file):

```json
{
  "@streams/kafka": { "path": "./plugins/streams/kafka", "enabled": true },
  "@sinks/otlp":    { "path": "./plugins/sinks/otlp", "enabled": false }
}
```

`hexdb plugins add|remove|list` edits the registry. Changes take effect at the next start.

### What a plugin receives

| `type` | Receives |
| --- | --- |
| `stream` | Every committed change to user tessellations, in order, in the same JSON as `/changes`. `tessellations = [...]` narrows it; `audit = true` adds the audit trail |
| `logs` | Server log records. `[logs] level = "info"` and `targets = ["hexdb_core::network"]` filter them |
| `metrics` | A metrics sample every 15 seconds |
| `source` | Nothing; the plugin brings data in through the API |

### How it runs

```toml
command = ["node", "index.mjs"]   # a process in the plugin folder: one JSON payload per line on stdin

# or a webhook: payloads POSTed in batches as a JSON array
[webhook]
url = "https://example.com/hexdb"
headers = { Authorization = "Bearer ..." }
batch_size = 100

# or the built-in OpenTelemetry exporter, for logs or metrics
builtin = "otlp"
[otlp]
endpoint = "http://localhost:4318"
service_name = "hexdb"
```

**Process plugins.**
- A process's stdout and stderr go to the HexDB log, tagged with the plugin ID.
- It's restarted five seconds after it exits.
- Output lines over 64 KB are cut.
- When the server exits, its plugins exit too. On Linux this uses a parent-death signal; on Windows a job object.

**Webhooks** must be `http` or `https` URLs. Redirects aren't followed.

**The built-in `otlp` exporter** sends OTLP/HTTP JSON to `/v1/metrics` or `/v1/logs`. The OpenTelemetry Collector, Grafana Alloy, Datadog, Dynatrace, Honeycomb and New Relic accept it.

Plugins run on the Overseer only, so a lattice delivers each payload once. `GET /plugins` and the Plugins page show each plugin's state, deliveries, restarts and last error.

### API access

```toml
[access]
role = "writer"
tessellations = ["customers"]
```

With `[access]`, HexDB creates a user `plugin-<id>` holding exactly that role. It issues a fresh API key each time the plugin starts and revokes the older ones. The process gets the key as `HEXDB_API_KEY` and the server's URL as `HEXDB_API`. The key is never stored in plaintext. Source and enricher plugins use this to write through the API like any client.

### Delivery guarantees

Stream plugins get at-least-once delivery.
- Each plugin's position is saved in the `_plugin_cursors` system tessellation, about once a second and when it stops.
- A restarted plugin continues from there, reading older changes from the change history.
- A change may be delivered twice after a crash, so make consumers idempotent (use `seq`).
- A new plugin starts at the current end of the feed, and that starting point is saved at once, so a plugin that fails before its first delivery still gets everything from then on.
- After a failover, the new Overseer has a different history, so plugins start at its end. HexDB logs a warning when this happens.

Logs and metrics are delivered from the moment the plugin starts.

### Isolation

A process plugin runs in its own folder with a clean environment. It gets only:
- `PATH` and the basic variables a runtime needs;
- the manifest's `[env]`;
- the server variables named in `pass_env`;
- the `HEXDB_*` variables HexDB sets.

The server's own `HEXDB_*` variables (which may hold keys) are never passed. When the lattice uses a private CA, `NODE_EXTRA_CA_CERTS` and `REQUESTS_CA_BUNDLE` point at it.

On Unix, HexDB refuses a registry or manifest that other users can write. Plugins still run with the server's privileges: install only plugins you trust, and keep the registry writable by administrators only.

### Writing a plugin

The SDK [plugins/sdk/hexdb-plugin.mjs](plugins/sdk/hexdb-plugin.mjs) has no dependencies:

```js
import { payloads, api, log } from "../../sdk/hexdb-plugin.mjs"

for await (const change of payloads()) {          // stream, logs and metrics plugins
  if (change.op === "put") log("order", change.id, change.document.total)
}

await api().post("/orders", { total: 12 })         // plugins with [access]
```

Any language works: read JSON lines from stdin, and use `HEXDB_API` and `HEXDB_API_KEY` for the API. Variables a plugin gets: `HEXDB_PLUGIN_ID`, `HEXDB_PLUGIN_TYPE`, `HEXDB_API`, `HEXDB_API_KEY` (with `[access]`), plus its `[env]`.

### Example plugins

All of these are listed, disabled, in [plugins.json](plugins.json).

| Plugin | Type | What it does |
| --- | --- | --- |
| [examples/change-logger](plugins/examples/change-logger) | stream | Logs each change; the minimal example |
| [streams/kafka](plugins/streams/kafka) | stream | Produces changes to Kafka, keyed by document ID |
| [streams/kinesis](plugins/streams/kinesis) | stream | Puts changes on an Amazon Kinesis stream |
| [sinks/otlp](plugins/sinks/otlp) | metrics | Metrics to an OTLP endpoint (built in) |
| [sinks/log-file](plugins/sinks/log-file) | logs | Writes log records to rotating JSON-lines files |
| [sinks/http-logs](plugins/sinks/http-logs) | logs | Posts log records to an HTTP collector (Splunk HEC, Logstash, ...) |
| [sources/postgres](plugins/sources/postgres) | source | Copies a PostgreSQL table and keeps it in sync by an updated-at column |
| [sources/mssql](plugins/sources/mssql) | source | The same for SQL Server |
| [sources/s3](plugins/sources/s3) | source | Imports JSON and JSON-lines objects from an S3 bucket |
| [enrichers/ai-keywords](plugins/enrichers/ai-keywords) | stream | Adds keywords to new documents with Claude |

### Plugin FAQ

**My plugin doesn't start.** Check the Plugins page and the Logs page (filter by the plugin ID). Common causes: `enabled` is false, `plugins.enabled` is false in the config, the command isn't on `PATH` (use an absolute path), or Node dependencies weren't installed (`npm install` in the plugin folder).

**Does a plugin see changes made while it was down?** A stream plugin does, as long as the change history reaches back that far.

**Can a plugin write back to the tessellation it watches?** Yes, with `[access]`. Guard against loops: skip changes your plugin made, for example by checking a field it sets.

**How do I ship the audit trail to a SIEM?** Use a `stream` plugin with `audit = true` and `tessellations = []` (or just `_audit`).

## 17. Security

### Authentication

Every API request needs credentials except:
- `GET /health`;
- `POST /auth/login`;
- `GET /openapi.json`;
- the admin UI's static files;
- `POST /shutdown` with its token;
- the hex-to-hex `/lattice/*` endpoints, which use their own signatures.

Requests without credentials get 401, including requests to paths that don't exist.

**Sessions.**
- Created by `POST /auth/login` with `{"login", "password", "code"?}`.
- The token is set as an `HttpOnly`, `SameSite=Strict` cookie (`Secure` over HTTPS), so page scripts can't read it. Scripts can ask for it with `"return_token": true` and send it as `Authorization: Bearer hxs.…`.
- Sessions last `security.session_hours` (12) and work on every hex of the lattice.
- `POST /auth/logout` revokes the session. `GET /auth/me` returns the signed-in user with their grants.

**API keys** (`hxk_…`) are for scripts, services and the CLI.
- Create them on the Account page or with `POST /auth/keys` (`{"name", "expires_in_days"}`). The key is shown once.
- A key acts with its user's current roles and doesn't use MFA.
- `GET /auth/keys` lists them; `DELETE /auth/keys/{id}` revokes one.

**Passwords.**
- At least 12 characters, not containing the login, not trivial.
- Hashed with Argon2id and never returned.
- `POST /auth/password` changes your own.

**Revocation.**
- Signing out revokes that session.
- Changing or resetting a password, or locking a user, signs that user out everywhere.
- Deleting a user ends their sessions and keys.
- The user record is read on every request, so role changes apply immediately. Live SSE streams re-check every 30 seconds.

**Brute force.**
- After `security.max_failed_logins` (5) failures for one login, or from one address, within `lockout_minutes` (15), sign-in is refused with 429. The count is shared across the lattice.
- Unknown logins take as long as wrong passwords and return the same message, so logins can't be enumerated.

### Multi-factor authentication

HexDB supports time-based one-time passwords (TOTP, RFC 6238: HMAC-SHA1, 30-second steps, 6 digits), which every authenticator app supports, plus ten backup codes.

| Endpoint | Body | Does |
| --- | --- | --- |
| `GET /auth/mfa` | | Whether MFA is on, and how many backup codes are left |
| `POST /auth/mfa/setup` | `{"password"}` | A pending secret and an `otpauth://` URI (the Account page shows a QR code) |
| `POST /auth/mfa/enable` | `{"code"}` | Turns MFA on and returns ten backup codes, shown once |
| `POST /auth/mfa/disable` | `{"password", "code"}` | Turns it off |
| `POST /auth/mfa/backup-codes` | `{"password", "code"}` | New backup codes, replacing the old ones |

**Signing in with MFA.** Once MFA is on, a sign-in without a code fails with 401 `mfa_required`, and the UI asks for one. A current TOTP code or an unused backup code works.

**Codes.**
- A TOTP code is accepted for the current step and one step either side, for clock drift.
- No code is accepted twice.
- Backup codes are stored as Argon2 hashes and work once each.

**Recovery.** An administrator can turn MFA off for a user who lost their device (`PATCH /users/{user}` with `{"use_mfa": false}`, or the Users page).

### Authorization

A user holds role grants. Each grant names a role and the tessellations it applies to: names, `*` for all, or `stream:<name>` for streams.

```bash
curl -X POST http://localhost:7700/users -H "Content-Type: application/json" -d '{
  "login": "ada", "password": "correct horse battery", "email_address": "ada@example.com",
  "roles": [{ "name": "writer", "tessellations": ["orders", "customers"] }, { "name": "reader", "tessellations": ["*"] }] }'
```

A role is a set of permissions:

| Permission | Allows | Scoped to the grant's tessellations |
| --- | --- | --- |
| `read` | Get, list, query, count, aggregate, the change feed; consume streams | yes |
| `write` | Insert, replace, patch, delete; publish to streams | yes |
| `manage` | Delete tessellations; manage indexes, schemas and advice; configure streams | yes |
| `status` | Server status, metrics, storage and lattice details | no |
| `logs` | The server log | no |
| `audit` | The audit trail | no |
| `plugins` | Plugins and their delivery state | no |
| `maintenance` | Flush, compact and back up | no |
| `admin` | Everything: users, roles, settings, functions, schedules, joining hexes, shutdown | no |

Built-in roles:

| Role | Permissions |
| --- | --- |
| `admin` | `admin` |
| `reader` | `read` |
| `writer` | `read`, `write` |
| `owner` | `read`, `write`, `manage` |
| `operator` | `status`, `logs`, `plugins`, `maintenance` (including backups) |
| `auditor` | `status`, `audit` |

Administrators create custom roles from any set of permissions:

```bash
curl -X POST http://localhost:7700/roles -H "Content-Type: application/json" \
  -d '{ "name": "analyst", "description": "Read data and see status", "permissions": ["read", "status"] }'
```

`GET /roles` lists the roles and the permissions a role can hold. A role still granted to someone can't be deleted; the error names who holds it. The last unlocked administrator can't be deleted, locked or demoted.

**Enforcement.**
- One middleware refuses every route outside the public list.
- Each handler checks the specific permission before looking anything up, so a 403 never reveals whether something exists.
- Lists (tessellations, changes, streams, GraphQL) include only what the caller can read.
- Transactions check each operation. GraphQL checks every resolver, including nested fields.
- Row filters and field masks apply at the engine's caller-facing reads and writes, for the request's caller (and for a schedule's or trigger's owner).
- A refused request (403) is recorded in the audit trail.

### Row filters and field masks

A role can be restricted per tessellation (or `*`, every tessellation it's granted on):

```bash
curl -X POST http://localhost:7700/roles -H "Content-Type: application/json" -d '{
  "name": "regional", "permissions": ["read", "write"],
  "restrictions": { "orders": { "filter": { "region": { "$user": "attributes.region" } }, "hide": ["cost", "margin"] } } }'

curl -X PATCH http://localhost:7700/users/ada -H "Content-Type: application/json" -d '{ "attributes": { "region": "EU" } }'
```

**The two parts:**
- `filter` (the filter language) limits the documents the role can see and write. `{"$user": "login"}`, `{"$user": "id"}`, `{"$user": "email"}` and `{"$user": "attributes.<name>"}` stand for the caller's values. Users' attributes are set by administrators: a JSON object of up to 50 entries. A missing attribute makes the filter match nothing, never everything.
- `hide` lists fields (dotted paths) the role can't see or change.

**Combining grants.** When several of a user's grants allow an action on a tessellation:
- an unrestricted grant makes access unrestricted;
- otherwise the user sees the documents any of their filters allows;
- a field is hidden only if every grant hides it.

**Reads:**
- Queries, counts, aggregations, `GET /tessellations/{name}`, GraphQL, the change feed and functions only see matching documents. Other documents read as not found.
- Hidden fields are removed from every document returned.
- Filtering, sorting, grouping or aggregating on a hidden field is refused with 403, so values can't be probed. So is `$text` unless the text index covers no hidden field.
- The query plan's `scanned` count reports only visible documents.
- Delete events in the change feed carry no document, so they're shown by ID.

**Writes:**
- A document must match the filter before (replace, patch, delete) and after (every write) the change. Writes that would move a document out of reach are refused with 403.
- Writes can't set or remove hidden fields, and a full replace keeps their stored values.
- Update-by-filter only touches matching documents.
- An upsert whose key matches a document outside the filter fails with 409.

**Limits:**
- Managing a tessellation (indexes, schemas, deletion) needs an unrestricted grant.
- A stream can only be sourced from a tessellation the creator can read without restriction (a stream copies every change).
- Administrators are never restricted, and restrictions apply to user tessellations only.

### The audit trail

HexDB records security-relevant events as documents in the `_audit` system tessellation:

| Area | Actions |
| --- | --- |
| Accounts | `auth.*` (sign-in, failures, sign-out, password, MFA, API keys), `user.*`, `role.*` |
| Data structure | `tessellation.*`, `index.*`, `schema.*` (including `schema.rollback`) |
| Features | `stream.*`, `function.*`, `schedule.*`, `trigger.*` |
| Operations | `maintenance.*` (flush, compact, backup), `settings.update`, `server.shutdown`, `lattice.join_info` |
| Refusals | `access.denied` |

Each event holds the actor, the action, the target, the outcome, the client address and details. Passwords, tokens and keys are never recorded. Events expire after `security.audit_retention_days` (90; 0 keeps them forever).

```bash
curl "http://localhost:7700/audit?actor=ada&action=auth.&since=1760000000000&limit=100"
```

Filters: `actor`, `action` (a value ending in `.` matches that area), `target`, `outcome`, `since` and `until` (epoch milliseconds), `limit` and `offset`. Results are newest first and need the `audit` permission.

**Where events go.**
- Only the Overseer writes events. A replica forwards its events to the Overseer, signed, with a 2-second timeout, and keeps nothing itself. An event is lost if the Overseer is unreachable at that moment, though it's still in the replica's log.
- Every event is also logged at info under `hexdb::audit`.
- A stream plugin with `audit = true` can ship events elsewhere.

### Browsers

- Cookie-authenticated changes must come from the same origin (CSRF protection), and cross-site sign-in is refused.
- Responses carry `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, `Referrer-Policy: no-referrer`, and a Content Security Policy. The admin UI loads nothing from other origins.
- API responses are `Cache-Control: no-store`.
- No CORS headers are sent, so other sites can't read responses.

### Transport

Set `[tls] cert_file` and `key_file` (PEM) to serve HTTPS (HTTP/1.1 and HTTP/2 via ALPN). With TLS:
- HSTS is sent;
- the session cookie is `Secure`;
- hexes replicate over HTTPS, trusting `tls.ca_file` for private CAs.

Without TLS, HexDB warns when it listens on a non-loopback address.

Behind a reverse proxy, list the proxy in `network.trusted_proxies` (IPs or CIDR ranges). The client address is then taken from `X-Forwarded-For`: the right-most address that isn't a trusted proxy. Otherwise the TCP peer is the client. The client address is what sign-in throttling, connection limits and the audit trail use.

### Encryption at rest

All data and metadata on disk is encrypted with AES-256-GCM using `storage.encryption_key`.

| File | Format |
| --- | --- |
| WAL records | `[length][nonce][ciphertext]`, the record compressed with Zstandard first |
| SSTable bodies | Each authenticated with its document ID and sequence number, so bodies can't be swapped between entries |
| SSTable indexes | In chunks of 64 records, each authenticated with its chunk number |
| Catalog, settings, metrics history, index snapshots | Sealed files: `HXE1` magic, key ID, nonce and ciphertext, authenticated with the file name so files can't be swapped |

Only SSTable headers (format, counts, offsets, key ID) are plaintext. Every nonce is random (96 bits). Documents in memory aren't encrypted.

**Rotating the key:**

1. Set a new `storage.encryption_key` and move the old one to `storage.previous_encryption_keys`.
2. Restart. Old data stays readable, and everything new uses the new key.
3. Compaction (scheduled, or `POST /compact`) rewrites SSTables, and every sealed file is rewritten on its next save.
4. When `/status` shows `storage.sstable_files_on_old_keys: 0`, remove the old key.

A server whose data needs a key that isn't configured refuses to start, rather than discard anything. The error names the setting to fix.

### Between hexes

Hexes authenticate each other with the lattice key: `network.lattice_secret`, or, when that's empty, a key derived from `storage.encryption_key`. The key is never sent.

- **Discovery** is a mutual challenge-response. The prober sends a hello with a timestamp, a nonce and a MAC. The responder answers only a fresh (within 60 seconds), never-seen, valid hello, with an identity MAC bound to the prober's nonce. Strangers learn nothing, and a replayed or forged reply can't impersonate an Overseer.
- **Replication and other `/lattice/*` requests** carry `X-HexDB-Lattice-Signature`: a timestamp, a nonce, and a MAC over the method, path, query and body hash. Each is valid for 60 seconds and once. User sessions and API keys are never accepted there.
- MACs are keyed BLAKE3 with keys derived from the lattice key, compared in constant time.
- Session tokens are signed with a key derived from the lattice key, which is why a session works on every hex.

**Rotating the lattice secret** without downtime:

1. On each hex in turn, set the new `lattice_secret` and add the old one to `network.previous_lattice_secrets`. Use the word `derived` for the key derived from the storage key, when moving from no secret to an explicit one. Restart that hex.
2. Hexes sign with the current key and accept any listed key. A hex whose request isn't accepted retries with its previous keys, so mixed hexes keep talking.
3. Once every hex has the new secret, remove `previous_lattice_secrets`. API keys are re-hashed with the new key as they're used. Sessions signed with an old key end when it's removed.

### Local files

- `hexdb.pid` (the shutdown token) and `initial-admin-password.txt` are readable by the owner only: mode 0600 on Unix, and on Windows inherited permissions are removed, leaving the current user and SYSTEM.
- Secrets belong in `hexdb.local.toml` or environment variables, never in a committed `hexdb.toml`.
- `GET /settings` hides secrets in the effective configuration.

## 18. Lattices and replication

### Discovery and election

Every hex runs a TCP discovery listener (`network.discovery_endpoint`, default port 7702). Every `discovery_interval_seconds` (10), it probes:
- the seed addresses in `network.peers` (other hexes' discovery endpoints);
- the local ports 7702-7709, unless `scan_local_ports = false`.

Probes run concurrently with a 500 ms timeout. A peer missing for three rounds is "lost", and it's forgotten after 30.

Every hex runs the same election over the same view, so they agree without a consensus round:

1. Candidates are live hexes whose `identity.role` is `auto` or `overseer`. If any prefer `overseer`, only those count.
2. A hex that is already Overseer keeps the role while it's alive, so leadership doesn't flap when a larger hex joins. If two claim it (after a partition heals), the better-ranked one wins.
3. Otherwise candidates are ranked by RAM, then disk, then ID.
4. Every other hex is a Harvester, or a Replicant if it prefers that. `harvester` and `replicant` hexes never lead.

### Replication

Every hex keeps a full copy. A replica:

1. **Full sync.** It reads a consistent snapshot from the Overseer: its sequence number S, its tessellations and index definitions, then every document with its version, page by page. Local data the Overseer doesn't have is removed.
2. **Streaming.** It follows the Overseer's change feed after S with long polls, applying each batch atomically together with its replication cursor, so a crash can't leave data and cursor out of step. A change older than the snapshot's version of a document is skipped, so changes made during the snapshot apply exactly once.

**Resuming.**
- The cursor names the Overseer's history ID, kept in its catalog, and a position in it.
- After an Overseer restart the history is the same, so replicas resume from their cursor, reading older changes from the Overseer's change history if needed.
- After a failover the new Overseer has a different history, so replicas take a full sync from it.

**Schemas, indexes and tessellations.** Replicas copy these definitions from the Overseer's catalog. Each change-feed response carries a fingerprint of the catalog, and a replica re-reads the catalog as soon as the fingerprint changes. A new schema version or index is therefore on every hex within one poll (a few seconds), and a replica that takes over as Overseer enforces the same schemas. A periodic check every 15 seconds backs this up.

**Progress.** Each poll for changes after N acknowledges N, which gives the Overseer each replica's exact position. `/status` shows the lag per replica, and the dashboard's lattice card shows it too.

**Reads and writes.** Replicas serve reads. A write sent to a replica is forwarded to the Overseer (`replication.forward_writes`, default true), signed with the lattice key, and the response carries `X-HexDB-Forwarded-To`. With forwarding off, the write fails with 421 `read_only_replica`, naming the Overseer.

### Durability options

By default replication is asynchronous. A write the Overseer acknowledged, but no replica had received yet, is lost if the Overseer fails before it comes back. Two settings trade latency for safety:

```toml
[replication]
min_acks = 1          # replicas that must have applied a write before it's acknowledged
ack_timeout_ms = 5000 # on timeout the write stays committed and the client gets 503 replication_timeout
quorum = 2            # hexes (including itself) the Overseer must see to accept writes
```

- **`min_acks`.** With it set, an acknowledged write exists on at least that many replicas. Writes to system tessellations that don't need it (the audit trail, sign-in throttling, replication state, plugin cursors) don't wait.
- **`quorum`.** A hex cut off from the rest stops taking writes, with 503 `no_quorum`. With a majority quorum (2 of 3, 3 of 5), a network partition can't leave two Overseers both accepting writes. Startup work that needs to write (creating the first admin) waits for the quorum.

### Adding a hex

On the Dashboard, "Add a hex" (administrators, password required) shows:
- a `hexdb.toml` snippet: the lattice name, seed addresses, and addresses for the new hex;
- a `hexdb.local.toml` snippet with the shared secret;
- notes on network reachability.

`POST /join {"password"}` returns the same. Each request is audited.

The lattice name is `network.lattice_name`. When it's left empty on the first hex, HexDB generates a name (an adjective and a noun) and saves it in the catalog. Every other hex must use the same name.

To try a lattice on one machine, either start a separate demo lattice or add hexes to your own server:

```bash
node scripts/lattice-demo.mjs          # three new hexes on 7800/7810/7820 with sample data; type stop N / start N / list / quit
hexdb lattice spawn --count 2          # two Harvesters joining the lattice of the hex in ./hexdb.toml
hexdb lattice list
hexdb lattice stop --remove            # stop them and delete their folders
```

In the demo, `stop 1` stops the Overseer. Within a few seconds another hex is elected, and the remaining replica takes a full sync from it, because the new Overseer has its own change history. The stopped hex shows as "lost" until `start 1` brings it back as a replica.

Spawned hexes live in `.hexdb-local/hex-N/` next to the config, on free ports, with the same lattice name and secret (or storage key), and the main hex as a seed.

For hexes on different machines:
- bind `api_endpoint` and `discovery_endpoint` to `0.0.0.0` or the machine's address;
- set `network.advertise_host` to the address other hexes should use;
- list the other hexes in `peers`;
- open the API and discovery ports between them;
- give every hex the same `lattice_name` and `lattice_secret`.

Each hex can have its own `storage.encryption_key` when a `lattice_secret` is set.

## 19. Storage internals

### Write path

1. A write is validated: permissions, size limits, the schema, and expected versions for read-modify-write operations.
2. Under the engine's state lock, the batch gets consecutive sequence numbers, is queued to the WAL (so WAL order equals sequence order), and is applied to memory and to indexes.
3. The lock is released. A dedicated WAL thread batches whatever is waiting into one write and one fsync (group commit). With `storage.wal_sync = false` it skips the fsync. That's faster, but a power loss or OS crash can lose recently acknowledged writes.
4. Once the record is durable, the write is acknowledged, published to the change feed, and (with `min_acks`) held until enough replicas confirm.

### Read path

Memory holds the newest version of every unflushed document plus an LRU cache of recently read ones, within `memory.ram_mb`. On a miss:
1. HexDB asks each of the tessellation's SSTables, newest first.
2. Each SSTable checks its Bloom filter, finds the right index chunk by binary search, and reads and decrypts it. Recently used chunks are cached.
3. The highest sequence number wins. Tombstones and expired documents read as not found.
4. The result is cached.

### Counts

Each tessellation's document count is cached and kept exact by commits. Under the state lock, a commit checks whether each document it writes was visible before (memory, then the SSTable Bloom filter and index) and adjusts the count by the difference. A count is computed from the key index only when first needed, after its tessellation is dropped, or once the earliest TTL among its documents passes. So unfiltered totals, `GET /tessellations/{name}`, and the dashboard's per-tessellation figures don't rescan a busy tessellation after every write. Other statistics (sizes for `/status`) are cached per tessellation until that tessellation changes.

### Flush and compaction

**Flush.** Every `wal_flush_check_frequency` seconds, when memory needs room, and at shutdown, dirty entries are written to new SSTables (`<tessellation>/<ulid>.hxs`). Each file is written to a temporary name, fsynced and renamed into place. Then:
- the WAL rotates to a new segment;
- older segments are retired to `wal/archive/`;
- index snapshots are saved.

Clean entries over `ram_mb` are evicted, least recently used first.

**Compaction.** Every `compaction_frequency` seconds, or on `POST /compact`, a tessellation's SSTables are merged into one, keeping the newest version of each document. Tombstones and expired documents are dropped once no older version can remain. Compaction also rewrites files written in older formats or with a previous key.

### Recovery

At startup:
1. HexDB reads the catalog and opens each SSTable, verifying every index chunk.
2. It replays the WAL segments in order. Records for tessellations dropped after they were written are skipped.
3. A torn final record (a crash mid-write) ends its segment with a warning. Any other record that can't be decrypted stops the startup, so a wrong key can't cause data to be thrown away.
4. Replayed data is flushed, indexes are loaded from snapshots or rebuilt, and the hex joins the lattice.

Every acknowledged write is in the WAL or an SSTable at every moment, because WAL segments are retired only after the SSTables holding their contents are durable.

### Memory and vertices

Every document held in memory is split into six shards with Reed-Solomon coding (four data shards, two parity), one per vertex. Each shard carries a BLAKE3 hash.

- Reading a document checks the hashes and rebuilds it from any four good shards.
- A background check (`memory.vertex_integrity_check_frequency`, 300 s) verifies every shard and rewrites corrupt ones in place.

Any two of the six vertices can be lost or corrupted without losing data. The dashboard's vertex hexagon shows each vertex's shards, bytes and repairs. This protects against memory corruption. Durability comes from the WAL and SSTables.

### Data layout

```text
<storage.path>/
├── catalog.hxe                tessellations, index definitions, schemas, history ID, lattice name (sealed)
├── settings.hxe               runtime settings changed from the UI or API (sealed)
├── metrics-history.hxe        6 hours of metrics samples (sealed)
├── query-stats.hxe            the query advisor's statistics (sealed)
├── hexdb.pid                  the running server's PID, endpoint and shutdown token (owner-only)
├── initial-admin-password.txt only until you delete it
├── wal/
│   ├── <first-seq>.wal        active write-ahead log segments
│   └── archive/<first-seq>.wal  the change history
├── indexes/<tessellation>/<index>.hxi   index snapshots (sealed)
└── <tessellation>/<ulid>.hxs  SSTables, including system tessellations such as users, _audit, _streams
```

### File formats

**WAL record.** `[u32 big-endian length][12-byte nonce][ciphertext]`. The ciphertext is AES-256-GCM over the Zstandard-compressed JSON record `{"seq", "time", "op"}`. `op` is a put, a delete, a tessellation drop, or a batch of these, written atomically.

**SSTable (version 4).** All integers are big-endian.

| Offset | Field | Size |
| --- | --- | --- |
| 0x00 | Magic `HXDB` | 4 |
| 0x04 | Version (4) | 2 |
| 0x06 | Compression (1 = Zstandard) | 1 |
| 0x07 | Encryption (1 = AES-256-GCM) | 1 |
| 0x08 | Entry count | 8 |
| 0x10 | Created (epoch ms) | 8 |
| 0x18 | Index offset | 8 |
| 0x20 | Index size | 8 |
| 0x28 | Index checksum (first 8 bytes of BLAKE3 over the index block) | 8 |
| 0x30 | Max sequence number | 8 |
| 0x38 | Key ID (identifies the key without revealing it) | 8 |
| 0x40 | Bodies | ... |

Each body is `nonce (12) | AES-256-GCM(zstd(JSON document))`, with the document ID and sequence number as authenticated data. Tombstones have no body.

The index block is a series of chunks of up to 64 records. Each chunk is `length (4) | nonce (12) | AES-256-GCM(records)`, authenticated with its chunk number. A record is `id (16) | flags (1: bit 0 TTL, bit 1 tombstone) | seq (8) | [ttl (8)] | body offset (8) | body length (4)`.

Only the chunk directory (each chunk's first ID, offset and length) and a Bloom filter of the IDs stay in memory, about 2 bytes per document.

Versions 2 (unencrypted) and 3 (encrypted bodies, plaintext index) are still read. Compaction rewrites them as version 4.

**Sealed file.** `HXE1 | key ID (8) | nonce (12) | AES-256-GCM(content)`, authenticated with the file name.

## 20. Limits and configuration reference

### Limits

| Limit | Default | Setting |
| --- | --- | --- |
| Document size | 1 MB of JSON | `limits.max_document_kb` (live) |
| Bulk, query, transaction and GraphQL bodies | 32 MB | `limits.max_request_mb` |
| Other request bodies | 2 MB | fixed |
| Documents per bulk request | 10,000 | fixed |
| Operations per transaction | 1,000 | fixed |
| Page size | 1,000 (default 100) | fixed |
| Disk space | 8 GB (the sample config uses 16 GB) | `storage.disk_mb` (live). Over it, writes that add data get 507 and deletes still work |
| Memory for documents | 1 GB | `memory.ram_mb` |
| Open connections | 1,024; 128 per client address | `limits.max_connections`, `max_connections_per_client` |
| Request time | 60 s (long polls and streams exempt) | `limits.request_timeout_seconds` |
| Header and TLS handshake time, idle keep-alive | 10 s | `limits.header_timeout_seconds` |

Disk usage is measured by the metrics task every 15 seconds and after every flush.

### Runtime settings

These settings can be changed from the Settings page or `PUT /settings` (administrators) without editing files:

```bash
curl http://localhost:7700/settings
curl -X PUT http://localhost:7700/settings -H "Content-Type: application/json" \
  -d '{ "limits.max_document_kb": 2048, "storage.wal_sync": null }'     # null removes an override
```

Changes are saved, encrypted, in the data directory and apply on top of the config files at every start, for this hex only. Live settings take effect at once; the rest at the next restart, and `GET /settings` lists the changes still waiting.

| Live | Applies at restart |
| --- | --- |
| `limits.max_document_kb`, `storage.disk_mb`, `storage.change_history_hours`, `storage.change_history_mb`, `security.session_hours`, `security.audit_retention_days`, `replication.min_acks`, `replication.ack_timeout_ms` | `limits.max_request_mb`, `limits.max_connections`, `limits.max_connections_per_client`, `limits.request_timeout_seconds`, `limits.header_timeout_seconds`, `memory.ram_mb`, `memory.ttl_scan_frequency`, `security.max_failed_logins`, `security.lockout_minutes`, `storage.compaction_frequency`, `storage.wal_flush_check_frequency`, `storage.wal_sync`, `compression.compression_level`, `replication.forward_writes`, `replication.quorum`, `plugins.enabled` |

### Configuration reference

| Setting | Default | Meaning |
| --- | --- | --- |
| `network.api_endpoint` | `127.0.0.1:7700` | REST, GraphQL and UI address |
| `network.discovery_endpoint` | `127.0.0.1:7702` | Lattice discovery address |
| `network.lattice_name` | generated | Hexes with the same name form a lattice |
| `network.peers` | `[]` | Other hexes' discovery endpoints |
| `network.scan_local_ports` | `true` | Also probe local ports 7702-7709 |
| `network.discovery_interval_seconds` | 10 | Seconds between discovery rounds |
| `network.advertise_host` | none | The host other hexes should use, when binding 0.0.0.0 |
| `network.lattice_secret` | derived | The shared key that authenticates hexes |
| `network.previous_lattice_secrets` | `[]` | Old secrets still accepted, for rotation |
| `network.trusted_proxies` | `[]` | Proxies whose `X-Forwarded-For` is trusted |
| `identity.role` | `auto` | `auto`, `overseer`, `harvester` or `replicant` |
| `storage.path` | `./.hexdb` | Data directory |
| `storage.encryption_key` | required | `base64:` plus 32 bytes |
| `storage.previous_encryption_keys` | `[]` | Old keys still accepted for reading |
| `storage.disk_mb` | 8192 | Disk budget |
| `storage.wal_sync` | `true` | fsync before acknowledging |
| `storage.wal_flush_check_frequency` | 60 | Seconds between flush checks |
| `storage.compaction_frequency` | 1800 | Seconds between compactions |
| `storage.change_history_hours`, `change_history_mb` | 24, 512 | Change history kept on disk |
| `storage.backup_path` | `backups` next to the data directory | Where backups are written; must be outside the data directory |
| `memory.ram_mb` | 1024 | Memory budget for documents |
| `memory.ttl_scan_frequency` | 600 | Seconds between expiry sweeps |
| `memory.vertex_integrity_check_frequency` | 300 | Seconds between vertex checks |
| `compression.compression_level` | 0 | Zstandard level (-7 to 22; 0 is the library default) |
| `security.admin_login`, `admin_email` | `hexdbadmin` | The first administrator |
| `security.admin_password` | empty | Generated when empty; ignored once users exist |
| `security.session_hours` | 12 | Session lifetime |
| `security.max_failed_logins`, `lockout_minutes` | 5, 15 | Sign-in throttling |
| `security.audit_retention_days` | 90 | Audit event lifetime (0: forever) |
| `tls.cert_file`, `key_file` | empty | PEM certificate chain and key; both set means HTTPS |
| `tls.ca_file` | empty | Extra CAs trusted for other hexes |
| `limits.*` | see above | Request and connection limits |
| `replication.min_acks`, `ack_timeout_ms`, `forward_writes`, `quorum` | 0, 5000, true, 0 | See [Durability options](#durability-options) |
| `plugins.enabled`, `registry` | true, `plugins.json` | Plugin loading |
| `functions.scripts`, `python`, `node` | true, `python3`/`python`, `node` | Script runtimes |
| `ai.api_key_env`, `model`, `base_url` | `ANTHROPIC_API_KEY`, `claude-sonnet-5-5`, Anthropic API | Query advisor AI |
| `analyzers.<name>` | none | Custom text analyzers |
| `ui.path` | `../hexdb_admin/dist` | Built admin UI |

## 21. Operations

```bash
curl http://localhost:7700/health                       # public: {"status": "ok"}; signed-in users also get name, role, version
curl http://localhost:7700/status                       # documents, memory, disk, operations, vertices, lattice, replication
curl "http://localhost:7700/status/history?minutes=60"  # a metrics sample every 15 s, kept for 6 hours (survives restarts)
curl "http://localhost:7700/logs?level=warn&limit=100"  # recent log records; ?after= tails, ?q= searches, ?target= filters by module
curl -X POST http://localhost:7700/flush                # write unflushed data to SSTables
curl -X POST http://localhost:7700/compact              # merge SSTables now
curl -X POST http://localhost:7700/backup -d '{"name": "nightly-1"}' -H "Content-Type: application/json"
curl http://localhost:7700/backups                      # backups in storage.backup_path
curl http://localhost:7700/plugins
curl http://localhost:7700/openapi.json                 # the REST API as OpenAPI 3.1
```

**Logging.** The server keeps its last 5,000 log records in memory for `/logs` and the Logs page. `RUST_LOG` sets the level for the console and the in-memory log. Writes, errors and background tasks log at info; reads at debug; refused requests at warn.

**Backups.** `POST /backup` (or `hexdb backup`, both needing the `maintenance` permission) writes a consistent copy of the data directory while the hex keeps serving reads and writes:

1. The backup holds off flushes and compaction, fixes a sequence number, rotates the WAL so every write up to that number is in a closed segment, and captures the catalog.
2. SSTables are immutable, so they're hard-linked into the backup when it's on the same file system (copied otherwise). The closed WAL segments, index snapshots and encrypted metadata are copied.
3. The backup is built in `<name>.partial/` and renamed when complete, with a plaintext `backup.json` manifest (time, sequence number, file count; no names or data).

Writes continue throughout, but flushes wait until the backup finishes; with hard links that's moments. `GET /backups` (or `hexdb backup --list`) lists backups. A backup name may contain letters, digits, `-` and `_`; without one, the name is the time and sequence number.

Restoring: stop the hex (or start a new one), point `storage.path` at the backup folder (or copy it into the data directory), and start it with the same encryption keys. Recovery replays the copied WAL on top of the SSTables, exactly as after a crash at the backup's sequence number. The restored data gets a new change history ID, so replicas take a full sync from it and plugins start at its end, rather than mistaking it for the original's history.

Hard-linked backups share disk blocks with the live data, so they don't protect against losing the disk: copy backup folders to other storage. Back up `storage.encryption_key` separately too; the data is useless without it. A Replicant is another option: a live copy you can back up while it's stopped.

**Upgrades.** Stop the hex, replace the binaries and the UI, and start it. Data in older formats is read and rewritten by compaction. In a lattice, upgrade replicas first, then the Overseer. A failover moves the Overseer role while it's down.

**Monitoring.** Use the `@sinks/otlp` plugin for metrics, a logs plugin for logs, and the audit trail for security events. `/status` is suitable for health checks that need more than `/health`.

## 22. Drivers

| Language | Folder | Install |
| --- | --- | --- |
| JavaScript / TypeScript | [drivers/node](drivers/node) | `npm install hexdb` (Node.js 18+, Deno, Bun, browsers) |
| Python | [drivers/python](drivers/python) | `pip install hexdb` (3.9+, no dependencies) |
| .NET | [drivers/dotnet](drivers/dotnet) | `dotnet add package HexDB.Client` (.NET 8+) |
| Entity Framework Core | [drivers/dotnet/HexDB.EntityFrameworkCore](drivers/dotnet/HexDB.EntityFrameworkCore) | `dotnet add package HexDB.EntityFrameworkCore` (.NET 8+, EF Core 8) |

Each driver covers:
- documents, queries, counts, aggregations and upserts;
- transactions and idempotency keys;
- the change feed;
- streams with consumer groups;
- functions and GraphQL.

They authenticate with an API key or a login, and retry 429 and 503 for reads and for writes that carry an idempotency key. Package names are as published by the release workflow. See [drivers/README.md](drivers/README.md) for examples and how to run their tests.

### Entity Framework Core

`HexDB.EntityFrameworkCore` (in [drivers/dotnet/HexDB.EntityFrameworkCore](drivers/dotnet/HexDB.EntityFrameworkCore)) is an EF Core 8 provider built on `HexDB.Client`. Install it with `dotnet add package HexDB.EntityFrameworkCore` and configure a context:

```csharp
public class Order
{
    public string Id { get; set; } = null!;   // leave null: a ULID is generated on Add
    public string Customer { get; set; } = "";
    public double Total { get; set; }
    public OrderStatus Status { get; set; }
    public List<string> Tags { get; set; } = new();
}

public class ShopContext : DbContext
{
    public DbSet<Order> Orders => Set<Order>();

    protected override void OnConfiguring(DbContextOptionsBuilder options) =>
        options.UseHexDB("http://127.0.0.1:7700", Environment.GetEnvironmentVariable("HEXDB_API_KEY"));
}

await using var db = new ShopContext();
await db.Database.EnsureCreatedAsync();          // creates the tessellations
db.Orders.Add(new Order { Customer = "ada", Total = 12 });
await db.SaveChangesAsync();
var big = await db.Orders.Where(o => o.Total > 10).OrderByDescending(o => o.Total).Take(20).ToListAsync();
```

`UseHexDB(url, apiKey, o => ...)` takes two options: `o.HttpClient(client)` to supply your own `HttpClient`, and `o.PageSize(n)` (1 to 1000, default 500), which sets how many documents each request reads.

**Mapping.**
- Each entity type is a tessellation, named after its `DbSet` property (or the class name). Override it with `modelBuilder.Entity<T>().ToTessellation("name")`.
- Each property is a document field of the same name. Override it with `.Property(x => x.P).ToJsonProperty("name")`.
- The key is the document's `id`, so it must be a single `string` or `Guid` property.
  - A `string` key left `null` gets a new ULID when the entity is added. EF Core only generates a value when the key holds its default, so initialize string keys to `null!`, not `""`.
  - A `Guid` key is stored as the ULID with the same 128 bits.
- Property types:
  - numbers, strings, booleans, enums (stored as numbers);
  - `DateTime`, `DateTimeOffset`, `DateOnly`, `TimeOnly`, `TimeSpan`, `Guid`, `byte[]`;
  - arrays, lists and string-keyed dictionaries of those;
  - `JsonNode` and `JsonElement` for free-form JSON.
- Value converters apply as usual.
- `decimal` is stored as a JSON number, which the server reads as a 64-bit float, so it keeps about 15 significant digits.

**Queries.** LINQ is translated into one `_query` request: filters, sorting and paging run on the server. Results come back a page at a time and are tracked and identity-resolved like any EF Core query. Supported:
- `Where`, with:
  - `==`, `!=`, `<`, `<=`, `>`, `>=`, `&&`, `||`, `!`;
  - null checks and boolean properties;
  - `string.Contains`, `StartsWith`, `EndsWith`, `Equals` and `string.IsNullOrEmpty`;
  - `list.Contains(x.P)` (becomes `$in`) and `x.Tags.Contains(value)` (an array element);
  - `EF.Property(x, "P")`.
- `OrderBy`, `ThenBy`, and their `Descending` forms. String sorting is ordinal (case-sensitive).
- `Skip` and `Take`.
- `First`, `Single`, `Count`, `LongCount` and `Any`, with or without a predicate.
- `Find`.
- `Select`: the projection runs on the client, after the documents are read.
- `AsNoTracking`, async execution, and `ToQueryString()`, which describes the HexDB request.

A query HexDB can't answer throws `InvalidOperationException` instead of loading the tessellation and filtering in memory. This covers, for example:
- method calls such as `ToUpper()` in a filter;
- `Sum` and other aggregates (use the client's `AggregateAsync`);
- `GroupBy` and `Join`;
- `Where` after `Skip`, `Take` or `Select`.

**Saving.** `SaveChanges` sends every added, modified and deleted entity as one `POST /transactions`, so the save is atomic: if one operation fails, none apply.
- Added entities are inserted.
- Modified entities are patched with only the changed properties; a property set to `null` removes the field.
- Deleted entities are deleted.

Concurrency tokens (`[ConcurrencyCheck]` or `.IsConcurrencyToken()`) become an `if_match` precondition on their original values. A document changed or deleted since it was read raises `DbUpdateConcurrencyException`. Any other refusal, such as a unique-index conflict, raises `DbUpdateException` with HexDB's status and message.

**Database operations.**
- `EnsureCreated` creates the model's tessellations; existing ones are kept.
- `EnsureDeleted` drops them.
- `CanConnect` checks `/health`.
- Indexes, schemas and permissions aren't created from the model. Set them up through the API, the CLI or the admin UI.

**Not supported yet.**
- Navigations and relationships: store related ids as properties and query them separately.
- Owned types and complex types: use a property of a JSON-serializable type instead.
- Inheritance.
- Composite keys.
- Explicit transactions: `BeginTransaction` throws, because each `SaveChanges` is already one transaction.
- Migrations.

### ODBC and JDBC

There's no ODBC or JDBC driver yet: both need a SQL dialect. A SQL endpoint is planned, followed by an ODBC driver that sends SQL to it. Until then, tools with REST or JSON data sources (Power BI, Tableau Web Data Connectors, Grafana's JSON data source) can read HexDB directly.

## 23. FAQ

**How do I choose between `_update`, a transaction and a function?**
- `_update` applies one merge patch to everything a filter matches.
- A transaction combines different operations with preconditions.
- A function saves either one (or a script) under a name with parameters, so clients don't repeat it.

**Why did my write return 503 but the data is there?** With `replication.min_acks` set, the write committed on the Overseer, but not enough replicas confirmed within `ack_timeout_ms`. Retry with the same idempotency key to be safe.

**Why can't I name a tessellation `settings`?** API routes use that name. Reserved names include `auth`, `audit`, `backup`, `backups`, `triggers`, `settings`, `join`, `streams`, `functions`, `schedules`, `analyzers`, `schemas`, `changes`, `logs`, `plugins`, `lattice`, `status`, `health` and `ui`. Names starting with `_` are reserved too, and `users` and `roles` are taken by system tessellations.

**A replica is far behind or keeps re-syncing.**
1. Check the Logs page on the replica, filtered by `hexdb_core::replication`.
2. If the Overseer's change history doesn't reach back to the replica's position, the replica takes a full sync; raise `change_history_hours` or `change_history_mb`.
3. After a failover, a full sync from the new Overseer is expected.

**I changed `encryption_key` and the server won't start.** Put the old key in `storage.previous_encryption_keys`; see [Encryption at rest](#encryption-at-rest).

**Two hexes don't see each other.** Check that:
- they share `lattice_name` and the lattice key (or the same storage key, without a `lattice_secret`);
- each can reach the other's discovery port;
- `peers` lists them;
- `advertise_host` is set when they bind `0.0.0.0`.

Discovery failures are logged under `hexdb_core::network`.
