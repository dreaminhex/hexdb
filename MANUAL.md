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
8. [GraphQL and gRPC](#8-graphql-and-grpc)
9. [Aggregations](#9-aggregations)
10. [SQL](#10-sql)
11. [Transactions](#11-transactions)
12. [Indexes and the query advisor](#12-indexes-and-the-query-advisor)
13. [Schemas](#13-schemas)
14. [Change data](#14-change-data)
15. [Streams](#15-streams)
16. [Files](#16-files)
17. [Functions, schedules and triggers](#17-functions-schedules-and-triggers)
18. [Plugins](#18-plugins)
19. [Security](#19-security)
20. [Lattices and replication](#20-lattices-and-replication)
21. [Storage internals](#21-storage-internals)
22. [Limits and configuration reference](#22-limits-and-configuration-reference)
23. [Operations](#23-operations)
24. [Drivers](#24-drivers)
25. [FAQ](#25-faq)

---

## 1. Overview

HexDB stores JSON documents in named collections (tessellations) and serves them over HTTP: a REST API, a GraphQL endpoint, a read-only SQL endpoint, and an admin UI, all from one server binary (`hexdb_api`). A command-line tool (`hexdb`) starts, stops and inspects servers.

What it provides:

| Area | Features |
| --- | --- |
| Data | Documents with generated, time-sortable IDs; replace, merge-patch, delete; bulk writes; upserts; update-by-filter; per-document TTLs; idempotency keys; optional versioned schemas, inferred from existing documents and saved to apply now or later |
| Files | Any binary file, deduplicated, compressed when that helps, encrypted and split across the six vertices; metadata (EXIF, IPTC, XMP, ID3, Vorbis, QuickTime, Matroska, Broadcast WAV, PDF, Office, ...) extracted and searchable; thumbnails; versions with rollback; a trash; resumable uploads; ranged downloads |
| AI | Your own provider and key (Claude, OpenAI, GitHub Models, Azure OpenAI, local OpenAI-compatible servers): AI tags and descriptions for files, AI-chosen video thumbnails, a second opinion from the index advisor; token estimates, usage per feature and a monthly budget |
| Configuration | Lattice-wide settings in the admin UI and `hexdb config`: encrypted, versioned, audited, checked on every hex, with rollback and write-only secrets |
| Reading | A JSON filter language, sorting, cursor and offset paging, counts, aggregations, full-text search with analyzers, GraphQL, SQL `SELECT` |
| Consistency | Every write is atomic and durable before it's acknowledged; multi-document transactions are serializable for the documents they touch |
| Performance | Field, composite, unique and text indexes; an index advisor; an LRU memory cache; SSTables with Bloom filters; Zstandard compression |
| Change data | An ordered change feed with an on-disk history; streams (publish/subscribe) with consumer groups; plugins for Kafka, Kinesis, OpenTelemetry and more |
| Logic | Functions (saved queries, aggregations, transactions, scripts in Python, TypeScript or JavaScript), schedules, and triggers that run code before or after writes |
| Distribution | Several servers form a lattice with automatic discovery, election, replication and failover, built to stay up on slow and shared CPUs. Slicing spreads the data across the replicas. Synchronous acknowledgements, a quorum, and write forwarding are optional. Each hex checks for new releases; HexDB Enterprise updates a lattice one hex at a time |
| Security | Sessions, API keys, TOTP MFA, built-in and custom roles with row filters, hidden and masked fields (PCI DSS-style card-number masking) and folder limits for files, an audit trail, AES-256-GCM encryption at rest, TLS, authenticated hex-to-hex traffic |

What it doesn't do (yet):

- Write through SQL. SQL is read-only; writes go through REST, GraphQL, gRPC and the drivers.

## 2. Terminology

**Hex.** One running HexDB server.

**Vertex.** One of the six regions of a hex's memory. Every document held in memory is split into six fragments with Reed-Solomon coding (four data, two parity), one fragment per vertex. Any two vertices can be lost or corrupted and the document is still rebuilt. See [Memory and vertices](#memory-and-vertices).

**Tessellation.** A collection of documents, like a table or a MongoDB collection. Roles are granted per tessellation. Names are 1-64 letters, digits, `_` or `-`, unique ignoring case. They can't start with `_` (reserved for system tessellations) or match an API route name (`users`, `roles`, `streams`, `functions`, `settings`, ...).

**Files tessellation.** A tessellation of kind `files`: each document describes a stored file, whose content is in the chunk store. See [Files](#16-files).

**Chunk.** A piece of a file's content (about 2 MB), stored once however many files contain it, and split into six fragments across the vertices.

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

1. `hexdb.toml`, found through `--config <path>`, then the `HEXDB_CONFIG` variable, then `./hexdb.toml`, then `hexdb.toml` next to the executable, then `hexdb.toml` in the user's HexDB folder (`%LOCALAPPDATA%\HexDB` on Windows, `~/Library/Application Support/HexDB` on macOS, `$XDG_CONFIG_HOME/hexdb` or `~/.config/hexdb` elsewhere; `HEXDB_HOME` overrides it). Without one, built-in defaults apply.
2. `hexdb.local.toml` next to it, if present. Values here override `hexdb.toml`. This is where secrets go; the repository ignores the file.
3. Environment variables named `HEXDB_<SECTION>__<FIELD>` (two underscores), for example `HEXDB_NETWORK__API_ENDPOINT=0.0.0.0:7700`. List settings (`peers`, `trusted_proxies`, `previous_lattice_secrets`, `previous_encryption_keys`) take comma-separated values.
4. Runtime settings saved from the admin UI (`settings.hxe` in the data directory). These apply on top of everything else; see [Runtime settings](#runtime-settings).

Relative paths in the file (`storage.path`, `ui.path`, `plugins.registry`) resolve against the config file's folder.

The one required setting is `storage.encryption_key`: 32 random bytes, base64-encoded, with a `base64:` prefix. Generate one with `hexdb secret` or `echo "base64:$(openssl rand -base64 32)"`. Without the key, the data can't be read. Back it up separately from the data.

`hexdb init` writes a starting configuration: `hexdb.toml` (data in `./data` beside it, and the admin UI that shipped with this copy of HexDB) and `hexdb.local.toml` with a new key, readable by the user only. It writes to the user's HexDB folder, or to `--dir <folder>`, and never replaces a file that exists. `hexdb start` runs it by itself when it finds no configuration and no key in the environment, which is what happens on the first start after installing HexDB.

```toml
# hexdb.local.toml
[storage]
encryption_key = "base64:..."

[network]
lattice_secret = "base64:..."   # optional; see section 19
```

### Starting and stopping

```bash
hexdb init             # create a configuration with a new key (hexdb start does this when there's none)
hexdb start            # in the foreground
hexdb start -s         # in the background, restarted if it crashes; output goes to hexdb.log in the data directory
hexdb stop             # graceful: drains the WAL and flushes to SSTables
hexdb stop --force     # kill a server that won't stop (unflushed writes come back from the WAL)
hexdb health
HEXDB_TOKEN=hxk_... hexdb status
HEXDB_TOKEN=hxk_... hexdb backup        # a consistent backup while running; --list shows them
HEXDB_TOKEN=hxk_... hexdb sql "SELECT COUNT(*) FROM orders"   # see section 10; -p fills ? parameters, --json prints the raw answer
```

`hexdb start` runs the `hexdb_api` binary found next to the CLI (or on `PATH`, or `--server-bin`). The server writes `hexdb.pid` to its data directory: its PID, endpoint, executable, start time and a one-time shutdown token. The file is readable by its owner only. `hexdb stop` posts the token to `POST /shutdown`. `--force` kills the process only if its executable and start time match the file, so a stale file can't kill an unrelated process that reused the PID.

The CLI sends `--token` (or `HEXDB_TOKEN`) only over HTTPS or to this machine (`localhost`, `127.0.0.1`, `::1`): to another host over plain HTTP it refuses, since anyone on the network could read the token. Set `HEXDB_ALLOW_PLAINTEXT=1` for a network you trust.

### The first administrator

With no users, the first start creates the administrator named by `security.admin_login` (default `hexdbadmin`). If `security.admin_password` is empty (recommended), HexDB generates a password, prints it once on the console, and writes it to `initial-admin-password.txt` in the data directory, readable by the owner only. It's never written to the log. Sign in, change the password on the Account page, and delete the file.

### A forgotten password

An administrator resets another user's password, unlocks the account or turns off its MFA on the Users page. When the administrator's own password is lost, reset it from the server's machine:

```bash
hexdb reset-password                    # the user named by security.admin_login
hexdb reset-password --login alice      # another user (login or user ID)
hexdb reset-password --mfa-off          # also turn off MFA (lost authenticator and backup codes)
hexdb --config /etc/hexdb/hexdb.toml reset-password   # the server's configuration, if it isn't found by default
```

Only someone who can write to the server's storage folder (`storage.path`) can reset a password this way. That folder already holds the data and is readable only by the server's account, so the reset grants nothing new, and no network request can start one. Run the CLI as the server's account or as an administrator of the machine. In Docker, run `docker exec -u hexdb <container> hexdb reset-password`; the image's `HEXDB_CONFIG` points the CLI at the server's configuration. In a lattice, run it on the Overseer's machine (users are written there).

**What the reset does.** The user gets the new password; the account is unlocked; its failed sign-ins are cleared (on this hex and lattice-wide); every session it had is signed out; with `--mfa-off`, its authenticator, pending enrollment and backup codes are removed. Roles aren't changed. The server logs a warning and records `user.password_recovered` in the audit trail (actor `-`, client `local`, details `{"mfa_turned_off": true|false}`).

**What the CLI does.**

1. It generates a password: 18 random bytes from the operating system, URL-safe Base64 (24 characters).
2. It deletes any earlier answer (`reset-password.result`), then writes `reset-password.request` into the storage folder. The request holds only the password's Argon2 hash; the password itself is never written anywhere.
3. If the server is running (or its runtime file can't be read, as for a Windows service), it waits up to 20 seconds for `reset-password.result`, prints the outcome and deletes the answer:

   ```text
   The password was reset and the user's sessions signed out.
     login:    admin
     password: 3kQ9...Vw
   Sign in and change it.
   ```

   A refusal (an unknown login, a hex that isn't the Overseer) is printed as `The server refused the reset: <reason>` and exits with status 1.
4. If the server isn't running, or doesn't answer within 20 seconds, the CLI leaves the request in place, prints the password and says it takes effect when the server takes the request (at its next start, or once a starting hex has finished its election). It exits with status 0.

**The files.** Another program on the server's machine can reset a password the same way, without the CLI, by following this protocol. Both files are plain JSON in the storage folder, created with the folder's own permissions (they hold no secret, so the CLI and a server running under another account can each read what the other wrote).

`<storage.path>/reset-password.request`, written by the requester:

```json
{ "login": "admin", "password_hash": "$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>", "turn_off_mfa": false }
```

- `login`: the user's login (case doesn't matter) or user ID. Surrounding spaces are ignored.
- `password_hash`: an Argon2 hash of the new password in PHC string format (`$argon2id$...`, `$argon2i$...` or `$argon2d$...`). Any parameters work: sign-in reads them from the string. HexDB itself uses Argon2id with m=19456, t=2, p=1.
- `turn_off_mfa` (optional, default false).

`<storage.path>/reset-password.result`, written by the server:

```json
{ "login": "admin" }
{ "login": "nobody", "error": "User 'nobody' not found." }
```

- On success, `login` is the user's login as stored, and there's no `error`.
- On failure, `error` says why. The reasons are an unreadable request (`login` is then empty), a `password_hash` that isn't an Argon2 hash, an unknown user, or a hex that isn't the Overseer (the message names the Overseer's address). Nothing is changed on failure.
- The server never deletes the answer. The requester reads it and deletes it. Delete any old answer before writing a new request, so the next answer is the one for that request.

**How the server handles a request.**

- It looks for the request at startup and then every 2 seconds.
- A hex that isn't the Overseer and doesn't yet know which hex is (one still in its first election) leaves the request where it is and takes it later.
- Otherwise it deletes the request first, so a request is applied at most once, then applies it and writes the answer. A request the server can't delete isn't applied: it logs a warning instead.
- A replica that knows the Overseer consumes the request and answers with an error naming it. Write the request on the Overseer's machine.

**Checklist for another program.** Write the request atomically (to a temporary name in the same folder, then rename it), so the server never reads half a file. Then poll for the answer for up to about 20 seconds, read it, and delete it. HexDB never sees the password, so it can't apply its password policy: generate a long random one, or check at least 12 characters, not containing the login. Show or deliver the password only after the answer reports success.

For example, in Python with the `argon2-cffi` package:

```python
import json, os, secrets, time
from argon2 import PasswordHasher

storage = "/var/lib/hexdb"                       # the server's storage.path
password = secrets.token_urlsafe(18)             # 24 characters, as the CLI makes
request = {"login": "admin", "password_hash": PasswordHasher().hash(password), "turn_off_mfa": False}

result = os.path.join(storage, "reset-password.result")
if os.path.exists(result):
    os.remove(result)                            # the next answer is this request's
tmp = os.path.join(storage, "reset-password.request.tmp")
with open(tmp, "w") as f:
    json.dump(request, f)
os.replace(tmp, os.path.join(storage, "reset-password.request"))

for _ in range(80):                              # up to 20 seconds
    if os.path.exists(result):
        with open(result) as f:
            answer = json.load(f)
        os.remove(result)
        if "error" in answer:
            raise SystemExit("refused: " + answer["error"])
        print(answer["login"], password)
        break
    time.sleep(0.25)
else:
    print("no answer yet: the reset applies when the server takes the request")
```

### Docker

The [Dockerfile](Dockerfile) builds an image with the server, the CLI, the admin UI, and Python and Node.js for script functions. HexDB runs as the unprivileged user `hexdb`: the entrypoint starts as root only to give the data volume to `hexdb` (some platforms, Fly.io among them, create volumes owned by root), then drops to it. Started with `--user`, it runs as that user. On Fly.io it sets `network.advertise_host` to the machine's private address (`<machine>.vm.<app>.internal`) unless you set it. Configuration lives in `/etc/hexdb/hexdb.toml` ([docker/hexdb.toml](docker/hexdb.toml)) and data in the volume `/var/lib/hexdb`.

```bash
docker run -p 7700:7700 -v hexdb-data:/var/lib/hexdb \
  -e HEXDB_STORAGE__ENCRYPTION_KEY="base64:..." hexdb
```

The server writes only its volume, so it also runs with a read-only root file system, no Linux capabilities and no privilege escalation; the parser sandbox (with its seccomp filter) works under Docker's default seccomp profile:

```bash
docker run --read-only --tmpfs /tmp --cap-drop ALL --security-opt no-new-privileges \
  -p 7700:7700 -v hexdb-data:/var/lib/hexdb -e HEXDB_STORAGE__ENCRYPTION_KEY="base64:..." hexdb
```

[docker-compose.yml](docker-compose.yml) runs a three-hex lattice (two Harvester candidates and a Replicant) on ports 7700, 7710 and 7720, with those settings.

### As a service

[packaging/systemd/hexdb.service](packaging/systemd/hexdb.service) runs HexDB under systemd. The Debian package (from the APT repository, or built by `cargo deb -p hexdb_api`) installs it, creates a `hexdb` user and a key in `/etc/hexdb/hexdb.local.toml`; start it with `sudo systemctl enable --now hexdb`. Homebrew runs it with `brew services start hexdb`. On Windows, run it as a service from an elevated (administrator) prompt:

```powershell
hexdb --config C:\HexDB\hexdb.toml service install   # starts with Windows; --name, --account, --server-bin
hexdb service start
hexdb service status
hexdb service stop                                    # graceful: flushes to disk first
hexdb service uninstall                               # data and configuration stay
```

The service runs `hexdb_api --service --config <hexdb.toml>` under its own virtual account, `NT SERVICE\HexDB`, not LocalSystem. Install gives that account read access to the configuration file and its `hexdb.local.toml` (only those files, not their folder) and modify access to the data directory (all are private to their owner otherwise); with `--account` you grant access yourself. Its recovery actions restart it 5, 10 and 30 seconds after a crash (or any nonzero exit), resetting after a day. Output goes to `hexdb.log` in the data directory. The CLI's `hexdb stop` can't stop the service (the runtime file is private to the service's account); use `hexdb service stop`. Without a service, `hexdb start -s` runs it in the background, restarted if it crashes, until you sign out. Every way to install HexDB is listed in the README's Installing section, and [packaging/RELEASING.md](packaging/RELEASING.md) explains how each is built.

### On a Raspberry Pi

A Raspberry Pi 3, 4, 5 or Zero 2 W running a 64-bit OS (64-bit Raspberry Pi OS, Ubuntu, Debian) is an ordinary Linux arm64 machine: use the Linux arm64 release archive, the arm64 Debian package or the `linux/arm64` Docker image. CI runs the whole test suite on 64-bit ARM.

On a 32-bit OS (ARMv7), build from source with `cargo build --release --target armv7-unknown-linux-gnueabihf`; there are no 32-bit release builds. It works the same, except that the parser sandbox has its resource limits but no seccomp filter (no 32-bit ARM support in the filter library; `/status` shows `storage.files.parser_sandbox.seccomp: false`).

The defaults assume a few GB of memory. On a board with 1 or 2 GB:

```toml
[memory]
ram_mb = 256              # documents held in memory (default 1024)

[files]
cache_mb = 16             # recently read file content (default 64)
sandbox_memory_mb = 256   # per file being parsed (default 1024); larger images may then get no thumbnail
```

Keep the data directory on an SSD (USB 3) rather than the SD card: the write-ahead log and compaction write steadily, and SD cards are slow and wear out. Thumbnails and AI tagging are the heaviest background work; turn thumbnails off with `files.thumbnails = false` if uploads are slow. Hexes find each other through `network.peers` and by scanning ports on their own machine, not by broadcasting, so on a LAN of Pis list each one's discovery address (`<ip>:7702`) in the others' `network.peers`.

## 4. The admin UI

The admin UI is at `/ui/` on every hex (`/` redirects there). It's built from [hexdb_admin](hexdb_admin) into `hexdb_admin/dist`, which `ui.path` points at. Everything it does goes through the same API as any client, with the signed-in user's permissions. Pages and buttons a user can't use are hidden, and the server refuses them anyway.

**Dashboard** (needs `status`)
- Cards: documents, tessellations, memory and disk against their budgets, operation rates, uptime.
- An activity chart: documents per tessellation, operations per minute, or storage, over 15 minutes to 6 hours.
- Vertex health: a hexagon of the six fragments.
- The lattice card: each hex's role, version, address, replication state and lag, with an arrow on hexes that need an update. Administrators get "Add a hex", which shows the settings a new server needs to join; see [Adding a hex](#adding-a-hex).
- A banner when a newer release is out or the hexes run different versions, naming the hexes behind and linking the release notes; on HexDB Enterprise it has the button for a rolling update, and shows its progress hex by hex (see [Operations](#23-operations)).
- A per-tessellation table. On a replica, a banner names the Overseer. For users with the `maintenance` permission, "Flush to disk" writes unflushed data to SSTables and "Back up" writes a backup (see [Operations](#23-operations)).

**Files.** Browse a files tessellation by folder, as a grid with thumbnails or as a list, or pick a type (audio, images, video, documents, archives) to list those files from every folder below. Upload by dropping files or with the Upload button (large files resume after a dropped connection), and search names, tags and metadata. Audio files have a play button in the grid and the list; they play in a bar at the bottom of the page (one at a time, streamed with Range requests, so long WAV masters start at once and can be seeked). When a search finds nothing, the page looks for close matches instead, so a typo such as "tatoo" still finds "tattooed". Videos without a server thumbnail show a frame the browser decodes itself. Open a file to preview it (images, audio, video), read its metadata, edit its name, folder, tags and description, download or restore earlier versions, or move it to the trash. The Trash view restores or deletes trashed files.

**Queries.** A console for GraphQL and SQL, switched at the top left. GraphQL has schema-aware completion and validation, and variables; SQL has table and column completion, `?` or `$1` parameters (a JSON array), and runs read-only `SELECT`s (see [SQL](#10-sql)). Both have examples written against the tessellations the hex has, history, and JSON or table results. Ctrl+Enter (Cmd+Enter on macOS) runs the query.

**Tessellations**
- Create and delete tessellations and see their sizes.
- The key button opens the indexes dialog. It lists indexes, creates field or text indexes (choosing an analyzer for text), and has a Suggestions section. "From recent queries" runs the rule-based advisor; "Ask Claude" adds the configured AI provider's suggestions. Each suggestion has a button to apply it.
- The document button opens the schema dialog. It shows the version history and any migration in progress, and edits the next version. Check reports compatibility and how many existing documents wouldn't fit (running migration functions on them); Save registers the version. "Roll back to" restores an earlier version's fields.

**Schemas.** Every tessellation with its schema version, and a mark on those with a version waiting to be applied. Pick one to see its version history, migration progress and rollback, and to edit the next version in a JSON editor that marks errors as you type and completes schema keywords. The editor starts from the current version, or on a tessellation without a schema, from one read from its documents (see [Schemas](#13-schemas)). Check reports compatibility and how many existing documents would fit; saving checks first, refuses an incompatible version, and asks before saving one some documents wouldn't fit. **Save, apply later** keeps the version without applying it (writes follow the current version until then); **Save and apply now** registers it and migrates existing documents in the background. A saved version can be applied, edited or discarded. The same editor is in the schema dialog on the Tessellations page.

**Documents.** Pick a tessellation, filter with JSON, sort and page. Files tessellations are listed too: each file is a document whose `meta` holds its extracted metadata, so `{"meta.artist": "Ashes Gone"}` filters files (files are added on the Files page, not here). A side panel creates, edits (with an optional TTL) and deletes documents. The footer shows which index answered the query.

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

**Configuration** (administrators). The lattice configuration (see [Lattice configuration](#lattice-configuration)): the AI settings in a form, secrets as write-only fields that show only whether they are set and a fingerprint, an optional comment saved with the version, each hex's applied version and state, and the history with the keys each version changed and a Roll back button.

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
| 429 | `rate_limited` | Too many failed sign-ins (see `Retry-After`), this month's AI token budget is used up, or the AI provider is rate limiting |
| 502 | `overseer_unreachable` | A replica couldn't forward a write to the Overseer |
| 502 | `ai_provider_error` | The AI provider returned an error (its message, without the key) |
| 503 | `replication_timeout` | Committed, but fewer than `replication.min_acks` replicas confirmed in time |
| 503 | `no_quorum` | The Overseer can't see `replication.quorum` hexes, so it refuses writes |
| 507 | `quota_exceeded` | An upload would take its user past their quota |
| 503 | `content_unavailable` | A file's content can't be read here or fetched from another hex right now |
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

The advisor records each query's shape for index suggestions; see [section 12](#12-indexes-and-the-query-advisor).

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

## 8. GraphQL and gRPC

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

### gRPC

The gRPC service `hexdb.v1.HexDB` is served on the API port, next to REST: requests with `content-type: application/grpc` go to it. It uses HTTP/2, over TLS when `[tls]` is configured (ALPN `h2`) and in clear text otherwise. Generate a client in any language from [hexdb_api/proto/hexdb/v1/hexdb.proto](hexdb_api/proto/hexdb/v1/hexdb.proto).

| RPC | Runs as |
| --- | --- |
| `Get`, `Delete` (`DocumentRef`) | `GET` / `DELETE /{tessellation}/{id}` |
| `Insert`, `Replace`, `Patch` (`Write`) | `POST /{tessellation}`, `PUT` / `PATCH /{tessellation}/{id}` |
| `Query`, `Aggregate` (`Body`) | `POST /{tessellation}/_query`, `/_aggregate` |
| `Sql` (`Body`) | `POST /sql` |
| `Publish` (`Body`) | `POST /streams/{name}/messages` |
| `Call` (`CallRequest`) | any REST call with a JSON body: `{ method, path, json }` |
| `Changes` (server streaming) | `GET /changes/stream` |
| `Subscribe` (server streaming) | `GET /streams/{name}/subscribe` |

```bash
grpcurl -plaintext -import-path hexdb_api/proto -proto hexdb/v1/hexdb.proto \
  -H "authorization: Bearer $KEY" \
  -d '{ "name": "articles", "json": "{\"filter\": {\"published\": true}, \"limit\": 10}" }' \
  localhost:7700 hexdb.v1.HexDB/Query
```

- **Every call is the REST request.** It runs through the same middleware as REST: authentication and its throttling, permissions, row filters and field masks, quotas and limits, the audit log and the request log. On a replica, writes go to the Overseer. The REST documentation applies to every option and reply.
- **JSON in, JSON out.** Documents, query bodies and replies are JSON text (`json` fields), so 64-bit integers keep their precision and new REST options work without a new proto. `Reply.status` is the REST status; a reply that isn't JSON (a file from `Call`) comes back in `body` with `content_type`, up to 64 MB (use REST for larger downloads).
- **Errors** become gRPC statuses with the REST message: 400 `INVALID_ARGUMENT`, 401 `UNAUTHENTICATED`, 403 `PERMISSION_DENIED`, 404 `NOT_FOUND`, 409 `ALREADY_EXISTS`, 410 `OUT_OF_RANGE`, 412 `FAILED_PRECONDITION`, 413, 429 and 507 `RESOURCE_EXHAUSTED`, 503 `UNAVAILABLE`, 504 `DEADLINE_EXCEEDED`. The REST error code (`not_found`, `quota_exceeded`, ...) is in the `hexdb-error` trailer, and `retry-after` is passed on.
- **Credentials** come only from the `authorization` metadata (`Bearer <API key or session token>`). Cookies are ignored, so a web page can't make calls with a visitor's session.
- **Feeds** send `Event { event, id, json }`: `change` events with the sequence number, or `message` events with the offset. Resume from the last `id` with `after`. `lagged` and `unauthorized` end the stream.

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

## 10. SQL

`POST /sql` runs a read-only SQL `SELECT`: joins, subqueries, `UNION`, `WITH`, expressions and functions included. Each table is read through the same query path as the REST API, so indexes, permissions, row filters and field masks apply the same way. BI tools and the [ODBC driver](#odbc) use it; the `hexdb sql` command prints its results as a table.

```bash
curl -X POST http://localhost:7700/sql -H "Authorization: Bearer $HEXDB_API_KEY" -H "Content-Type: application/json" -d '{
  "sql": "SELECT customer, COUNT(*) AS orders, SUM(total) AS revenue FROM orders WHERE status = ? GROUP BY customer HAVING COUNT(*) > 1 ORDER BY revenue DESC LIMIT 10",
  "params": ["paid"] }'
# => { "columns": [{ "name": "customer", "type": "string" }, { "name": "orders", "type": "integer" }, { "name": "revenue", "type": "number" }],
#      "rows": [["ada", 12, 4410.5], ["grace", 9, 3120]],
#      "next": null,
#      "translated": { "tessellation": "orders", "aggregate": { ... }, "having": "COUNT(*) > 1", "limit": 10 } }
```

```bash
HEXDB_TOKEN=... hexdb sql "SELECT customer, total FROM orders WHERE total > ? ORDER BY total DESC LIMIT 5" -p 100
```

**Request.**
- `sql`: one statement.
- `params`: values for `?` placeholders (numbered in order of appearance) or `$1`, `$2`, ... Every parameter must be used.
- `page_size`: rows per response, 1 to 10,000 (default 1,000).
- `cursor`: the previous response's `next`, to get the following page. Send the same statement and parameters with it.

**Response.**
- `columns`: each column's name and a type inferred from the page's values: `boolean`, `integer`, `number`, `string`, `json` (objects, arrays, or mixed types) or `null` (no values).
- `rows`: arrays of values in column order.
- `next`: a cursor, or null on the last page.
- `translated`: how the statement runs. A single-table statement becomes one query or aggregation, shown as its filter, sort and aggregation. Anything else shows `"executor": "general"` and the filter sent with each table read (`reads`). Use it to check which index a statement can use.

**Catalog.** SQL clients list tables and columns before they query:
- `GET /sql/tables` returns the tessellations the caller can read.
- `GET /sql/columns?table=orders` returns each column's `name`, `type`, `nullable` and `source`:
  - `id` comes first;
  - then the current schema's fields (`source: "schema"`; a nested rule such as `dims.width` lists `dims` as a `json` column);
  - then, unless the schema forbids other fields, the fields found in the first 100 documents (`source: "sample"`).
- Hidden fields of the caller's role aren't listed; masked ones are listed as strings.

**Two ways a statement runs.**
- **One query.** A `SELECT` from one tessellation in the shape below becomes a single query or aggregation, run where the data is. It uses indexes for its filter and sort, and pages through results with a cursor, so it handles any table size.
- **The general executor.** Everything else: joins, subqueries, `UNION` / `INTERSECT` / `EXCEPT`, `WITH`, and expressions over fields. Each table is read once through the query path, with the conditions in `WHERE` and `ON` that compare one table's field with a value sent along as that read's filter (so indexes apply). The rest runs in memory: hash joins on equality conditions, then filters, grouping, sorting and limits.
  - One read may return at most `limits.sql_max_rows` documents (100,000). A larger read is refused with a message, never cut short: narrow it with `WHERE`.
  - The same limit, times 100, bounds the work a statement may do (rows read, compared and produced), which stops runaway joins without an equality condition.
  - Pages after the first run the statement again and skip the rows already returned.

The single-query shape:

| Clause | Supported |
| --- | --- |
| `SELECT` | `*`, fields (dotted paths such as `address.city`), values, `AS` aliases, `DISTINCT`; `COUNT(*)`, `COUNT(field)`, `COUNT(DISTINCT field)`, `SUM`, `AVG`, `MIN`, `MAX` |
| `FROM` | One tessellation, with an optional alias (`FROM orders o`, then `o.total`). `SELECT 1` works without `FROM` |
| `WHERE` | A field compared with a value (`=`, `<>`, `<`, `<=`, `>`, `>=`); `AND`, `OR`, `NOT`; `IN`, `NOT IN`; `BETWEEN`; `LIKE` and `NOT LIKE`; `IS [NOT] NULL`; `IS [NOT] TRUE` / `FALSE`; a boolean field on its own |
| `GROUP BY` | Fields, select aliases or positions (`GROUP BY 1`) |
| `HAVING` | Comparisons of group fields and aggregates with values, with `AND`, `OR`, `NOT`, `IN`, `BETWEEN`, `IS NULL` |
| `ORDER BY` | Fields, aliases, aggregates or positions, `ASC` / `DESC` |
| Paging | `LIMIT`, `OFFSET`, `LIMIT offset, count`, `FETCH FIRST n ROWS ONLY`, `TOP n` |
| Similarity | `ORDER BY SIMILARITY(field, 'text') DESC LIMIT k`, or a vector (`'[0.1, ...]'`, or a JSON array parameter); `SIMILARITY(...)` in the select list is the score. Needs a [vector index](#vector-search) on the field; `OFFSET` + `LIMIT` at most 100; no `GROUP BY` or aggregates; single table |

Notes:
- **Fields and values.**
  - `id` is the document ID. `SELECT *` returns `id` and then every field present in the page's documents, in the order they first appear. Metadata fields (`_schema`, `_expires_at`) are left out of `*` but can be selected by name.
  - Identifiers are case-sensitive, like field names. Quote names that aren't plain identifiers: `"order-date"`.
  - Dates are strings in HexDB: `placed_at >= '2026-01-01'` and `DATE '2026-01-01'` both compare as text, which works for ISO 8601 dates.
  - Equality on an array field matches any element: `tags = 'rust'`.
- **NULL rules.** Comparisons follow SQL's rules for NULL:
  - a comparison with NULL matches nothing (use `IS NULL`);
  - `x <> 1`, `NOT (x = 1)` and `x NOT IN (1, 2)` don't match documents where `x` is missing or null;
  - `SUM` of no values is NULL;
  - a missing field reads as NULL.
- **LIKE patterns.**
  - `%` matches any run of characters and `_` one character. `ESCAPE` makes the next character literal: `LIKE 'a!_%' ESCAPE '!'`.
  - Matching is case-sensitive; `ILIKE` ignores case.
  - Patterns with `%` only at the start or end become an index-friendly prefix, suffix or substring filter. Other patterns are checked in the general executor.
- **Aggregates.**
  - They follow [Aggregations](#9-aggregations), including the 100,000-group limit.
  - `HAVING` is evaluated over every group before paging.
  - `COUNT(*)` without `GROUP BY` uses the exact count, so it doesn't read the documents.
- **Access.**
  - Reading needs read permission on the tessellation.
  - A restricted role sees only its rows; its hidden fields come back as NULL and its masked fields masked.
  - Filtering, grouping or sorting by a hidden field is refused with 403, as in the REST API.
- **Replicas.** `POST /sql` is a read, so a replica answers it itself instead of forwarding it to the Overseer.

### Joins, subqueries and expressions

```sql
-- Revenue per customer country, for paid orders, with customers who ordered nothing.
SELECT c.country, COUNT(o.id) AS orders, COALESCE(SUM(o.total + o.shipping), 0) AS revenue
FROM customers c
LEFT JOIN orders o ON o.customer = c.code AND o.status = 'paid'
GROUP BY c.country
HAVING COUNT(o.id) > 0
ORDER BY revenue DESC

-- Customers whose largest order is above their country's average.
WITH by_country AS (SELECT country, AVG(total) AS average FROM orders GROUP BY country)
SELECT name FROM customers c
WHERE (SELECT MAX(total) FROM orders o WHERE o.customer = c.code)
    > (SELECT average FROM by_country b WHERE b.country = c.country)
```

**Joins.**
- `[INNER] JOIN`, `LEFT [OUTER] JOIN`, `RIGHT [OUTER] JOIN`, `FULL [OUTER] JOIN` and `CROSS JOIN`, with `ON` or `USING (field, ...)`, and parenthesized joins. A comma join with its condition in `WHERE` (`FROM a, b WHERE a.x = b.y`) is an inner join.
- Equality conditions between the two sides make a hash join. Other conditions are checked pair by pair, which the work limit bounds.
- Every table in `FROM` needs a distinct name or alias. `alias.field` reads a table's field. An unqualified name reads the only table in `FROM`, or, with several, the one whose document has that field (two having it is an error: qualify it).
- `SELECT *` lists each table's columns in turn; `alias.*` lists one table's.

**Subqueries and set operations.**
- `x IN (SELECT ...)`, `EXISTS (SELECT ...)`, `x > ANY (SELECT ...)` / `ALL (...)`, and a subquery as a value (it must return at most one row and one column).
- Subqueries may refer to the outer query's tables by alias (correlated). Uncorrelated subqueries run once.
- Subqueries in `FROM` need an alias: `FROM (SELECT ...) AS t`, optionally naming its columns, `AS t (a, b)`.
- `WITH name [(columns)] AS (SELECT ...)` defines a table for the statement; each runs once. `WITH RECURSIVE` isn't supported.
- `UNION`, `INTERSECT` and `EXCEPT` (`MINUS`), each with or without `ALL`. Both sides need the same number of columns. A trailing `ORDER BY` names result columns or positions.

**Expressions,** anywhere a value can go (`SELECT`, `WHERE`, `ON`, `GROUP BY`, `HAVING`, `ORDER BY`):
- arithmetic `+ - * / %` (integers stay exact; `7 / 2` is `3.5`; dividing by zero is an error), `||` to join text;
- comparisons between fields, `IS [NOT] DISTINCT FROM`, `CASE`, `CAST(x AS type)` (integer, number, string, boolean, date, timestamp, json; `TRY_CAST` gives NULL instead of an error);
- text: `UPPER`, `LOWER`, `LENGTH`, `SUBSTRING`, `TRIM` / `LTRIM` / `RTRIM`, `CONCAT`, `REPLACE`, `LEFT`, `RIGHT`, `POSITION`, `LOCATE`;
- numbers: `ABS`, `ROUND`, `TRUNCATE`, `FLOOR`, `CEIL`, `MOD`, `POWER`, `SQRT`, `SIGN`, `EXP`, `LOG` (natural), `LOG10`, `PI`;
- NULLs: `COALESCE`, `IFNULL`, `NULLIF`;
- dates (ISO 8601 strings): `EXTRACT(part FROM x)` and `YEAR`, `QUARTER`, `MONTH`, `WEEK`, `DAY`, `DAYOFWEEK` (1 = Sunday), `DAYOFYEAR`, `HOUR`, `MINUTE`, `SECOND`, `DAYNAME`, `MONTHNAME`; `CURRENT_DATE`, `CURRENT_TIME`, `NOW()`.
- Aggregates take expressions (`SUM(price * quantity)`), and `DISTINCT` (`SUM(DISTINCT x)`). `GROUP BY` takes expressions too. In a grouped query, every selected expression must be grouped or inside an aggregate.
- `ORDER BY` takes expressions and `NULLS FIRST` / `NULLS LAST`. NULLs sort first ascending and last descending by default.

**Access.** Each table a statement reads, in joins, subqueries and `WITH` included, needs read permission, and each is read with the caller's row filters, hidden fields and field masks. A hidden field reads as NULL in expressions and a masked one as its masked string, and a filter on either sent with a read is refused with 403. System tessellations can't be read.

**Not supported.** These are refused with a message naming the part:
- window functions (`OVER`), `WITH RECURSIVE`, `NATURAL` joins, `LATERAL`;
- statements other than `SELECT` (SQL is read-only; write through the REST API or transactions).

## 11. Transactions

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

## 12. Indexes and the query advisor

### Indexes

Indexes speed up filtered queries, counts, aggregations and `_update`. They never change results.

| Kind | Covers | Answers |
| --- | --- | --- |
| Field | One field or several (composite), optionally `unique` | Equality, `$in`, ranges and `$startsWith` on the first field; equality on all fields together. Array values index each element |
| Text | One or more string fields, with an analyzer; at most one per tessellation | `$text` |
| Vector | One field holding a vector (an array of numbers), optionally filled from source fields by the embeddings provider | Similarity searches (`near`); see [Vector search](#vector-search) |

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

### Vector search

A vector index finds the documents most similar in meaning to a piece of text or to a vector: semantic search over documents, and retrieval for the AI chat, MCP and A2A. It's an index type, not a separate store, so searches see the same documents, permissions, row filters and field masks as every other query.

```bash
# Vectors you compute yourself (any model): an array of numbers in a field.
curl -X POST http://localhost:7700/tessellations/articles/indexes -H "Content-Type: application/json" \
  -d '{ "kind": "vector", "fields": ["embedding"], "dimensions": 1536 }'

# Or let HexDB embed source fields with the lattice's embeddings provider.
hexdb config set ai.embeddings.provider voyage
hexdb config set ai.embeddings.api_key            # prompts
curl -X POST http://localhost:7700/tessellations/articles/indexes -H "Content-Type: application/json" \
  -d '{ "kind": "vector", "fields": ["embedding"], "source": ["title", "body"] }'

# The 5 articles nearest to a text (or "vector": [...]), among published ones.
curl -X POST http://localhost:7700/articles/_query -H "Content-Type: application/json" \
  -d '{ "filter": { "status": "published" }, "near": { "field": "embedding", "text": "storage engines for edge devices", "k": 5 } }'
```

- **Definition.** `kind: "vector"`, one field, and optionally `dimensions` (1-4096; otherwise the first vector stored sets it), `metric` (`cosine`, the default, or `dot`), `quantization` and `source` (up to 8 fields to embed). A vector is an array of numbers, or an object with one under `vector`. Vectors of the wrong size aren't indexed.
- **Memory.** The index holds 4 bytes per dimension per document, plus the graph (about 200 bytes per document). With `quantization: "int8"` it holds 1 byte per dimension and a 4-byte scale instead, a quarter of the memory (1536 dimensions: 6 KB down to 1.5 KB per document). The ranking is then approximate (recall in tests is over 85% at k = 10 before rescoring), but every result is rescored exactly from the document's own vector, so `_score` and the order of what's returned are exact.
- **Searching.** `POST /{tessellation}/_query` with `near: { field, vector | text, k, min_score }` returns the `k` (1-100, default 10) nearest documents that match `filter`, best first, each with `_score` (cosine similarity, -1 to 1, or the dot product). It takes no `sort`, `limit`, `offset` or `after`; `fields` works. GraphQL has `similar(tessellation, field, text | vector, k, minScore, filter)`, and the AI tools have `similar` (chat, MCP and A2A answer "find notes about ..." by meaning). `_score` is ignored when a document is written back. In SQL: `SELECT title, SIMILARITY(embedding, 'edge storage') AS score FROM articles WHERE status = 'published' ORDER BY SIMILARITY(embedding, 'edge storage') DESC LIMIT 5` (see [SQL](#10-sql)).
- **How it searches.** Up to 2,000 vectors, every vector is scored (exact). Beyond that, an HNSW graph (16 links per vector, 32 on the bottom layer) finds the nearest. The search widens until `k` documents pass the filter and the caller's row filter, or every vector has been considered, so restrictive filters stay correct but cost more. Removed vectors are skipped, and the graph is rebuilt once they outnumber the live ones. Snapshots and startup work as for other indexes.
- **Auto-embedding.** With `source`, a write whose source text changed queues the document. On the Overseer, a background task embeds the queue in batches of 64 and writes `{"vector": [...], "model": "...", "source_hash": "..."}` into the field, as a guarded update: a document changed meanwhile is embedded again. Writes that leave the source text alone cost nothing, and vectors a client writes as plain arrays are kept. Changing `ai.embeddings.model` rebuilds these indexes and re-embeds every document. `/status` shows `embeddings`: the model, documents waiting, and the last error (failures retry every minute). Text is cut to 16,000 characters per document.
- **Searching by text** embeds the text first (`ai.embeddings.*`). The asking user's tokens count against `ai.embeddings.user_daily_tokens` (200,000 a day by default), and every embedding counts against the monthly budget (`ai.monthly_token_budget`, as `embeddings` and `embeddings.query`).
- **Providers.** `ai.embeddings.provider` is `same` (the chat provider and its key), `openai`, `voyage`, `azure_openai`, `github`, `openai_compatible` (Ollama, LM Studio, vLLM) or `none`. Anthropic has no embeddings API; with Claude for chat, use Voyage AI (Anthropic's recommended embeddings provider). Default models: `text-embedding-3-small` (OpenAI, GitHub Models) and `voyage-3.5` (Voyage). Others need `ai.embeddings.model` and, for Azure and OpenAI-compatible servers, `ai.embeddings.base_url`.
- **Files.** In a files tessellation, embed `meta.text` (extracted body text) with the name and description to search documents by meaning; see [Finding files](#finding-files).
- **Privacy.** Auto-embedding sends the source fields' text to the embeddings provider, so choose source fields accordingly. A vector field covered by a mask is sealed like any masked field and not indexed, and a caller can't search a field hidden from or masked for them.

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

With `?ai=true` and an AI provider configured (see [AI](#ai)), HexDB also asks the model for a second opinion and returns its suggestions separately (`source: "ai"`). It sends:
- the query shapes;
- the existing indexes;
- the field names and types of a sample of documents.

It never sends field values.

Before 1.6 the advisor read an Anthropic key from the environment variable named by `[ai] api_key_env` in hexdb.toml. That still works while `ai.provider` is `none`, for the advisor only; set the provider in the lattice configuration instead.

Query statistics are kept per hex and saved with the metrics history (`query-stats.hxe`), so they survive restarts. Shapes not seen for 30 days are dropped. Needs `manage` on the tessellation.

## 13. Schemas

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
- `tokenize`: `one_way` or `vault` (string fields only, without `enum`): the value is replaced by a token on write. See [Tokenization](#tokenization).

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

**Starting from the documents.** `GET /tessellations/{name}/schemas/infer` reads up to 1,000 of a tessellation's documents and returns a first schema they all fit: every top-level field with the one type its values share (`number` for a mix of integers and other numbers, `any` for anything more mixed), `nullable` where a null was seen, nothing required, and other fields allowed. Registered as it is, it changes no write that works today. `present` counts the documents with each field, to decide which to make required. The admin UI's schema editor starts from it on a tessellation without a schema, and from the current version otherwise.

**Saving a version to apply later.** `PUT /tessellations/{name}/schemas/pending`, with the same body as registering a version, checks it the same way and keeps it without applying it: writes keep following the current version and no migration starts. Saving again replaces it. `GET .../schemas` shows it as `pending`, with the version number it would get and, if a version registered since makes it no longer fit, why. `POST .../schemas/pending/apply` registers it and starts its migration (its migration functions run as whoever applies it); `DELETE .../schemas/pending` discards it. The admin UI's Schemas page does the same with **Save and apply now** or **Save, apply later**; it checks the version first, refuses one that isn't compatible, and asks before saving one that some existing documents wouldn't fit.

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

## 14. Change data

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
websocat "ws://localhost:7700/changes/ws?after=830" -H "Authorization: Bearer $KEY"   # the same over a WebSocket
```

SSE clients resume after a reconnect from the `Last-Event-ID` header. WebSocket clients pass the last `id` they saw as `after`. Each WebSocket text frame is one event, `{"event": "change", "id": "831", "data": { ...the change... }}`; the server pings every 15 seconds and ignores anything the client sends but a close. The `lagged` and `unauthorized` events end the feed in both forms. A WebSocket signed in with the session cookie must come from the server's own pages (its `Origin` must match), so another site can't open one with a visitor's cookie; API keys and bearer tokens have no such check. A user sees only changes to tessellations they can read; system tessellations are left out. GraphQL: `changes(after, tessellation, limit) { changes lastSeq }`.

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

## 15. Streams

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

# Live, as Server-Sent Events, or over a WebSocket ({"event": "message", "id": <offset>, "data"} frames)
curl -N "http://localhost:7700/streams/order-events/subscribe?group=billing"
websocat "ws://localhost:7700/streams/order-events/ws?group=billing" -H "Authorization: Bearer $KEY"
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

### Queues

A stream gives every consumer group every message. A queue gives each message to one consumer at a time, for work that should be done once: a consumer receives messages, each leased to it for the queue's visibility timeout, and acknowledges each when it's done. A message that isn't acknowledged in time comes back for another consumer; after `max_receives` tries it moves to the queue's dead-letter queue (or is dropped and logged without one).

```bash
curl -X POST http://localhost:7700/queues -d '{ "name": "dead" }'
curl -X POST http://localhost:7700/queues -d '{ "name": "thumbnails", "visibility_seconds": 60, "max_receives": 5, "dead_letter": "dead", "retention_hours": 96 }'
curl -X POST http://localhost:7700/queues/thumbnails/messages -d '{ "messages": [{ "body": { "file": "a.jpg" } }, { "body": "later", "delay_seconds": 300 }, { "body": "once", "dedupe_key": "job-42" }] }'
curl -X POST http://localhost:7700/queues/thumbnails/receive -d '{ "max": 10, "wait_seconds": 20 }'
# -> { "messages": [{ "id", "receipt", "body", "receives", "sent_at" }] }
curl -X POST http://localhost:7700/queues/thumbnails/ack -d '{ "receipts": ["01J...abc"] }'                  # done: deleted
curl -X POST http://localhost:7700/queues/thumbnails/nack -d '{ "receipts": ["..."], "delay_seconds": 30 }'  # give it back
curl -X POST http://localhost:7700/queues/thumbnails/extend -d '{ "receipts": ["..."], "visibility_seconds": 120 }'
curl http://localhost:7700/queues/thumbnails      # configuration and counts: ready, in_flight, delayed
```

- **Receiving** leases up to `max` (100) ready messages; `visibility_seconds` overrides the queue's timeout for this call, and `wait_seconds` (up to 20) waits for messages to arrive (long polling).
- **Receipts** are new for every lease. Acknowledging, releasing or extending with a receipt whose lease ran out (the message went to another consumer) does nothing and counts as `stale`.
- **Sending** takes up to 100 messages, each a JSON `body` of at most 256 KB, optionally delayed (`delay_seconds`, up to 15 minutes) or de-duplicated (a `dedupe_key` sent again within 5 minutes is skipped; its ID comes back `null`).
- **Delivery** is at least once: a consumer that dies before acknowledging leaves the message to come back, so make handlers idempotent. Order is roughly first in, first out, not strict.
- **Permissions** use `queue:<name>` in role grants: `write` to send, `read` to receive, acknowledge, release and extend, `manage` to create, change and delete. Changes are audited (`queue.create`, `queue.update`, `queue.delete`).
- **Storage.** Messages are documents in `_queue_<name>` (encrypted, replicated, kept for `retention_hours`). Every queue call but the `GET`s changes messages, so it runs on the Overseer (replicas forward them); the Overseer keeps an index of when each message is next ready in memory and rebuilds it from the documents after a failover. The admin UI's Queues page shows each queue's counts and sends test messages.

## 16. Files

A files tessellation stores files of any kind: photos, audio, video, Photoshop and Illustrator files, office documents, archives. Each file is a document describing it, so everything that works on documents works on files: filters, full-text search, aggregations, SQL, GraphQL, roles, row filters, field masks, the change feed and replication. The bytes live in a separate chunk store, compressed when that helps, encrypted, split across the six vertices, and copied to every hex in the lattice.

HexDB reads what each file says about itself (EXIF, IPTC, XMP, ID3, Vorbis comments, QuickTime, Matroska, Broadcast WAV, PDF and Office properties, and more) into the file's document, so files can be found by camera, artist, place, date or the words inside them.

### Creating a files tessellation

```bash
curl -X POST http://localhost:7700/tessellations -H "Content-Type: application/json" \
  -d '{ "name": "media", "kind": "files" }'
```

It starts with two indexes: `path_name` (unique on folder and name) and `search` (a text index over `name`, `path`, `tags`, `description` and `meta`). The admin UI's Tessellations page creates one with "Holds: Files"; its Files page browses, uploads, previews and searches them.

Permissions: `read` lists, searches and downloads; `write` edits names, folders, tags and descriptions; `upload`, `delete` and `restore` add, trash and restore files. Creating a files tessellation needs `write` and `upload`. A role's `folders` restriction limits any of these to some folders; see [Row filters and field masks](#row-filters-and-field-masks).

### Uploading

```bash
curl -X PUT --data-binary @IMG_0042.jpg \
  "http://localhost:7700/media/_files/2024/Iceland/IMG_0042.jpg?modified=2024-06-01T12:30:45Z"
```

- The path after `_files/` is the folder and the file name. `201` creates a file; `200` adds a version to the file already at that path. Uploading content identical to the current version stores nothing and answers `200` with `X-HexDB-Unchanged: true`.
- `?modified=` and `?created=` take RFC 3339 times or epoch milliseconds (default: now).
- The content type is taken from the file's first bytes, then its name's extension, then the `Content-Type` header, then `application/octet-stream`.
- Names are 1-255 bytes without `/`, `\`, control characters, `.` or `..`. Folders are stored as `/a/b/` (at most 64 levels and 1,024 bytes); `/` and `\` both separate them.
- Uploads stream; the request size and time limits don't apply. `files.max_file_mb` and `storage.disk_mb` do (413 and 507).
- Uploads sent to a replica are streamed on to the Overseer.

**Resumable uploads**, for large files and unreliable connections:

| Request | Does |
| --- | --- |
| `POST /media/_uploads` `{"path", "size"?, "content_type"?, "modified"?, "created"?}` | Starts a session: `{"id", "path", "offset": 0, "size", "expires_at"}` |
| `PATCH /media/_uploads/{id}?offset=N` (or `Upload-Offset: N`) | Sends the next part, raw, starting at byte N. A part at the wrong offset gets 409 |
| `GET /media/_uploads/{id}` | Where the session is, to resume after a failure |
| `POST /media/_uploads/{id}/complete` | Creates the file (or its new version), as an upload would |
| `DELETE /media/_uploads/{id}` | Abandons the session |

Sessions expire after a day. Only the user who started one (or an administrator) can continue it. The admin UI, the CLI and the drivers use sessions in 8 MB parts for files of 16 MB and more.

### The file document

```json
{
  "id": "01JZ...", "name": "IMG_0042.jpg", "path": "/2024/Iceland/",
  "size": 4823112, "content_type": "image/jpeg", "sha256": "9f2c...",
  "version": 2, "versions": [{ "version": 1, "size": 4810022, "sha256": "...", "uploaded_at": "...", "uploaded_by": "ada", ... }],
  "created_at": "...", "modified_at": "...", "uploaded_at": "...", "uploaded_by": "ada",
  "content": { "manifest": "<chunk ID>", "chunks": 3, "compressed": false },
  "thumbnail": { "chunk": "<chunk ID>", "width": 320, "height": 213, "content_type": "image/jpeg" },
  "meta": { "title": "Glacier Lagoon", "taken_at": "2024-06-01T12:30:45", "width": 6240, "height": 4160,
            "camera": { "make": "FUJIFILM", "model": "X-T5" }, "gps": { "lat": 64.0784, "lon": -16.2306 },
            "keywords": ["iceland", "ice"], "exif": { ... }, "iptc": { ... }, "xmp": { ... } },
  "extract": { "status": "done", "version": 1, "extractors": ["exif", "iptc", "image", "xmp", "thumbnail"], "errors": [] },
  "tags": ["holiday"], "description": "..."
}
```

- `size`, `content_type`, `sha256`, `content`, `version`, `versions`, `created_at`, `modified_at`, `uploaded_at`, `uploaded_by`, `meta`, `extract`, `thumbnail` and `ai` (AI tags; see [AI](#ai)) are written only by HexDB. Document writes (PUT, PATCH, bulk, `_update`, transactions, GraphQL) keep their stored values whatever the request says.
- `name` and `path` can be changed with a PATCH: that renames or moves the file. A clash with another file's folder and name is a 409.
- Any other field (`tags`, `description`, your own) is yours.
- Files can't be created with `POST /media`; they're uploaded.

### Downloading

```bash
curl -O -J http://localhost:7700/media/01JZ.../content
curl http://localhost:7700/media/_files/2024/Iceland/IMG_0042.jpg -o IMG_0042.jpg
curl -H "Range: bytes=0-1023" http://localhost:7700/media/01JZ.../content
```

- `Range` (one range; several ranges get the whole file), `If-Range`, `ETag` (the SHA-256) with `If-None-Match` (304), `Last-Modified`, and `HEAD` work as browsers and media players expect, so audio and video can seek.
- `?version=N` reads an earlier version.
- `Content-Disposition` is `inline` for images (except SVG), audio, video, PDF and plain text, and `attachment` for everything else or with `?download=true`. With `X-Content-Type-Options: nosniff` and the API's `default-src 'none'` Content Security Policy, a stored HTML or SVG file can't run scripts in a browser.
- `GET /media/{id}/thumbnail` returns a file's thumbnail (404 if it has none).
- `GET /media/{id}` returns the document, as for any tessellation.

### Finding files

Searches run against the documents; content is read only when downloaded.

```bash
# By any field, including extracted metadata
curl -X POST http://localhost:7700/media/_query -H "Content-Type: application/json" \
  -d '{ "filter": { "meta.camera.model": "X-T5", "meta.taken_at": { "$gte": "2024-06-01" } }, "fields": ["name", "path", "meta.gps"] }'

# Every word of the query in the name, folders, tags, description or any metadata
curl -X POST http://localhost:7700/media/_query -d '{ "filter": { "$text": "glacier lagoon" } }' -H "Content-Type: application/json"

# One folder's files, and the folders under it
curl -X POST http://localhost:7700/media/_query -d '{ "filter": { "path": "/2024/" }, "sort": "name" }' -H "Content-Type: application/json"
curl -X POST http://localhost:7700/media/_aggregate -H "Content-Type: application/json" \
  -d '{ "filter": { "path": { "$startsWith": "/2024/" } }, "group_by": ["path"], "aggregates": { "files": { "$count": "*" }, "bytes": { "$sum": "size" } } }'

# SQL and GraphQL (dotted paths go in GraphQL variables, since literals can't contain dots)
curl -X POST http://localhost:7700/sql -d '{ "sql": "SELECT name, meta.title FROM media WHERE meta.artist = '\''Miles Davis'\''" }' -H "Content-Type: application/json"
```

Indexes on metadata fields (for example `meta.camera.model` or `meta.artist`) make such filters fast, as for any field.

**By meaning.** A [vector index](#vector-search) that embeds the extracted text finds files about something, whatever words they use. PDFs, office documents, text files and anything else with body text have it in `meta.text`. Extraction writes it after the upload, and that write queues the file for embedding, so a new file becomes searchable once both have run:

```bash
curl -X POST http://localhost:7700/tessellations/media/indexes -H "Content-Type: application/json"   -d '{ "kind": "vector", "fields": ["embedding"], "source": ["name", "description", "meta.title", "meta.text", "ai.description"] }'
curl -X POST http://localhost:7700/media/_query -H "Content-Type: application/json"   -d '{ "filter": { "path": { "$startsWith": "/contracts/" } }, "near": { "field": "embedding", "text": "termination notice period", "k": 5 }, "fields": ["name", "path"] }'
```

Images without text are found through what describes them: their name, tags, description, metadata and AI tags (`ai.description`). Their pixels aren't embedded.

### Metadata extraction

After each upload, new version or version restore, the Overseer reads the file in the background: `extract.status` is `pending` until `meta` (and `thumbnail`) are written, then `done`, with the extractors that found something and any errors. A file that can't be read is still stored and searchable by name, folder, type, size, hash and dates. `POST /media/_reextract` (or `?id=` for one file) reads files again. Files that are pending, or were read by an older version of the extractors, are queued again when a hex becomes the Overseer.

**Common fields.** The first source that has a value wins; format-specific tags are read before XMP:

| Field | From |
| --- | --- |
| `title`, `description`, `subject`, `author`, `keywords`, `copyright`, `language`, `publisher`, `creator_tool`, `rating` | Any family below |
| `artist`, `album`, `album_artist`, `composer`, `genre`, `year`, `track`, `disc`, `comment`, `label`, `isrc`, `bpm` | Audio and video tags |
| `taken_at`, `recorded_at` | EXIF, IPTC, XMP, QuickTime, Broadcast WAV |
| `duration_ms`, `bitrate_kbps`, `sample_rate`, `channels`, `bit_depth`, `codec` | Audio and video streams |
| `width`, `height`, `orientation`, `color_mode`, `layers` | Images, video, Photoshop, DICOM |
| `color_profile` | The embedded ICC profile's description ("sRGB IEC61966-2.1", "Display P3", ...) |
| `camera` (`make`, `model`, `lens`, `serial`), `exposure` (`f_number`, `exposure_time`, `focal_length`, `iso`, ...) | EXIF, XMP, QuickTime, GoPro telemetry, DICOM |
| `gps` (`lat`, `lon`, `alt`), `location` (`city`, `state`, `country`, `sublocation`) | EXIF, QuickTime, GoPro telemetry (the first fix), IPTC, XMP |
| `pages`, `words`, `lines`, `slides` | PDF, Office, OpenDocument, text |
| `text` | The start of the file's text (plain text, Markdown, PDF, Word, PowerPoint, Excel, OpenDocument, SVG, archive listings), for `$text` |

**Tag families.** Each is kept under its own key in `meta`:

| Key | Read from |
| --- | --- |
| `exif` (with `GPS`) | JPEG, TIFF, HEIC/HEIF, AVIF, PNG, WebP, and camera RAW (CR2, CR3, NEF, ARW, DNG, RAF, ORF, RW2, PEF, ...) |
| `iptc` | IPTC-IIM in JPEG (Photoshop APP13), PSD, TIFF |
| `xmp` | Any file with an XMP packet: JPEG, TIFF, PNG, PSD, AI, PDF, MP4, InDesign, SVG (RDF) and more |
| `png` | PNG `tEXt`, `zTXt` and `iTXt` chunks |
| `psd` | Photoshop documents (PSD, PSB): size, channels, depth, color mode, layers, writer |
| `image` | Image format and dimensions (most formats) |
| `id3v1`, `id3v2`, `ape`, `vorbis`, `mp4`, `riff_info`, `aiff` | MP3, AAC, FLAC, Ogg Vorbis/Opus/Speex, M4A/MP4, WAV, AIFF, APE, WavPack, Musepack |
| `bext`, `ixml` | Broadcast WAV (BWF) description, originator, time reference, loudness, coding history; iXML from field recorders (project, scene, take, notes, track names) |
| `asf` | WMA and WMV |
| `quicktime` | MOV, MP4, 3GP and Matroska/WebM track metadata (make, model, creation date, duration, size, ISO 6709 location) |
| `matroska` | MKV and WebM Info, Tracks and Tags |
| `mpeg7` | MPEG-7 descriptions (titles, people, keywords, annotations, genres), in XML files or embedded |
| `pdf` | PDF and Illustrator (AI) Info dictionaries and page counts, and `text_pages`, the pages that had text (files up to 64 MB). Body text goes to `text`, page by page; PDFs that need a password and scanned pages (images of text) have none |
| `icc` | Embedded ICC color profiles in JPEG, PNG, WebP, TIFF, PSD, HEIC and AVIF: description, copyright, manufacturer, model, version, device class, color space, connection space, rendering intent, creation date |
| `gpmf` | GoPro telemetry in MP4: the camera (`device`), the sensors it logged (accelerometer, gyroscope, GPS, ...), and from GPS (GPS5, or GPS9 on HERO11 and later) `gps` with the fix count, start and end times, distance, top speed, altitude range and a `track` of up to 500 `[lat, lon, alt]` points |
| `eps` | EPS/PostScript DSC comments |
| `office`, `odf`, `epub` | Word, Excel and PowerPoint properties; OpenDocument meta; EPUB package metadata |
| `font` | TrueType and OpenType name tables |
| `svg` | SVG size, title and description |
| `dicom` | Non-identifying DICOM tags (modality, study and series description, manufacturer, size). Patient names and IDs are never read |
| `archive` | ZIP, JAR, APK, TAR and `.tar.gz` entry names and counts |
| `c2pa` | Whether a file carries C2PA content credentials, and its claim generator |

**Limits.** Extraction reads at most `files.extract_max_mb` (64) of a file and stops after `files.extract_timeout_seconds` (30). Each extractor runs on its own: one that fails or panics adds a line to `extract.errors` and the others still run. Strings are cut at 4,096 bytes, arrays and objects at 500 items; `meta.text` is at most `files.text_kb` (64) KB. A `meta` larger than 256 KB loses its largest tag families first (listed in `meta.dropped`). Binary data (embedded thumbnails, maker notes, cover art) is left out.

**The parser sandbox.** Files come from users, and file parsers are where a hostile file does harm, so extraction, thumbnails and video frames are made in a separate worker process (`files.sandbox`, on by default): the server's own executable, started for one file at a time. The worker never sees the store, the keys or the network. It asks the hex for byte ranges of the file over a pipe, and the hex reads them from the encrypted store, enforcing the read budget and deadline itself. The worker gets an empty environment and no handles but its pipes, and is killed at the deadline or if the hex exits. On Linux it also runs under resource limits (`files.sandbox_memory_mb`, 1024, of memory, CPU seconds, no file writes, few descriptors) and a seccomp filter that allows only memory, thread, clock and pipe system calls: opening files, sockets and starting programs fail. On macOS the resource limits apply (macOS doesn't enforce the memory one). On Windows the worker runs in its own job object with a memory limit, a CPU-time limit, no child processes and UI restrictions. `/status` shows `storage.files.parser_sandbox` (runs, failures, workers stopped at the deadline). Turning the sandbox off parses files in the server process, as before 2.0.

Not read yet: text in scanned PDFs and images (no OCR; the AI can read them, see [AI](#ai)), frames of video that isn't H.264.

Extractor version 3 (2.0) added PDF body text, ICC profiles and GoPro telemetry: files read by an older version are read again when a hex becomes the Overseer. AI results are kept, since the content hasn't changed.

### Thumbnails

Images that the server can decode (JPEG, PNG, GIF, WebP, BMP, TIFF) and Photoshop files (their embedded preview) get a thumbnail during extraction: at most `files.thumbnail_px` (320) on the longest side, never enlarged, with EXIF orientation applied; JPEG, or PNG when the image has transparency. Turn them off with `files.thumbnails = false`.

So do H.264 videos in MP4, MOV, M4V and 3GP files, uploaded any way (API, CLI, drivers or the admin UI). HexDB reads the file's index, picks the keyframe about a third of the way in, past intros and fades from black, and decodes only that frame, within the same read budget and deadline as extraction; the video's rotation is applied. Fragmented MP4 (browser recordings, DASH) uses its first frame. The decoder is Cisco's OpenH264, built into the binary (see THIRD-PARTY-NOTICES.md); HEVC, VP9 and AV1 videos get no server thumbnail, and the admin UI shows a frame the browser decodes instead. Building with `cargo build --no-default-features` leaves the decoder out. Files read by an older version of the extractors, including videos uploaded before this, are read again when a hex becomes the Overseer.

### Versions

Uploading to a path that has a file makes a new version. The previous one moves to `versions` (newest first), up to `files.max_versions` (10) and, with `files.version_days` set, no older than that. Versions share unchanged chunks, so a small edit to a large file stores little.

```bash
curl http://localhost:7700/media/01JZ.../versions                      # {"current", "versions"}
curl -O -J "http://localhost:7700/media/01JZ.../content?version=1"
curl -X POST http://localhost:7700/media/01JZ.../versions/1/restore    # version 1's content becomes the next version
```

A restore makes a new version (with `restored_from`), so the version it replaced is kept and the rollback can itself be undone.

### Trash

Deleting a file's document (`DELETE /media/{id}`, or a delete in a transaction or GraphQL) moves it to the trash, with `from`, `trashed_at`, `trashed_by` and an expiry `files.trash_days` (30) later. Expired files are gone for good.

| Request | Does |
| --- | --- |
| `GET /media/_trash` | Trashed files from `media`: `{"documents", "total"}` |
| `POST /media/_trash/{id}/restore` | Puts the file back. If its folder and name are taken: 409; add `?name=` or `?path=` |
| `DELETE /media/_trash/{id}` | Deletes it for good |
| `DELETE /media/_trash` | Empties the trash |

Dropping a files tessellation drops its trashed files too.

### Upload quotas

Administrators can limit how much each user keeps in files: `quotas.default_mb` and `quotas.default_files` in the lattice configuration apply to every user without their own limit (0: no limit; administrators get only a limit of their own), and a user's `quota_mb` and `quota_files` (the Users page, or `PATCH /users/<login>`; `null` returns them to the default, 0 means no limit) override it. A user's usage is what their uploads keep stored: every file whose current version they uploaded, in any files tessellation and in the trash, with its earlier versions. An upload that would go past a limit is refused with 507 `quota_exceeded`: before it starts when its size is known (a resumable upload's `size`, a PUT's `Content-Length`), and again when it's committed. A new version counts its bytes but isn't a new file. Moving files to the trash doesn't free room; purging them does. `GET /auth/quota` shows your limits and usage, and the Files page shows a bar when a limit applies.

### Sharing links

A sharing link gives people without an account one file, or the files under one folder: `https://<hex>/_s/<code>`. A browser opening it gets a page with a preview and a Download button (and the folder's files, for a folder link); scripts get JSON. Users with the `share` permission make links on the Files page (Share, or Share folder) or with the API, and see them under Shared links, with their downloads, where they can copy or delete them. Administrators can see everyone's.

```bash
curl -X POST http://localhost:7700/shares -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d '{ "tessellation": "media", "file_id": "01J...", "expires_in_days": 7, "password": "a long passphrase", "max_downloads": 10, "note": "for the press" }'
# -> { "code": "PVETfyjH", "path": "/_s/PVETfyjH", ... }   ("folder": "/art/" instead of file_id for a folder)
curl http://localhost:7700/_s/PVETfyjH                      # what the link is (no account needed)
curl -X POST http://localhost:7700/_s/PVETfyjH/unlock -d '{ "password": "..." }'   # -> { "token" }, valid for an hour
curl -o cover.png "http://localhost:7700/_s/PVETfyjH/content?download=true"         # &t=<token>; &file=<id> in a folder link
curl http://localhost:7700/_s/<code>/files                  # a folder link's files
curl -X DELETE http://localhost:7700/shares/PVETfyjH -H "Authorization: Bearer $KEY"
```

**Codes: MAR-62.** Link codes are made with MAR-62, an algorithm by HexDB's author first used for the gotoit.us link shortener: an HMAC-SHA256 over the time (in nanoseconds), a random nonce and what is linked (the tessellation and the file or folder), whose first 47 bits are written in Base62 (0-9, A-Z, a-z) and padded to 8 characters. The HMAC key is derived from the lattice key, so codes can't be predicted without it, and every hex resolves every link. A code is also its link's document ID in the `_shares` system tessellation (the 47-bit value), so a lookup is one read; a code already in use is drawn again with a new nonce.

**What a link can reach.** Only what it names: one file, or the files under one folder, as its maker sees them. A link acts as its maker, resolved on every request, so their read grant, row filters and folder limits apply, and the link stops working when they lose the `share` permission or access to the tessellation, or their account is locked or removed.

**Limits.** Links expire: `sharing.default_days` (7) unless the maker says otherwise, at most `sharing.max_days` (30; 0 lets links never expire). A link can need a password (stored as an Argon2 hash; `sharing.require_password` makes it mandatory), and allow a number of downloads (a download is a `GET` of the content from its start; a thumbnail isn't one). `sharing.enabled` turns links off everywhere.

**Guessing and leaking.** Unknown, expired, used-up and deleted codes all answer the same 404, and they and wrong passwords count toward a lockout per client address (the sign-in limits), so codes can't be found by trying them. Responses carry `X-Robots-Tag: noindex`, `Referrer-Policy: no-referrer` (the code never leaves in a `Referer`), `nosniff` and the download CSP; HTML and SVG are always downloads. Each link's creation, downloads (with the client address), wrong passwords and deletion are audited (`share.create`, `share.download`, `share.unlock`, `share.delete`). Anyone who has a link can pass it on, so prefer passwords and short expiries for anything sensitive.

### How file content is stored

1. **Chunks.** A file is cut into content-defined chunks (FastCDC: 512 KB to 8 MB, 2 MB on average), so an edit changes only the chunks around it. A chunk's ID is a keyed BLAKE3 hash of its content, keyed with a random content key kept in the catalog, so identical content is stored once across all files, versions and tessellations, while the ID doesn't reveal a known file's plain hash.
2. **Compression.** Images, audio and video, lossless or not (WAV, AIFF, FLAC, TIFF, RAW, PSD, JPEG, MP3, MP4, ...), and already-compressed formats (ZIP, gzip, 7z, Office files, EPUB, ...) are stored byte for byte. Other chunks are compressed with Zstandard when that saves at least 5%. Zstandard is lossless either way.
3. **Encryption.** Each chunk is encrypted with AES-256-GCM under the storage key, with its ID as authenticated data.
4. **Vertices.** The encrypted chunk is split with the hex's Reed-Solomon code into four data and two parity fragments, and fragment *n* is appended to vertex *n*'s pack file, with a BLAKE3 hash. Any two vertices can lose or corrupt a chunk's fragments and it's still read: the missing fragments are rebuilt and written back. `files.vertex_paths` puts the six vertices in six directories, for example on six disks, so a chunk survives losing any two of them.
5. **Durability.** A file's chunks are written and fsynced before its document commits, so every file in the index has its content. A crash mid-upload leaves unreferenced chunks, which garbage collection removes.
6. **Manifest.** A file's chunk list is itself stored as a chunk, referenced by `content.manifest`.

**Garbage collection** runs with each compaction (`storage.compaction_frequency`, or `POST /compact`, whose answer includes `chunks`): chunks no file, version, trashed file or upload session references are removed, packs that are mostly dead are rewritten, and chunks still under a previous storage key are re-encrypted. Chunks of uploads in progress are kept.

**Scrubbing** verifies every fragment every `files.scrub_hours` (24), or on `POST /scrub` (needs `maintenance`), and rewrites bad ones. A chunk that can't be rebuilt from this hex's vertices is copied from another hex.

**Memory.** The chunk index takes about 80 MB per TB stored. Recently read chunks are cached in `files.cache_mb` (64). File content counts against `storage.disk_mb`: with parity, about 1.5 times the (compressed) content.

`/status` reports `storage.files`: chunks, content and stored bytes, pack files, degraded chunks, chunks on old keys, chunks copied from other hexes, and per-vertex bytes, corrupt fragments found and repairs.

### Files in a lattice

- Replicas create files tessellations as files tessellations, and use the Overseer's content key (sent sealed with the lattice transfer key) so a chunk has the same ID on every hex.
- As file documents arrive, a replica copies the chunks it's missing from the Overseer (four at a time), and checks every file after a full sync and every 10 minutes.
- A read of content a replica doesn't have yet (or can't rebuild) is fetched from another hex and kept.
- Chunks travel sealed with AES-256-GCM under a key derived from the lattice key, bound to their ID, so they stay secret without TLS. `GET /lattice/chunks/{id}` needs a lattice signature and is served by every hex, so hexes repair each other.
- Uploads and resumable parts sent to a replica are streamed to the Overseer. Metadata extraction runs on the Overseer; replicas receive the results.

### Command line

```bash
export HEXDB_TOKEN=hxk_...
hexdb put media IMG_0042.jpg IMG_0043.jpg --to /2024/Iceland   # files of 16 MB and more go in resumable parts
hexdb put media ./Pictures -r --to /photos                     # a folder, keeping its structure
hexdb ls media /2024                                           # subfolders and files
hexdb ls media --search "glacier lagoon"
hexdb get media 2024/Iceland/IMG_0042.jpg -o ~/Downloads       # by path or ID; --version N for an earlier one
```

## 17. Functions, schedules and triggers

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

**Permissions.** A function always runs with the permissions of whoever runs it: a query needs read access to its tessellation, a transaction whatever its operations need. Anyone signed in can run a function and see its name, description and parameters; only administrators see its query, operations or script code, and only they create, change or delete functions, because scripts run code on the server.

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

## 18. Plugins

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
| [sources/s3-files](plugins/sources/s3-files) | source | Copies files from S3 or an S3-compatible store (R2, B2, Wasabi, MinIO) into a files tessellation |
| [sources/azure-files](plugins/sources/azure-files) | source | The same from an Azure Blob Storage container |
| [sources/gdrive-files](plugins/sources/gdrive-files) | source | The same from a Google Drive folder (read-only access) |
| [sources/dropbox-files](plugins/sources/dropbox-files) | source | The same from a Dropbox folder (read-only access) |
| [enrichers/ai-keywords](plugins/enrichers/ai-keywords) | stream | Adds keywords to new documents with the lattice's AI provider |

### Importing files

The four `*-files` source plugins copy files you already have elsewhere into a files tessellation, where they're read, thumbnailed, tagged and searchable like any upload. Folders are kept: an object `photos/2024/a.jpg` under the source's prefix or folder becomes `TARGET/TARGET_FOLDER/photos/2024/a.jpg`. Each file is copied once per version at the source (an S3 or Azure ETag, a Drive checksum, a Dropbox revision); a file that changes there comes in as a new version. What's been copied is remembered in a state file in the plugin's folder, so restarts don't copy again. `POLL_SECONDS` sets how often each looks for new files (0: once).

Setting one up:

1. `npm install` in the plugin's folder (S3 and Azure; Drive and Dropbox need nothing).
2. Credentials go in the HexDB process's environment, never in `plugin.toml`; each manifest's `pass_env` names the variables it passes on (`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`, `AZURE_STORAGE_CONNECTION_STRING`, `GDRIVE_CLIENT_ID`/`_SECRET`/`_REFRESH_TOKEN`, `DROPBOX_APP_KEY`/`_SECRET`/`_REFRESH_TOKEN`). For Drive and Dropbox, `node connect.mjs` in the plugin's folder signs in with your own app (read-only access) and prints the refresh token.
3. Set the source (bucket, container, folder ID or Dropbox folder), `TARGET` and `TARGET_FOLDER` in the manifest's `[env]`, and grant the plugin's `[access]` role the target tessellation (`writer` includes `upload`).
4. Enable it in the registry. It runs as its own user (`plugin-sources-s3-files`, ...), so its role decides where it may write, and an upload quota on that user caps how much it copies.

### Plugin FAQ

**My plugin doesn't start.** Check the Plugins page and the Logs page (filter by the plugin ID). Common causes: `enabled` is false, `plugins.enabled` is false in the config, the command isn't on `PATH` (use an absolute path), or Node dependencies weren't installed (`npm install` in the plugin folder).

**Does a plugin see changes made while it was down?** A stream plugin does, as long as the change history reaches back that far.

**Can a plugin write back to the tessellation it watches?** Yes, with `[access]`. Guard against loops: skip changes your plugin made, for example by checking a field it sets.

**How do I ship the audit trail to a SIEM?** Use a `stream` plugin with `audit = true` and `tessellations = []` (or just `_audit`).

## 19. Security

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
- After `security.max_failed_logins` (5) failures for one login, or from one address, within `lockout_minutes` (15), sign-in is refused with 429. The count is shared across the lattice. Wrong answers to the password (and code) a signed-in user must give again (changing the password, setting up or turning off MFA, new backup codes, "Add a hex") count the same way, so a stolen session or API key can't be used to guess the password.
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
| `upload` | Files tessellations: upload files and new versions (including resumable uploads); create a files tessellation (with `write`) | yes |
| `delete` | Files tessellations: move files to the trash (from any route, including transactions and GraphQL), delete them from the trash, empty it | yes |
| `restore` | Files tessellations: restore files from the trash and earlier versions of files | yes |
| `status` | Server status, metrics, storage and lattice details | no |
| `logs` | The server log | no |
| `audit` | The audit trail | no |
| `plugins` | Plugins and their delivery state | no |
| `maintenance` | Flush, compact and back up | no |
| `ai` | Run prompts through the lattice's AI provider (`POST /ai/complete`), within its token budget | no |
| `chat` | Ask the AI chat about the data you can read (`POST /ai/chat`, the Ask page), within the chat limits | no |
| `share` | Make sharing links to files and folders you can read, for people without an account (see [Sharing links](#sharing-links)); not in any built-in role but `admin` | no |
| `detokenize` | Read the real values behind vault tokens (see [Tokenization](#tokenization)); not in any built-in role but `admin` | yes |
| `admin` | Everything: users, roles, settings, functions, schedules, joining hexes, shutdown | no |

Built-in roles:

| Role | Permissions |
| --- | --- |
| `admin` | `admin` |
| `reader` | `read` |
| `writer` | `read`, `write`, `upload`, `delete`, `restore` |
| `owner` | `read`, `write`, `manage`, `upload`, `delete`, `restore` |
| `operator` | `status`, `logs`, `plugins`, `maintenance` (including backups) |
| `auditor` | `status`, `audit` |
| `ai` | `ai`, `chat` (for plugins and scripts that use the lattice's AI provider, and the AI chat) |

Administrators create custom roles from any set of permissions:

```bash
curl -X POST http://localhost:7700/roles -H "Content-Type: application/json" \
  -d '{ "name": "analyst", "description": "Read data and see status", "permissions": ["read", "status"] }'
```

On files tessellations, `write` edits a file's name, folder, tags and description; adding, deleting and restoring files need `upload`, `delete` and `restore`. Before 1.6, `write` allowed all of it: on the first start of 1.6, custom roles that have `write` are given the three new permissions once, so they keep working; remove them from roles that shouldn't have them.

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

**The parts:**
- `filter` (the filter language) limits the documents the role can see and write. `{"$user": "login"}`, `{"$user": "id"}`, `{"$user": "email"}` and `{"$user": "attributes.<name>"}` stand for the caller's values. Users' attributes are set by administrators: a JSON object of up to 50 entries. A missing attribute makes the filter match nothing, never everything.
- `hide` lists fields (dotted paths) the role can't see or change.
- `mask` lists fields the role sees masked, for card numbers, account numbers and other values a role needs to recognize but not read (PCI DSS requirement 3.4). Each field says how many characters stay: `show_first` (up to 6) and `show_last` (up to 4), both 0 by default. `{"card_number": {"show_last": 4}}` makes `4111111111111111` read as `************1111`. Strings and numbers keep their length; a value that would keep fewer than 4 characters masked is masked whole; objects, arrays and booleans read as `********`; null stays null. A field can't be both hidden and masked by one role.
- `folders` (files tessellations) limits the role to files in these folders and their subfolders, e.g. `["/inbox/", "/shared/2026/"]`: the same as a filter on `path`, and both must match when a role has both.

Restrictions belong to the role, so each of its permissions is narrowed the same way. To let someone upload only into one folder while reading everything, grant two roles:

```bash
curl -X POST http://localhost:7700/roles -H "Content-Type: application/json" -d '{
  "name": "inbox-uploader", "permissions": ["upload"], "restrictions": { "media": { "folders": ["/inbox/"] } } }'
curl -X PATCH http://localhost:7700/users/ada -H "Content-Type: application/json" -d '{
  "roles": [{ "name": "inbox-uploader", "tessellations": ["media"] }, { "name": "reader", "tessellations": ["media"] }] }'
```

**Combining grants.** When several of a user's grants allow an action on a tessellation:
- an unrestricted grant makes access unrestricted;
- otherwise the user sees the documents any of their filters allows;
- a field is hidden only if every grant hides it;
- a field every grant hides or masks, and at least one masks, is masked, showing as many characters as the most revealing of those masks.

**Reads:**
- Queries, counts, aggregations, `GET /tessellations/{name}`, GraphQL, the change feed and functions only see matching documents. Other documents read as not found.
- Hidden fields are removed from every document returned, and masked fields are masked: in REST, GraphQL, SQL (where a masked column is a string), the change feed, functions and MCP.
- Filtering, sorting, grouping or aggregating on a hidden or masked field is refused with 403, so values can't be probed. So is `$text` unless the text index covers no such field.
- The query plan's `scanned` count reports only visible documents.
- Delete events in the change feed carry no document, so they're shown by ID.

**Writes:**
- A document must match the filter before (replace, patch, delete) and after (every write) the change. Writes that would move a document out of reach are refused with 403.
- Writes can't set or remove hidden or masked fields, and a full replace keeps their stored values. A full replace may send a masked field back exactly as it was shown (`************1111`), so a document can be read, edited and written back; any other value for it is refused with 403. A replace that turns a masked field's parent into something other than an object is refused too, since it would drop the stored value.
- Update-by-filter only touches matching documents.
- An upsert whose key matches a document outside the filter fails with 409.

**Limits:**
- Managing a tessellation (indexes, schemas, deletion) needs an unrestricted grant.
- A stream can only be sourced from a tessellation the creator can read without restriction (a stream copies every change).
- Administrators are never restricted, and restrictions apply to user tessellations only.

**Masked fields in memory.** A field that any role masks usually holds something like a card or account number, so HexDB keeps its value sealed wherever documents stay in memory. That covers the hex's vertices (unflushed and recently read documents), the change feed's last 10,000 changes, a sliced replica's recent changes, idempotency records, and WAL records being written. A process memory dump shows ciphertext for these fields, not values.
- Sealing is AES-256-GCM under a key derived from the lattice key, so every hex can open values sealed on another, and they replicate and back up sealed. Each value is bound to its tessellation, document ID and field path, so it can't be moved elsewhere. Sealed values are stored that way in the WAL and SSTables too, on top of the storage encryption.
- Reading a document opens its sealed values for that request only, so filters, sorting, aggregation, SQL, GraphQL, MCP and administrators see real values, and masks apply per caller as above. The buffers that held a decrypted value are overwritten after use, and so are the hex's shards when a document is replaced or evicted. Copies a request makes while building its response aren't reached.
- Documents of a tessellation with sealed fields aren't kept in the parsed-document cache, so reads of them parse and decrypt each time.
- Indexes hold a keyed token instead of a sealed value. Unique indexes still work, but the planner doesn't use an index that covers a sealed field, so filters and sorts on it scan the tessellation. Text indexes leave sealed fields out; `$text` searches that cover them scan as well.
- The sealed fields follow the roles: every path any role masks, per tessellation (`*` restrictions: every user tessellation). When a role's masks change, each hex reseals what it holds and rebuilds the affected indexes. Documents already on disk are sealed again as they're read into memory. Values sealed before a mask was removed stay readable and are written back as plain values with the document's next write.
- `/status` lists the sealed fields under `memory_protection.sealed_fields`.

**Core dumps.** A core dump holds the whole process memory, keys and open documents included. The server turns core dumps off at startup by setting `RLIMIT_CORE` to 0 (soft and hard limits) on Unix. The kernel honors that for core files, and dump handlers such as systemd-coredump and apport honor it for the crashing process. `/status` reports it as `memory_protection.core_dumps` (Unix; `null` elsewhere). `security.core_dumps = true` in `hexdb.toml` turns dumps back on, for debugging a crash. Windows writes full process dumps only when an administrator has configured Windows Error Reporting's LocalDumps for the server.

### Tokenization

A field marked `tokenize` in the tessellation's [schema](#13-schemas) never holds its real value. Writes replace the value with a token before the document reaches the WAL, memory, disk, replicas or backups, so card numbers, account numbers and national IDs aren't stored in documents at all.

```bash
curl -X POST http://localhost:7700/tessellations/accounts/schemas -H "Content-Type: application/json" -d '{
  "fields": {
    "card": { "type": "string", "tokenize": "vault" },
    "ssn":  { "type": "string", "tokenize": "one_way" }
  }
}'
curl -X POST http://localhost:7700/accounts -H "Content-Type: application/json" -d '{ "name": "Ada", "card": "4111111111111111", "ssn": "123-45-6789" }'
# stored: "card": "tok_q3v6...k2m_1111", "ssn": "tok_7hd2...xa_6789"
```

- **Tokens.** `tok_` followed by 26 lowercase base32 characters (a keyed BLAKE3 hash of the value under a key derived from the lattice key), then `_` and the value's last 4 characters when it has at least 10. Shorter values show nothing. Equal values give equal tokens across the lattice.
- **Lookups.** Filters comparing a tokenized field for equality (a plain value, `$eq`, `$ne`, `$in`, `$nin`) compare tokens. So `{"card": "4111111111111111"}` finds the document in REST, GraphQL and single-table SQL (`WHERE card = ?`), and unique indexes work. Ranges, prefixes, `$contains` and full-text search can't see values. Filtering by the token itself works too.
- **Writes.** Schema rules (`required`, `min_length`, `max_length`, ...) check the real value, before it's replaced. A token written back unchanged, as a document read and saved again carries it, is kept as it is.
- **One-way** tokens can't be turned back into the value by anyone, administrators included: use them for values you only need to match, such as de-duplicating customers by national ID.
- **Vault** tokens can. The value is kept in the `_vault` system tessellation, sealed with the lattice keys and bound to its token, written in the same commit as the document. Users with the `detokenize` permission on the tessellation read it back:

  ```bash
  curl -X POST http://localhost:7700/tessellations/accounts/detokenize -H "Content-Type: application/json" \
    -d '{ "tokens": ["tok_q3v6...k2m_1111"] }'
  # {"values": {"tok_q3v6...k2m_1111": "4111111111111111"}}
  ```

  It takes up to 1,000 tokens. A token that isn't a vault token of that tessellation (one-way, unknown, or another tessellation's) gives `null`. A role that hides or masks a vault field can't detokenize in that tessellation, even with the permission. Each call is audited as `token.detokenize`, with how many tokens were asked for and found, never the values.
- **Adding it later.** A new schema version that tokenizes a field rewrites the existing documents in the background, like any schema migration (`GET /tessellations/{name}/schemas` shows the progress). Vault entries aren't deleted with documents, since one token can be shared by several.
- **Masks still apply.** A masked tokenized field shows its token masked, and tokenization can be combined with [sealed fields](#row-filters-and-field-masks).

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

`access.denied` is recorded for signed-in callers refused with 403, up to 30 per user per minute. The 30th says the rest of that minute's denials aren't recorded (they're still in the request log at warn), so a caller can't fill the disk with refused requests.

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
- API responses are `Cache-Control: no-store` (file content and thumbnails are `private` and revalidated with their ETag).
- Stored files are served `inline` only for images (not SVG), audio, video, PDF and plain text, and otherwise as attachments; with `nosniff` and `default-src 'none'`, a stored HTML or SVG file can't run scripts.
- Other sites can't read responses unless their origin is listed in `cors.allowed_origins` (below).

### Cross-origin requests (CORS)

By default HexDB sends no CORS headers, so a page on another origin can't read its responses. To let your own web app call the API from the browser, list its origin in the lattice configuration:

```bash
hexdb config set cors.allowed_origins "https://app.example.com, http://localhost:5173"
```

- Each origin is `scheme://host[:port]` with no path; values are normalized as browsers send them (lowercase, no default port) and checked when saved. `*` allows every origin and can't be combined with others.
- A listed origin gets `Access-Control-Allow-Origin` with its own value (and `Vary: Origin`); with `*`, the literal `*`. Responses also expose `ETag`, `Location`, `Retry-After`, `Content-Disposition`, `Content-Range`, `Last-Modified`, `Idempotent-Replayed`, `Upload-Offset` and the `X-HexDB-*` headers to the page.
- Preflights (`OPTIONS` with `Access-Control-Request-Method`) are answered before authentication: 204 with the allowed methods (`GET`, `HEAD`, `POST`, `PUT`, `PATCH`, `DELETE`), the requested headers, and `Access-Control-Max-Age: 600`. From an origin that isn't allowed, they get 403 `cross_origin`.
- Credentials are never allowed across origins (no `Access-Control-Allow-Credentials`), so the visitor's session cookie can't be used by another site, and cookie-authenticated changes still have to come from HexDB's own pages. A cross-origin app sends an API key or a session token as `Authorization: Bearer <token>`. An origin listed by name (not through `*`) may also call `POST /auth/login` with `"return_token": true` to sign a user in and get a token.
- The setting is lattice-wide and takes effect on every hex within a second; change it on the Configuration page (Cross-origin access) too.

### Transport

Set `[tls] cert_file` and `key_file` (PEM) to serve HTTPS (HTTP/1.1 and HTTP/2 via ALPN). With TLS:
- HSTS is sent;
- the session cookie is `Secure`;
- hexes replicate over HTTPS, trusting `tls.ca_file` for private CAs.

Without TLS, HexDB warns when it listens on a non-loopback address.

Outgoing HTTPS (other hexes, AI providers, storage and import plugins, webhooks) trusts the operating system's certificate store, plus `tls.ca_file`. On Linux that's the `ca-certificates` package, which the Debian package depends on and the Docker image includes.

Behind a reverse proxy, list the proxy in `network.trusted_proxies` (IPs or CIDR ranges). The client address is then taken from `X-Forwarded-For`: the right-most address that isn't a trusted proxy. Otherwise the TCP peer is the client. The client address is what sign-in throttling, connection limits and the audit trail use. When a trusted proxy terminates TLS and sends `X-Forwarded-Proto: https`, HexDB marks the session cookie `Secure` and sends HSTS, as it does with its own TLS.

### Encryption at rest

All data and metadata on disk is encrypted with AES-256-GCM using `storage.encryption_key`.

| File | Format |
| --- | --- |
| WAL records | `[length][nonce][ciphertext]`, the record compressed with Zstandard first |
| SSTable bodies | Each authenticated with its document ID and sequence number, so bodies can't be swapped between entries |
| SSTable indexes | In chunks of 64 records, each authenticated with its chunk number |
| File chunks | Each encrypted before it's split into fragments, authenticated with its chunk ID and flags; the fragments' headers (chunk ID, lengths, key ID, fragment hash) are plaintext |
| Catalog, settings, metrics history, index snapshots | Sealed files: `HXE1` magic, key ID, nonce and ciphertext, authenticated with the file name so files can't be swapped |

Only SSTable headers (format, counts, offsets, key ID) are plaintext. Every nonce is random (96 bits). Documents in memory aren't encrypted.

**Rotating the key:**

1. Set a new `storage.encryption_key` and move the old one to `storage.previous_encryption_keys`.
2. Restart. Old data stays readable, and everything new uses the new key.
3. Compaction (scheduled, or `POST /compact`) rewrites SSTables and re-encrypts file chunks, and every sealed file is rewritten on its next save.
4. When `/status` shows `storage.sstable_files_on_old_keys: 0` and `storage.files.chunks_on_old_keys: 0`, remove the old key.

A server whose data needs a key that isn't configured refuses to start, rather than discard anything. The error names the setting to fix.

### Between hexes

Hexes authenticate each other with the lattice key: `network.lattice_secret`, or, when that's empty, a key derived from `storage.encryption_key`. The key is never sent.

- **Discovery** is a mutual challenge-response. The prober sends a hello with a timestamp, a nonce and a MAC. The responder answers only a fresh (within 60 seconds), never-seen, valid hello, with an identity MAC bound to the prober's nonce. Strangers learn nothing, and a replayed or forged reply can't impersonate an Overseer.
- **Replication and other `/lattice/*` requests** carry `X-HexDB-Lattice-Signature`: a timestamp, a nonce, and a MAC over the method, path, query and body hash. Each is valid for 60 seconds and once. User sessions and API keys are never accepted there.
- **Their answers** carry `X-HexDB-Lattice-Response`: a MAC over the request's signature, the status and the body hash. A replica checks it before using a snapshot page, a change batch or a chunk, so data changed in transit (on a link without TLS) is refused, and an answer can't be replayed onto another request. Hexes from 2.0 announce this in their (authenticated) discovery identity and must sign; unsigned answers are accepted only from an older hex during an upgrade.
- File chunks and the content key are sent sealed with AES-256-GCM under a transfer key derived from the lattice key, so they stay secret without TLS. Documents in snapshots and change batches are authenticated but not encrypted: use TLS between hexes on networks you don't control. The Overseer's answers to writes a replica forwards for a client are relayed as they are, not signed.
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
- Uploaded files are parsed in a sandboxed worker process that has no keys, no store and (on Linux) no file or network access; see [the parser sandbox](#metadata-extraction).

### Configuration, AI and storage

- **Secrets** in the [lattice configuration](#lattice-configuration) are sealed with the lattice transfer key, bound to their setting name. They're never returned, logged or written to the audit trail; reads show a keyed fingerprint. Every hex holds them decrypted in memory to use them. A change sent to a replica is forwarded to the Overseer like any write, so without TLS between hexes it crosses the network in the clear (see Transport).
- **Who can change what.** The configuration, its history and rollback, AI usage and estimates, tagging existing files and the storage test are for administrators. `POST /ai/complete` needs the `ai` permission and draws on the lattice's monthly budget, which caps what a holder can spend. The chat needs the `chat` permission and stays within its own limits per user and per client address (see [The AI chat](#the-ai-chat)); its lookups run as the asking user. The MCP server and the chat only read: there is no tool that writes.
- **What leaves the hex.** AI calls send what `ai.tagging.send` allows to the configured provider at the configured URL; an `http://` URL (fine for a local model server) sends the key unencrypted. AI base URLs can't name the well-known cloud metadata services (169.254.169.254, `metadata.google.internal`, ...); a host name that resolves to one isn't caught.
- **Field masks.** Fields any role hides or masks in a files tessellation are left out of what AI tagging sends, so an AI description can't reveal them to that role. Images are sent as they are when `thumbnail` or `image` is allowed.
- **Prompt injection.** A file's contents can try to steer its own AI tags and description; the instructions tell the model to treat them as data, and the AI only writes the `ai` field of that file. Treat AI tags as suggestions, not trusted labels.

## 20. Lattices and replication

### Discovery and election

Every hex runs a TCP discovery listener (`network.discovery_endpoint`, default port 7702). A hex that can't open it (another program has the port) stops at startup with `Failed to bind the discovery endpoint`, as it does for the API port, since an unreachable hex can't replicate, vote or fail over. Every `discovery_interval_seconds` (10), it probes:
- the seed addresses in `network.peers` (other hexes' discovery endpoints);
- the local ports 7702-7709, unless `scan_local_ports = false`.

A peer given by name is probed at every address the name resolves to, so one name can stand for a group of hexes: Fly.io's `<app>.internal`, a Kubernetes headless service, or round-robin DNS.

Probes run concurrently with a 3-second timeout. A peer that doesn't answer three consecutive rounds is "lost", and it's forgotten after 30. A round that starts well after it was due (this hex was short of CPU) counts against no one, since the peers may simply not have been asked in time; the log says so.

Discovery is answered from a thread of its own, with the hex's identity as of its last round, so a hex busy enough to be slow (a throttled cloud CPU, a burst of catch-up work) keeps answering its lattice instead of being voted out. It stops answering only if the rest of the hex hasn't run for a minute, which means it's stuck, not busy.

Every hex runs the same election over the same view, so they agree without a consensus round:

1. Candidates are live hexes whose `identity.role` is `auto` or `overseer`. If any prefer `overseer`, only those count.
2. A hex that is already Overseer keeps the role while it's alive, so leadership doesn't flap when a larger hex joins. If two claim it (after a partition heals), the better-ranked one wins.
3. Otherwise, in a sliced lattice, the hex furthest along the change history wins, and among those equally far along, the one holding the most slices, so a hex with every slice is preferred to one that would have to collect them first. Then (and in a lattice without slicing) candidates are ranked by RAM, then disk, then ID.
4. Every other hex is a Harvester, or a Replicant if it prefers that. `harvester` and `replicant` hexes never lead.

A hex that restarts reports where its data really is: an Overseer that restarts reports its last write and every slice, so it gets its role back instead of passing it to a replica that would rebuild.

**Background work runs at a lower priority.** Flushing, compaction, TTL sweeps, file extraction and AI tagging, copying file content between hexes, scrubbing, vertex checks, and serving catch-up copies (snapshots, slice snapshots, file chunks) to other hexes run on a pool of their own at a lower operating-system priority (nice 10 on Linux, below normal on Windows). On a throttled CPU, requests, `/health`, discovery and the change feed go first; the background work still gets about a tenth of a busy CPU and all of an idle one.

### Replication

Every hex keeps a full copy, unless the lattice splits its data into slices (see [Slicing](#slicing)). A replica:

1. **Full sync.** It reads a consistent snapshot from the Overseer: its sequence number S, its tessellations and index definitions, then every document with its version, page by page. Local data the Overseer doesn't have is removed.
2. **Streaming.** It follows the Overseer's change feed after S with long polls, applying each batch atomically together with its replication cursor, so a crash can't leave data and cursor out of step. A change older than the snapshot's version of a document is skipped, so changes made during the snapshot apply exactly once.

**Resuming.**
- The cursor names the Overseer's history ID, kept in its catalog, and a position in it.
- After an Overseer restart the history is the same, so replicas resume from their cursor, reading older changes from the Overseer's change history if needed.
- After a failover the new Overseer has a different history, so replicas take a full sync from it.

**Schemas, indexes and tessellations.** Replicas copy these definitions from the Overseer's catalog. Each change-feed response carries a fingerprint of the catalog, and a replica re-reads the catalog as soon as the fingerprint changes. A new schema version or index is therefore on every hex within one poll (a few seconds), and a replica that takes over as Overseer enforces the same schemas. A periodic check every 15 seconds backs this up.

**Progress.** Each poll for changes after N acknowledges N, which gives the Overseer each replica's exact position. `/status` shows the lag per replica, and the dashboard's lattice card shows it too.

**Reads and writes.** Replicas serve reads. A write sent to a replica is forwarded to the Overseer (`replication.forward_writes`, default true), signed with the lattice key, and the response carries `X-HexDB-Forwarded-To`. With forwarding off, the write fails with 421 `read_only_replica`, naming the Overseer. Requests about the hex itself stay on the hex they're sent to: `PUT /settings` (its runtime settings), `POST /backup`, `/scrub`, `/flush`, `/compact` and `/shutdown`.

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

### Slicing

With `replication.slicing = true` on the Overseer and two or more replicas, the replicas share the data instead of each keeping a full copy, which saves their disk and memory:

```toml
[replication]
slicing = true
slice_copies = 1      # replicas that keep each slice
recent_changes = 20000
```

- **Slices.** Every document of a user tessellation falls in one of 64 slices, by a hash of its tessellation and ID. System tessellations (users, roles, the audit trail, configuration) aren't split into slices: every hex keeps them.
- **Copies.** The Overseer keeps every slice, and each slice is also kept by `slice_copies` replicas. With the default of 1, any one hex can be lost without losing data. A lattice with a single replica keeps full copies, whatever the setting.
- **Placement.** Every hex computes the same placement from the live hexes (rendezvous hashing), so a hex that joins or leaves moves only its own slices. A replica copies the slices it gains from the Overseer, and gives up a slice only once another live replica holds it, so no slice has fewer copies while a new hex catches up.
- **Reads.** A replica holding slices doesn't have all the data, so it forwards requests that read data (queries, SQL, GraphQL, documents, files, MCP and the chat) to the Overseer, and answers with what the Overseer returns. Live feeds (`/changes/stream`, `/changes/ws`, stream subscriptions) aren't proxied: a sliced replica answers 421 `sliced_replica`, naming the Overseer to connect to. Health, status, sign-in and the hex's own maintenance stay local.
- **Failover.** When the Overseer is lost, the replica furthest along its history is elected (among equals, the one with the most slices). Before it serves data or takes writes (data requests get 503 `rebuilding` meanwhile), it collects the slices it lacks from the replicas that keep them, then applies its last `recent_changes` changes to bring each to its own position, so the data is consistent as of one point in history. A slice whose holder is down waits for it to come back: the dashboard, `/status` (`replication.rebuilding` and `waiting_for`) and the log name the hexes it's waiting for, and the other replicas show "waiting" with the reason rather than an error; if a holder was further behind than those recent changes reach, the gap is logged and recorded in the audit trail as `lattice.slices_incomplete`. The other replicas then sync their slices from the new Overseer.
- **Status.** `/status` shows `slices_held_count` and `sliced_documents` (the user documents this hex keeps) under `replication`.

What slicing doesn't change: the Overseer still keeps everything (it runs every write, transaction and unique index), so a lattice can't hold more than the Overseer can. It saves space on the replicas, not on the Overseer.

### Adding a hex

On the Dashboard, "Add a hex" (administrators, password required) shows:
- a `hexdb.toml` snippet: the lattice name, seed addresses, and addresses for the new hex;
- a `hexdb.local.toml` snippet with the shared secret;
- notes on network reachability.

`POST /join {"password"}` returns the same. Each request is audited.

The lattice name is `network.lattice_name`. When it's left empty on the first hex, HexDB generates a name (an adjective and a noun) and saves it in the catalog. Every other hex must use the same name.

### Hex names

Every hex has a name, shown in logs, in the dashboard and to other hexes. Set it with `identity.name` (or `HEXDB_IDENTITY__NAME`): 1 to 64 printable characters, spaces allowed. When it's left empty, the first start draws a name from a built-in list of cat and dog breeds (Great Pyrenees, Ocelot, Dachshund, Norwegian Forest Cat, ...) and saves it in the catalog with the hex's ID, so the hex keeps its name and ID across restarts. Setting `identity.name` later renames the hex and keeps its ID; clearing it again keeps the configured name, since it was saved. Two hexes in a lattice can't share a name: a configured name that a live hex already uses stops the start with an error, and a drawn name that collides is replaced by a free one.

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

## 21. Storage internals

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

Every document held in memory is split into six fragments with Reed-Solomon coding (four data fragments, two parity), one per vertex. Each fragment carries a BLAKE3 hash.

- Reading a document checks the hashes and rebuilds it from any four good fragments.
- A background check (`memory.vertex_integrity_check_frequency`, 300 s) verifies every fragment and rewrites corrupt ones in place.

Any two of the six vertices can be lost or corrupted without losing data. The dashboard's vertex hexagon shows each vertex's fragments, bytes and repairs. This protects against memory corruption. Durability comes from the WAL and SSTables.

A document in memory takes about 1.5 times its JSON size: four data fragments plus two parity fragments. Fragments under 16 KB use a table-driven Reed-Solomon code over GF(256), which has no per-call setup; larger fragments use reed-solomon-simd. Fragments live only in memory, so the code can change between versions without affecting stored data.

### Data layout

```text
<storage.path>/
├── catalog.hxe                tessellations, index definitions, schemas, history ID, lattice name (sealed)
├── settings.hxe               runtime settings changed from the UI or API (sealed)
├── metrics-history.hxe        6 hours of metrics samples (sealed)
├── query-stats.hxe            the query advisor's statistics (sealed)
├── hexdb.pid                  the running server's PID, endpoint and shutdown token (owner-only)
├── initial-admin-password.txt only until you delete it
├── reset-password.request     a waiting `hexdb reset-password` (the new password's hash), and its .result
├── wal/
│   ├── <first-seq>.wal        active write-ahead log segments
│   └── archive/<first-seq>.wal  the change history
├── indexes/<tessellation>/<index>.hxi   index snapshots (sealed)
├── blobs/vertex-<n>/<number>.hxp   file content: one pack file series per vertex (see files.vertex_paths)
└── <tessellation>/<ulid>.hxs  SSTables, including system tessellations such as users, _audit, _streams,
                               _files_trash (trashed files) and _uploads (resumable uploads in progress)
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

**Stored document.** `{"id", "tessellation", "json": {fields as plain JSON}, "ttl"}`, in memory, WAL records, SSTable bodies and replication. Field types are inferred again when it's read, which is lossless. Versions before 0.2 stored each field with a type tag (`"data": {"name": {"type": "String", "value": "..."}}`), about three times larger. Both forms are read, and compaction rewrites old SSTables in the new form. In a lattice, upgrade replicas before the Overseer, as usual: upgraded hexes read both forms, but older hexes can't read the new one.

The index block is a series of chunks of up to 64 records. Each chunk is `length (4) | nonce (12) | AES-256-GCM(records)`, authenticated with its chunk number. A record is `id (16) | flags (1: bit 0 TTL, bit 1 tombstone) | seq (8) | [ttl (8)] | body offset (8) | body length (4)`.

Only the chunk directory (each chunk's first ID, offset and length) and a Bloom filter of the IDs stay in memory, about 2 bytes per document.

Versions 2 (unencrypted) and 3 (encrypted bodies, plaintext index) are still read. Compaction rewrites them as version 4.

**Sealed file.** `HXE1 | key ID (8) | nonce (12) | AES-256-GCM(content)`, authenticated with the file name.

**Pack file** (file content, one series per vertex). An 8-byte header (`HXP1`, version 1, the vertex number), then records appended one after another; a new pack starts at 256 MB. A record is a 98-byte header and one fragment:

| Field | Size |
| --- | --- |
| Magic `SHRD` | 4 |
| Chunk ID (keyed BLAKE3 of the plaintext) | 32 |
| Fragment number (0-3 data, 4-5 parity) | 1 |
| Flags (bit 0: Zstandard) | 1 |
| Key ID | 8 |
| Plaintext length | 4 |
| Ciphertext length | 4 |
| Fragment length | 4 |
| BLAKE3 of the fragment | 32 |
| Check (first 8 bytes of BLAKE3 over the fields above) | 8 |

The ciphertext is `nonce (12) | AES-256-GCM(chunk, maybe compressed)` with the chunk ID and flags as authenticated data, padded and split into four equal data fragments, plus two Reed-Solomon parity fragments. At startup the index is rebuilt from the record headers. Only a vertex's newest pack is appended to; the others are sealed, and the first startup that reads a sealed pack saves its records next to it as `<number>.hxi` (the pack's length, 58 bytes per record, and a BLAKE3 checksum), so later startups read one small file per pack instead of every record header. An index whose checksum or pack length doesn't match is ignored and the pack is read again; reads verify each fragment's own header and hash either way, and deleting a pack deletes its index. Backups don't copy the indexes: a restored hex makes them on its first start. A torn record at the end of a vertex's newest pack (a crash mid-write) is cut off; a bad record elsewhere ends that pack's scan, and its fragments are rebuilt from the other vertices when read or scrubbed.

**File manifest** (stored as a chunk). `HXM1 | count (4) | (chunk ID (32) | length (4))*`, the file's chunks in order.

## 22. Limits and configuration reference

### Limits

| Limit | Default | Setting |
| --- | --- | --- |
| Document size | 1 MB of JSON | `limits.max_document_kb` (live) |
| Bulk, query, transaction and GraphQL bodies | 32 MB | `limits.max_request_mb` |
| Documents one table read returns in SQL with joins, subqueries or expressions | 100,000 | `limits.sql_max_rows` |
| Single-document writes (`POST /{tessellation}`, `PUT` and `PATCH` of `/{tessellation}/{id}`) | The larger of 32 MB and `limits.max_document_kb` | Follows those two settings, as they are at startup |
| Other request bodies | 2 MB | fixed |
| Documents per bulk request | 10,000 | fixed |
| Operations per transaction | 1,000 | fixed |
| Page size | 1,000 (default 100) | fixed |
| File size | No limit beyond the disk budget | `files.max_file_mb` |
| Upload and download time | No limit (exempt from the request time) | fixed |
| Extraction per file | 64 MB read, 30 s | `files.extract_max_mb`, `files.extract_timeout_seconds` |
| Upload sessions | Expire after 24 hours | fixed |
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

### Lattice configuration

Settings that belong to the whole lattice rather than one machine are kept in the lattice configuration: the AI provider and AI features (`ai.*`). Administrators change them on the Configuration page, with `hexdb config`, or through the API. `hexdb.toml` keeps what belongs to one hex (addresses, data directory, keys), and the runtime settings above stay per hex.

```bash
curl http://localhost:7700/config                                # newest version, schema, each hex's state
curl -X PUT http://localhost:7700/config -H "Content-Type: application/json" \
  -d '{ "changes": { "ai.provider": "anthropic", "ai.api_key": "sk-..." }, "comment": "Use Claude", "base_version": 3 }'
curl http://localhost:7700/config/history                        # versions with what each changed
curl http://localhost:7700/config/versions/2                     # one version
curl -X POST http://localhost:7700/config/rollback -d '{ "version": 2 }'
```

From the command line (with an administrator's API key in `HEXDB_TOKEN`):

```bash
hexdb config get                                   # every setting and each hex's version
hexdb config set ai.provider anthropic -m "Use Claude"
hexdb config set ai.api_key                        # prompts without echoing; secrets are never arguments
echo "$KEY" | hexdb config set ai.api_key --stdin
hexdb config set ai.tagging.send '["metadata","text","thumbnail"]'
hexdb config unset ai.model
hexdb config history
hexdb config diff 3 5                              # secrets compared by fingerprint
hexdb config rollback 3 -m "bad key"
```

- **Versions.** Every change writes a new, immutable version with its author, time, comment and a SHA-256 over its canonical form. `null` clears a setting. Saving values that are already in effect writes nothing. With `base_version`, a change made against an older version is refused with 409. A rollback writes a new version with an earlier version's contents, secrets included; history is never rewritten.
- **Secrets** (`ai.api_key`) are sealed with the lattice transfer key and bound to their setting name. No route, log or audit event contains them: reads show `{ "set": true, "fingerprint", "updated", "updated_by" }`, where the fingerprint is a keyed hash that tells two values apart and reveals nothing about them. A secret can be replaced or cleared, never read back.
- **Distribution.** Versions are documents in the `_config` system tessellation, so they replicate like other data: every hex has them, hexes that join later get them in their first sync, and a new Overseer after a failover already holds them. Each hex checks a version's checksum, its settings against the schema and that every secret opens before applying it, within a second of receiving it. A version that fails is rejected on that hex, which keeps the previous one and reports `"state": "rejected"` with the reason. `/status` has `config` (version, checksum, state) and the lattice view shows every hex's version.
- **Every setting is lattice-wide.** Versions saved by 1.6 preview builds may hold remote file storage settings (`file_storage.*`) and per-hex overrides; those features were removed before release, so such settings are ignored and left out of the next version saved.
- **Audit.** Changes are recorded as `config.update` (with the keys changed, cleared and the secrets set or cleared, never values) and `config.rollback`. Only administrators can read the configuration, its history, or roll back.
- **Demo mode.** For public demos, `security.demo_mode = true` in a hex's `hexdb.toml` (or `HEXDB_SECURITY__DEMO_MODE=true`) lets every signed-in user view the configuration, its history, AI usage and the token estimates, on the Configuration page (read-only, with a notice) and through the `GET` routes. Secret values are never shown to anyone. Saving, rolling back, the AI test and tagging existing files stay with administrators. Viewers see token estimates only for files tessellations they can read, and in the logs (with the `logs` permission) client addresses are masked (`66.177.x.x`), so visitors don't see each other's. Since visitors share one account, only administrators can change an account's password or MFA (setting up, turning off, new backup codes), so no visitor can lock the others out. It's off by default and set per hex, so set it on every hex of a demo lattice; `GET /auth/me` reports it.

| Setting | Default | Meaning |
| --- | --- | --- |
| `ai.provider` | `none` | `none`, `anthropic`, `openai`, `github` (GitHub Models), `azure_openai`, `openai_compatible` |
| `ai.model`, `ai.base_url`, `ai.api_key` | empty | Model, API base URL (required for Azure OpenAI and OpenAI-compatible endpoints), key |
| `ai.monthly_token_budget` | 1,000,000 | Tokens AI features may use per calendar month (0: no limit) |
| `ai.max_tokens_per_file` | 4,000 | Tokens one file may use |
| `ai.tagging.enabled` | false | AI tags and descriptions for uploaded files |
| `ai.tagging.tessellations` | empty (all) | Files tessellations whose new files are tagged, comma-separated |
| `ai.tagging.send` | `["metadata", "text"]` | What tagging sends: any of `metadata`, `text`, `thumbnail`, `image`, `video`, `pdf` |
| `ai.tagging.video_frames` | 4 | With `video`: frames sent from each video |
| `ai.video_thumbnails.enabled`, `frames` | false, 6 | The AI picks a video's thumbnail from this many frames |
| `ai.embeddings.provider` | `same` | Embeddings for vector indexes and text similarity searches: `same`, `none`, `openai`, `voyage`, `azure_openai`, `github`, `openai_compatible` |
| `ai.embeddings.model`, `base_url`, `api_key` | empty | Embedding model, API base URL and key (empty key with `same`: `ai.api_key`) |
| `ai.embeddings.user_daily_tokens` | 200,000 | Tokens one user's similarity searches by text may use per day (0: no limit) |
| `a2a.enabled` | false | Answer other agents over A2A (needs `ai.chat.enabled`; see [A2A](#a2a-agent2agent)) |
| `cors.allowed_origins` | empty | Origins whose pages may call the API from a browser, comma-separated, or `*` (see [CORS](#cross-origin-requests-cors)) |

AI features only ever use the key entered here, for this lattice.

### AI

Every AI feature (the query advisor, file tagging, video thumbnails, and plugins) goes through one client that uses the lattice configuration's `ai.*` settings. Nothing is sent while `ai.provider` is `none`.

| `ai.provider` | API | Default `ai.base_url` | `ai.model` |
| --- | --- | --- | --- |
| `anthropic` | Claude Messages API | `https://api.anthropic.com` | default `claude-opus-5-5` |
| `openai` | Chat completions, `Authorization: Bearer` | `https://api.openai.com/v1` | required |
| `github` | GitHub Models (a GitHub token with access to models) | `https://models.github.ai/inference` | required |
| `azure_openai` | Chat completions, `api-key` header | required, e.g. `https://<resource>.openai.azure.com/openai/v1` | required (the deployment) |
| `openai_compatible` | Chat completions (Ollama, LM Studio, vLLM...); the key is optional | required, e.g. `http://localhost:11434/v1` | required |

A ChatGPT subscription can't be used by applications; `openai` needs an OpenAI API key. GitHub Copilot has no API for other applications; GitHub Models is the closest. With Claude models that support it, HexDB asks the API to retry a request the model declines on another model (`fallbacks: "default"`).

- **Budget.** Each call's input and output tokens are added to the month's usage, per feature, in the `_ai_usage` system tessellation (replicas send theirs to the Overseer). Once the month's total reaches `ai.monthly_token_budget`, calls are refused with 429 until the next month or a higher budget (0: no limit). File features also stay within `ai.max_tokens_per_file` per call; other calls within 16,000 tokens.
- **Errors.** A provider's error is returned as 502 `ai_provider_error` with any copy of the key removed; a provider's 429 is returned as 429, and so is a used-up budget. `POST /ai/test` answers 200 with `"ok": false` and the error instead, and 400 when no provider is configured.

```bash
curl http://localhost:7700/ai                       # provider, model, this month's usage and budget (admins)
curl -X POST http://localhost:7700/ai/test          # a tiny call: does the provider, model and key work? (admins)
curl -X POST http://localhost:7700/ai/complete -H "Content-Type: application/json" \
  -d '{ "prompt": "Summarize: ...", "system": "Be brief.", "max_tokens": 2000 }'   # the `ai` permission
```

**AI tags and descriptions.** With `ai.tagging.enabled`, each file uploaded afterwards is sent to the AI once its metadata has been extracted, and the answer is stored on the file as `ai`: `tags` (up to 15, lowercase), `description`, `model`, `input_tokens`, `output_tokens` and the content's `sha256`, so the same content is never tagged twice (a new version is tagged again). `ai` is in the files text index, so `$text` finds files by their AI tags; it can't be written through the document routes, and the user's own `tags` and `description` are never changed. What is sent is set by `ai.tagging.send`: `metadata` (name, folder, type, size and the extracted metadata), `text` (the extracted text, up to 16,000 characters), `thumbnail`, `image` (a JPEG, PNG, GIF or WebP up to 4.5 MB is sent whole; otherwise its thumbnail), `video` (`ai.tagging.video_frames` frames spread through an H.264 video, sent instead of its thumbnail; other videos fall back to the thumbnail), and `pdf` (a PDF up to 20 MB and 100 pages is sent whole, and the model reads its text and pictures page by page; Anthropic and OpenAI only, other providers get what was extracted). Audio isn't sent: audio files are tagged from their metadata. `ai.tagging.tessellations` limits tagging to some files tessellations. The instructions tell the model that file contents are data, but a file can still try to steer its own tags: treat them as suggestions. A failure is recorded as `ai.status: "failed"` (or `"skipped"` when the budget is used up) with the error.

**AI video thumbnails.** With `ai.video_thumbnails.enabled`, `ai.video_thumbnails.frames` keyframes spread through each new H.264 video are decoded and sent as thumbnail-sized images, and the frame the model picks becomes the thumbnail (`thumbnail.chosen_by: "ai"`, with `frame` and `of`). Re-reading the file's metadata keeps that choice while the content is the same.

Turning either on affects files uploaded afterwards only. Existing files are processed when an administrator asks, after looking at the estimate:

```bash
curl http://localhost:7700/ai/estimate                  # per files tessellation: tokens per file, untagged files and their total
curl -X POST http://localhost:7700/media/_ai_tag        # tag files not tagged for their current content (?id= one, ?all=true every file)
curl -X POST http://localhost:7700/media/_ai_thumbnails # let the AI choose existing videos' thumbnails
```

Estimates are rough: about 450 tokens of instructions and answer per file, plus metadata and text at about 4 characters per token, plus image tokens (width x height / 750 after scaling to fit 1568 px, at most 1,600); for `video`, each frame as an image; for `pdf`, about 2,000 tokens a page; per AI thumbnail pick, about 300 tokens plus each frame. Providers count differently. Work runs on the Overseer, one file at a time, through the same budget; the requests are audited as `ai.tag` and `ai.thumbnails`. The CLI has `hexdb config ai-estimate`, `ai-tag <tessellation>` and `ai-thumbnails <tessellation>`; the Configuration page shows the estimates with buttons, and the Files page shows a file's AI tags.

### The AI chat

With `ai.chat.enabled`, users with the `chat` permission can ask questions about the lattice's data on the admin UI's Ask page or with `POST /ai/chat` (`{"question", "history"?: [{"role": "user"|"assistant", "content"}]}`). The configured provider answers by looking things up with the lattice tools below, as the user who asked: only the tessellations they can read, through their row filters, without the fields their roles hide. It answers only questions about this data, and it treats everything it reads as data, not instructions. The answer comes back with the lookups it made (`steps`), the tokens used and what's left of today's allowance. Each question is audited as `ai.chat` (tokens and lookups, not the text), and usage counts as the `chat` feature.

Limits, so a public deployment's users can't spend its owner's tokens:

| Setting | Default | Meaning |
| --- | --- | --- |
| `ai.chat.model` | empty (`ai.model`) | A smaller model is usually enough, e.g. `claude-haiku-5-5` |
| `ai.chat.max_tokens_per_answer` | 1024 | Tokens per model call |
| `ai.chat.max_tool_calls` | 6 | Lookups per question; then the model must answer with what it has |
| `ai.chat.max_question_chars`, `max_history` | 2000, 10 | Question length, and earlier messages sent along |
| `ai.chat.user_daily_tokens` | 100,000 | Per user per day (UTC); 0: no limit |
| `ai.chat.client_daily_tokens` | 50,000 | Per client address per day; separates visitors who share a login |
| `ai.chat.client_questions_per_minute` | 6 | Per client address; 429 with `Retry-After` |

All of it is also bounded by `ai.monthly_token_budget`. Chat requests sent to a replica run on the Overseer, which keeps the counters in memory (they restart with it; the monthly budget doesn't). When a limit is reached, the 429 says when it resets: the daily allowances at midnight UTC (with `Retry-After`), the monthly budget on the first of next month. A question stops offering lookups once half its allowance is spent, so the model always has room to answer with what it found.

### MCP server

`POST /mcp` serves the Model Context Protocol (Streamable HTTP, JSON responses), so external agents can read the lattice: Claude Desktop and Claude Code, or any MCP client. Connect with an API key of a user whose roles allow what the agent should see:

```bash
claude mcp add --transport http hexdb https://hexdb.example.com/mcp --header "Authorization: Bearer hxk_..."
```

The tools are the same as the chat's, and all read-only: `list_tessellations`, `describe_tessellation` (schema, indexes, text-search fields, sample field names), `query` (filter, sort, fields), `get_document`, `search` (full text), `count`, `aggregate`, and `sql` (a `SELECT` on one tessellation). They run with the key's permissions, row filters and field masks, return at most 25 documents and about 24 KB, and shorten long strings. The MCP server doesn't call an AI provider (the client brings its own model) and costs no tokens here. Any hex answers; `GET /mcp` (an event stream) returns 405.

### A2A (Agent2Agent)

Other AI agents can delegate questions about the lattice's data to it over the [A2A protocol](https://a2a-protocol.org/) (version 1.0, JSON-RPC binding). Where MCP hands an agent the tools and lets it bring its own model, A2A hands it an answer: the lattice's own chat does the lookups and replies. It's off until an administrator turns on both `ai.chat.enabled` and `a2a.enabled`:

```bash
hexdb config set ai.chat.enabled true
hexdb config set a2a.enabled true
curl https://hexdb.example.com/.well-known/agent-card.json
curl https://hexdb.example.com/a2a -H "Authorization: Bearer hxk_..." -H "A2A-Version: 1.0" -H "Content-Type: application/json"   -d '{"jsonrpc":"2.0","id":1,"method":"SendMessage","params":{"message":{"messageId":"1","role":"ROLE_USER","parts":[{"text":"How many orders shipped last week?"}]}}}'
```

- **The agent card** (`GET /.well-known/agent-card.json`) is public, as discovery needs, and says nothing while A2A is off (404). It names one skill (questions about the data), the JSON-RPC interface at `/a2a` (its URL is built from the request's host, `https` behind TLS or a trusted proxy), bearer authentication, and streaming. Push notifications are not offered.
- **Every request authenticates** with an API key or session token (`Authorization: Bearer`). The question runs as that user, exactly like the chat: it needs the `chat` permission, lookups use the user's read permissions, row filters and field masks, and the `ai.chat.*` limits and the monthly budget apply. Each answer is audited as `a2a.message`.
- **`SendMessage`** takes a text message (`ROLE_USER`, text parts; other parts are refused with -32005) and returns a completed task (`{"task": ...}`): the answer as an artifact and as the status message, the question and answer in `history` (`configuration.historyLength` trims it), and the model, tokens and lookups in `metadata.hexdb`. If the provider fails or a limit is reached, the task is `TASK_STATE_FAILED` with the reason. A task takes no further messages; to follow up, send a new message with the same `contextId`, and the earlier questions and answers go along as the chat's history.
- **`SendStreamingMessage`** answers the same question as Server-Sent Events (`text/event-stream`). Each event is a JSON-RPC response whose `result` holds one of: `task` (first, `TASK_STATE_WORKING`), `statusUpdate` (one per lookup the chat makes, with a message such as `Looking things up: query in 'orders'`, then the final status), and `artifactUpdate` (the answer, with `lastChunk: true`). The stream ends after the final status. The answer runs on even if the client disconnects, and the task stays readable with `GetTask`.
- **`GetTask`**, **`ListTasks`** (optionally by `contextId`) and **`CancelTask`** see only the caller's own tasks. Tasks are kept for an hour (at most 2,000), and cancelling a finished task gives -32002. `SubscribeToTask` gives -32004 (tasks finish with their answer), and push notification methods give -32003.
- **A2A 0.3 clients** work too: `message/send`, `message/stream`, `tasks/get` and `tasks/cancel` get 0.3-shaped answers (`kind` fields, lowercase states and roles; stream events are `task`, `status-update` with `final`, and `artifact-update`). Other `A2A-Version` values get -32009.
- Requests to `/a2a` are forwarded to the Overseer like the chat, where tasks, contexts and the chat's counters live, in memory. They aren't cut off by `limits.request_timeout_seconds`: the chat's limits bound how long an answer takes.

`POST /ai/complete` lets plugins and scripts use the lattice's provider without a key of their own; it needs the `ai` permission (the built-in `ai` role) and its usage is recorded as `api:<login>`. A plugin gets it with `ai = true` in its manifest's `[access]` section. The Configuration page has a "Test" button and shows the month's usage; the CLI has `hexdb config ai-test` and `hexdb config ai-usage`.

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
| `identity.name` | generated | The hex's name (1-64 characters); drawn and saved on the first start when empty |
| `storage.path` | `./.hexdb` | Data directory |
| `storage.encryption_key` | required | `base64:` plus 32 bytes |
| `storage.previous_encryption_keys` | `[]` | Old keys still accepted for reading |
| `storage.disk_mb` | 8192 | Disk budget |
| `storage.wal_sync` | `true` | fsync before acknowledging |
| `storage.wal_flush_check_frequency` | 60 | Seconds between flush checks |
| `storage.compaction_frequency` | 1800 | Seconds between compactions |
| `storage.change_history_hours`, `change_history_mb` | 24, 512 | Change history kept on disk |
| `storage.backup_path` | `backups` next to the data directory | Where backups are written; must be outside the data directory |
| `files.max_file_mb` | 0 (no limit) | Largest file accepted |
| `files.trash_days` | 30 | Days a deleted file stays in the trash |
| `files.max_versions`, `version_days` | 10, 0 | Earlier versions kept per file, and for how long (0: until pushed out) |
| `files.vertex_paths` | `[]` | Six directories for the vertices' fragments (default `blobs/vertex-0` ... `vertex-5` in the data directory) |
| `files.cache_mb` | 64 | Memory for recently read chunks |
| `files.scrub_hours` | 24 | Hours between scrubs of every fragment (0: never) |
| `files.extract` | `true` | Read metadata from uploaded files |
| `files.extract_max_mb`, `extract_timeout_seconds` | 64, 30 | What extraction may read and how long it may take, per file |
| `quotas.default_mb`, `default_files` | 0, 0 | Upload quota per user (lattice configuration; 0: none; see [Upload quotas](#upload-quotas)) |
| `sharing.enabled`, `default_days`, `max_days`, `require_password` | true, 7, 30, false | Sharing links (lattice configuration; see [Sharing links](#sharing-links)) |
| `files.sandbox`, `sandbox_memory_mb` | true, 1024 | Parse files in a restricted worker process, and its memory limit (see [the parser sandbox](#metadata-extraction)) |
| `files.text_kb` | 64 | Text kept per file for full-text search |
| `files.thumbnails`, `thumbnail_px` | `true`, 320 | Image thumbnails and their size (32-2048) |
| `memory.ram_mb` | 1024 | Memory budget for documents |
| `memory.ttl_scan_frequency` | 600 | Seconds between expiry sweeps |
| `memory.vertex_integrity_check_frequency` | 300 | Seconds between vertex checks |
| `compression.compression_level` | 0 | Zstandard level (-7 to 22; 0 is the library default) |
| `security.admin_login`, `admin_email` | `hexdbadmin` | The first administrator |
| `security.admin_password` | empty | Generated when empty; ignored once users exist |
| `security.session_hours` | 12 | Session lifetime |
| `security.max_failed_logins`, `lockout_minutes` | 5, 15 | Sign-in throttling |
| `security.audit_retention_days` | 90 | Audit event lifetime (0: forever) |
| `security.core_dumps` | false | Allow core dumps of the server (they hold its memory, keys included); see [masked fields in memory](#row-filters-and-field-masks) |
| `security.demo_mode` | false | Every signed-in user can view (not change) the lattice configuration, and only administrators can change passwords or MFA; for public demos |
| `tls.cert_file`, `key_file` | empty | PEM certificate chain and key; both set means HTTPS |
| `tls.ca_file` | empty | Extra CAs trusted for other hexes |
| `limits.*` | see above | Request and connection limits |
| `replication.min_acks`, `ack_timeout_ms`, `forward_writes`, `quorum` | 0, 5000, true, 0 | See [Durability options](#durability-options) |
| `replication.slicing`, `slice_copies`, `recent_changes` | false, 1, 20000 | See [Slicing](#slicing) |
| `plugins.enabled`, `registry` | true, `plugins.json` | Plugin loading |
| `functions.scripts`, `python`, `node` | true, `python3`/`python`, `node` | Script runtimes |
| `ai.api_key_env`, `model`, `base_url` | `ANTHROPIC_API_KEY`, `claude-sonnet-5-5`, Anthropic API | Pre-1.6 query advisor key, used only while the lattice configuration's `ai.provider` is `none` |
| `analyzers.<name>` | none | Custom text analyzers |
| `ui.path` | `../hexdb_admin/dist` | Built admin UI |
| `ui.notice` | empty | A short notice the admin UI shows above every page and on the sign-in screen (also returned by `GET /health`, to everyone) |
| `updates.check` | true | Ask `updates.url` for the newest release a minute after starting, then daily (see Operations) |
| `updates.url` | GitHub's latest-release API for dreaminhex/hexdb | Where to ask; anything answering with `tag_name` and `html_url` |
| `updates.control_url`, `updates.control_token` | empty | Set by HexDB Enterprise: the service that runs rolling updates, and the lattice's token for it |

## 23. Operations

```bash
curl http://localhost:7700/health                       # public: {"status": "ok"}; signed-in users also get name, role, version
curl http://localhost:7700/status                       # documents, memory, disk, operations, vertices, lattice, replication
curl "http://localhost:7700/status/history?minutes=60"  # a metrics sample every 15 s, kept for 6 hours (survives restarts)
curl "http://localhost:7700/logs?level=warn&limit=100"  # recent log records; ?after= tails, ?q= searches, ?target= filters by module
curl -X POST http://localhost:7700/flush                # write unflushed data to SSTables
curl -X POST http://localhost:7700/compact              # merge SSTables and collect unreferenced file chunks now
curl -X POST http://localhost:7700/scrub                # verify every file fragment now and repair bad ones
curl -X POST http://localhost:7700/backup -d '{"name": "nightly-1"}' -H "Content-Type: application/json"
curl http://localhost:7700/backups                      # backups in storage.backup_path
curl http://localhost:7700/plugins
curl http://localhost:7700/openapi.json                 # the REST API as OpenAPI 3.1
curl http://localhost:7700/updates                      # versions, the newest release, and a hosted lattice's rolling update
```

**Logging.** The server keeps its last 5,000 log records in memory for `/logs` and the Logs page. `RUST_LOG` sets the level for the console and the in-memory log. Writes, errors and background tasks log at info; reads at debug; refused requests at warn.

**Backups.** `POST /backup` (or `hexdb backup`, both needing the `maintenance` permission) writes a consistent copy of the data directory while the hex keeps serving reads and writes:

1. The backup holds off flushes and compaction, fixes a sequence number, rotates the WAL so every write up to that number is in a closed segment, and captures the catalog.
2. SSTables are immutable, so they're hard-linked into the backup when it's on the same file system (copied otherwise). The closed WAL segments, index snapshots and encrypted metadata are copied. File content comes along: full pack files are hard-linked (or copied), each vertex's current pack is copied up to its length, and garbage collection waits meanwhile. A backup's file content is in `blobs/vertex-<n>/`; with `files.vertex_paths` set, point those paths at the backup's folders to restore it.
3. The backup is built in `<name>.partial/` and renamed when complete, with a plaintext `backup.json` manifest (time, sequence number, file count; no names or data).

Writes continue throughout, but flushes wait until the backup finishes; with hard links that's moments. `GET /backups` (or `hexdb backup --list`) lists backups. A backup name may contain letters, digits, `-` and `_`; without one, the name is the time and sequence number.

Restoring: stop the hex (or start a new one), point `storage.path` at the backup folder (or copy it into the data directory), and start it with the same encryption keys. Recovery replays the copied WAL on top of the SSTables, exactly as after a crash at the backup's sequence number. The restored data gets a new change history ID, so replicas take a full sync from it and plugins start at its end, rather than mistaking it for the original's history.

Hard-linked backups share disk blocks with the live data, so they don't protect against losing the disk: copy backup folders to other storage. Back up `storage.encryption_key` separately too; the data is useless without it. A Replicant is another option: a live copy you can back up while it's stopped.

**Upgrades.** Stop the hex, replace the binaries and the UI, and start it. Data in older formats is read and rewritten by compaction. In a lattice, upgrade replicas first, then the Overseer. A failover moves the Overseer role while it's down.

**Knowing when to upgrade.** A minute after starting, then daily, each hex asks GitHub for the newest HexDB release (`updates.url`; a plain GET that sends nothing about the hex). `GET /status` reports it as `updates`, and each hex's version is in `network.lattice.hexes`. The first time a hex sees a release newer than its own, it logs a warning and records an `update.available` audit event. The admin UI's dashboard shows every hex's version, marks those that need an update (a newer release, or newer hexes beside them) and says which release is available, with its notes. `updates.check = false` turns the check off, for hexes without internet access; the dashboard still marks hexes behind the rest of the lattice.

**Rolling updates (HexDB Enterprise).** A hosted lattice updates itself from the dashboard: an administrator presses **Update to** the release the hosting service offers, and the Overseer updates the lattice one hex at a time. For each replica (Replicants first), it has the hosting service move the hex's machine to the new release, then waits until the hex is back on it and caught up (up to 15 minutes) before going on. Then it hands its own role over: it stops taking writes (they get 503 with a retry hint for a few seconds), waits until every replica has applied everything it published, and stops being a candidate, so the next election picks an updated hex. Last, its own machine is updated, and it rejoins as a replica. Reads keep working throughout. A step that fails stops the rollout, leaves the remaining hexes on their version and says why; the dashboard shows the progress hex by hex, and every step is in the audit trail (`update.started`, `update.hex`, `update.done`, `update.failed`). `GET /updates` reports the version, the newest release, what the hosting service offers and the rollout (any hex answers, by asking the Overseer); `POST /updates/rollout` starts one. Both need an administrator. The hosting service only ever moves the lattice's own machines, and only to the release it deploys.

**Crashes and restarts.** Every write is in the write-ahead log before it's acknowledged, so a crash (a kill, a power cut, a panic that ends the process) loses nothing acknowledged: the next start replays the log. Something has to start it again:
- `hexdb start -s` runs the server under a small supervisor (`hexdb supervise`, in the same log) that restarts it after a crash: at once, then waiting longer (up to 60 s) while it keeps crashing soon after starting, and not at all after five crashes in ten minutes. It doesn't restart a server that `hexdb stop` stopped (even with `--force`), that exited cleanly, or whose first start failed (a bad configuration or a taken port: `hexdb start` reports it). `hexdb start -s --no-restart` runs the server alone.
- The systemd unit restarts on failure, with the same crash-loop limit (`StartLimitBurst=5` in 10 minutes). `docker-compose.yml` uses `restart: unless-stopped`, and Fly.io restarts crashed machines.

A hex that didn't shut down cleanly leaves its runtime file behind, and the next start reports the crash: a warning in the log, a `server.recovered` audit event, and `restarts` in `/status`: `unclean_shutdowns`, and for the last ones when the crashed process started, its PID and version, its last panic (message, source location and thread, from `last-panic.json`), how it exited if the supervisor saw it, the supervisor's restart count, and how long storage took to open. The last 20 reports are kept in `crashes.json` in the data directory. `panics_since_start` counts panics the process survived. A request that panics fails. A background task that panics (flushing, compaction, replication, discovery, triggers, schedules, streams, extraction, a plugin's runner, ...) is started again, after 1 s and then waiting longer (up to 60 s) while it keeps panicking; `tasks` lists each task with its `restarts`, `last_panic` and whether it's `running`, and the overview page shows a notice, and `supervised` says whether `hexdb supervise` runs it.

**Monitoring.** Use the `@sinks/otlp` plugin for metrics, a logs plugin for logs, and the audit trail for security events. `/status` is suitable for health checks that need more than `/health`.

## 24. Drivers

| Language | Folder | Install |
| --- | --- | --- |
| JavaScript / TypeScript | [drivers/node](drivers/node) | `npm install @dreaminhex/hexdb` (Node.js 18+, Deno, Bun, browsers) |
| Python | [drivers/python](drivers/python) | `pip install hexdb` (3.9+, no dependencies) |
| .NET | [drivers/dotnet](drivers/dotnet) | `dotnet add package HexDB.Client` (.NET 8+) |
| Entity Framework Core | [drivers/dotnet/HexDB.EntityFrameworkCore](drivers/dotnet/HexDB.EntityFrameworkCore) | `dotnet add package HexDB.EntityFrameworkCore` (.NET 8+, EF Core 9) |
| ODBC | [drivers/odbc](drivers/odbc) | Release archives (`odbc/`), or `cargo build --release -p hexdb_odbc` |
| JDBC | [drivers/java](drivers/java) | `com.dreaminhex:hexdb-jdbc` from Maven Central, or the release's `hexdb-jdbc-<version>.jar` (Java 11+, no dependencies) |

The Node.js, Python and .NET drivers each cover:
- documents, queries, counts, aggregations and upserts;
- transactions and idempotency keys;
- the change feed;
- streams with consumer groups;
- functions and GraphQL;
- files: `files(name)` (`Files(name)` in .NET) uploads (resumable from 16 MB), downloads by path or ID (whole, streamed or a byte range), lists folders, searches, and handles versions, the trash, thumbnails and re-extraction.

The Entity Framework Core provider and the ODBC and JDBC drivers, described below, build on them or on SQL. The three client drivers authenticate with an API key or a login, and retry 429 and 503 for reads and for writes that carry an idempotency key. Package names are as published by the release workflow. See [drivers/README.md](drivers/README.md) for examples and how to run their tests.

### Entity Framework Core

`HexDB.EntityFrameworkCore` (in [drivers/dotnet/HexDB.EntityFrameworkCore](drivers/dotnet/HexDB.EntityFrameworkCore)) is an EF Core 9 provider built on `HexDB.Client`. Install it with `dotnet add package HexDB.EntityFrameworkCore` and configure a context:

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
- **Inheritance** (table-per-hierarchy): a hierarchy shares its root type's tessellation, and a discriminator field (`Discriminator`, or as configured) names each document's type. Querying a derived type's `DbSet`, or `OfType<T>()`, filters on the discriminator values of that type and its subtypes; each document is read as its own type.
- **Owned types** (`OwnsOne`, `OwnsMany`) are stored inside the owner's document: a nested object or an array of objects, named after the navigation (`.ToJsonProperty("name")` on the owned navigation renames it). Owned types may own types. They have no `id` of their own and are always loaded with their owner. Filters can use their properties: `Where(i => i.Billing.City == "Oslo")` becomes a filter on `Billing.City`.

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

**Relationships.** One-to-many and one-to-one relationships between entity types work as in EF Core: navigations, foreign key properties (stored as fields), fixup when you add a graph, and cascade deletes of the dependents you've loaded. HexDB itself doesn't enforce foreign keys or cascade on the server.
- `Include`, `ThenInclude`, string includes (`Include("Books.Reviews")`) and `AutoInclude` load related entities. Each included navigation is one more query on the related tessellation per page of results, with the page's keys in an `$in` filter, so index the foreign key fields you include by.
- Tracked queries connect entities through EF Core's change tracker; `AsNoTracking` queries connect them directly, in both directions.
- Filter and sort on foreign key properties (`Where(b => b.AuthorId == id)`), not through navigations: `Where(b => b.Author.Name == "...")` would need a join and throws.

**Saving.** `SaveChanges` sends every added, modified and deleted document as one `POST /transactions`, so the save is atomic: if one operation fails, none apply.
- Added entities are inserted, with their owned entities.
- Modified entities are patched with only the changed properties; a property set to `null` removes the field. A changed owned navigation is written whole: its object, or its array.
- Deleted entities are deleted.

**Transactions.** `BeginTransaction` groups several `SaveChanges` into one HexDB transaction: each save's operations wait, `Commit` sends them all atomically (up to 1,000 operations), and `Rollback`, or disposing the transaction without committing, discards them. Reads inside the transaction don't see its own uncommitted writes, and a concurrency conflict surfaces at `Commit` as `DbUpdateConcurrencyException`. Transactions don't nest.

Concurrency tokens (`[ConcurrencyCheck]` or `.IsConcurrencyToken()`) become an `if_match` precondition on their original values. A document changed or deleted since it was read raises `DbUpdateConcurrencyException`. Any other refusal, such as a unique-index conflict, raises `DbUpdateException` with HexDB's status and message.

**Database operations.**
- `EnsureCreated` creates the model's tessellations; existing ones are kept.
- `EnsureDeleted` drops them.
- `CanConnect` checks `/health`.
- Indexes, schemas and permissions aren't created from the model. Set them up through the API, the CLI or the admin UI.

**Not supported yet.**
- Many-to-many skip navigations: map the join entity, with a navigation to each side.
- Filtered includes (`Include(a => a.Books.Where(...))`), and navigations inside server-side filters and sorts.
- Complex types (`ComplexProperty`): use owned types.
- Table-per-type and table-per-concrete-type inheritance (table-per-hierarchy works).
- Composite keys.
- Migrations.

### ODBC

The ODBC driver ([drivers/odbc](drivers/odbc)) lets applications that read data through ODBC (reporting tools, spreadsheets, pyodbc, R) query HexDB. It sends SQL to [`POST /sql`](#10-sql) over HTTP or HTTPS, so it supports what HexDB's SQL supports: read-only `SELECT`s with joins, subqueries, `UNION`, `WITH` and expressions. `SQLGetInfo` reports those capabilities, so tools that read them can send joins and functions to the server. Permissions, row filters and field masks apply as for any other client.

Build it with `cargo build --release -p hexdb_odbc` (release archives include it in `odbc/`), then register it:
- **Windows:** run `drivers/odbc/install-windows.ps1` from an elevated PowerShell.
- **Linux and macOS:** add it to unixODBC's `odbcinst.ini`.

Then connect with a connection string or a DSN:

```text
Driver={HexDB};Server=http://127.0.0.1:7700;ApiKey=hxk_...
```

**Connection keys.**
- `Server` and `ApiKey` (or `UID` and `PWD` to sign in) are the essentials.
- `PageSize` sets rows per request (default 1,000).
- `MaxStringLength` sets the size reported for text columns (default 4,000).
- `Timeout` sets seconds per request (default 60).
- `CAFile` gives certificates to trust for HTTPS.

**Behavior.**
- **Types.**
  - Booleans are reported as `SQL_BIT`, integers as `SQL_BIGINT`, other numbers as `SQL_DOUBLE`, and text as `SQL_WVARCHAR`.
  - Objects and arrays are `SQL_WLONGVARCHAR` holding JSON.
  - A column's type comes from the tessellation's schema when there is one, otherwise from the first page of results.
- **Catalog.** Tessellations are tables in no catalog or schema, and `id` is each one's primary key.
- **Escapes.** ODBC escape sequences for dates and timestamps become string literals, `{fn ...}` becomes the function call, and `{oj ...}` becomes the join inside it.
- **Cursors.** Forward-only and read-only.

See [drivers/odbc/README.md](drivers/odbc/README.md) for installation on each platform, every connection key, conversions and limits.

The driver is tested with pyodbc through unixODBC on Linux and through the Windows driver manager (in CI). Desktop tools such as Excel and Power BI haven't been tested with it yet.

### JDBC

The JDBC driver ([drivers/java](drivers/java)) is for Java tools and applications: DBeaver, IntelliJ and DataGrip, Spark, reporting tools, and plain JDBC code. Like the ODBC driver, it sends SQL to [`POST /sql`](#10-sql), so permissions, row filters and field masks apply. It needs Java 11 or later and has no dependencies.

```java
Connection db = DriverManager.getConnection("jdbc:hexdb://127.0.0.1:7700?apiKey=" + System.getenv("HEXDB_API_KEY"));
```

- **URLs and properties.** `jdbc:hexdb://host:port` (add `?ssl=true` for HTTPS, or write `jdbc:hexdb:https://host:port`), with `apiKey`, or `user` and `password`, plus `caFile`, `pageSize` and `timeout`.
- **Behavior.** Results are forward-only and read-only, fetched a page at a time. Types and the catalog follow the ODBC driver: booleans, `BIGINT`, `DOUBLE`, `VARCHAR`, and JSON text for objects and arrays; tessellations are tables in no catalog or schema, with `id` as the primary key. JDBC escape sequences are rewritten like ODBC's. There are no transactions: autocommit stays on, and `commit` and `rollback` do nothing.

See [drivers/java/README.md](drivers/java/README.md) for every property, conversions, errors and limits.

## 25. FAQ

**How do I choose between `_update`, a transaction and a function?**
- `_update` applies one merge patch to everything a filter matches.
- A transaction combines different operations with preconditions.
- A function saves either one (or a script) under a name with parameters, so clients don't repeat it.

**Why did my write return 503 but the data is there?** With `replication.min_acks` set, the write committed on the Overseer, but not enough replicas confirmed within `ack_timeout_ms`. Retry with the same idempotency key to be safe.

**Why can't I name a tessellation `settings`?** API routes use that name. Reserved names include `auth`, `audit`, `backup`, `backups`, `triggers`, `settings`, `join`, `streams`, `functions`, `schedules`, `analyzers`, `schemas`, `sql`, `config`, `changes`, `logs`, `plugins`, `lattice`, `status`, `health` and `ui`. Names starting with `_` are reserved too, and `users` and `roles` are taken by system tessellations.

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
