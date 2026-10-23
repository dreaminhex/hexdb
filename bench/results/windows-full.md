# HexDB benchmark results

Machine: 12th Gen Intel(R) Core(TM) i9-12900H (20 threads), windows x86_64. HexDB 2.1.2, release build. Run with `cargo run --release -p hexdb_bench` (316 s).

## Binary sizes

Release builds (`cargo build --release`), not stripped further.

| Measurement | Result |
| --- | --- |
| Server (`hexdb_api`): storage engine, REST, GraphQL, SQL, replication | 50.0 MB |
| CLI (`hexdb`) | 9.2 MB |
| ODBC driver | 2.9 MB |
| Admin UI (built files) | 2.7 MB |
| Total | 64.8 MB |

## Encryption and compression (one core)

Documents are stored on disk as AES-256-GCM(zstd(JSON)), one document at a time. These run in-process on one thread.

| Measurement | Result |
| --- | --- |
| AES-256-GCM encryption (1 MB blocks) | 1104 MB/s |
| AES-256-GCM decryption (1 MB blocks) | 1503 MB/s |
| Order documents (avg 479 bytes of JSON): compressed size | 68% of the JSON (1.47x smaller) |
| Order documents: zstd compression | 92 MB/s |
| Order documents: zstd decompression | 286 MB/s |
| 10 MB structured JSON document: compressed size | 16% of the JSON (6.1x smaller); compress 440 MB/s, decompress 1014 MB/s |
| 10 MB random (base64) document: compressed size | 75% of the JSON (1.3x smaller); compress 927 MB/s, decompress 762 MB/s |

## Vertices: Reed-Solomon fragments in memory (one core)

Each document in memory is split into 4 data and 2 parity fragments across six vertices, each with a BLAKE3 hash. Any 2 of the 6 can be lost. A lost or corrupt vertex is rebuilt from the other fragments; these measure that in-process.

| Measurement | Result |
| --- | --- |
| Encode 100000 order documents (45.7 MB) into fragments | 406 ms (112 MB/s) |
| Memory used per byte of document (data + parity) | 1.51x |
| Read every document back (hashes checked) | 247 MB/s |
| Read every document with one vertex lost (rebuilt on the fly) | 104 MB/s |
| Rebuild a lost vertex: repair all 100000 documents' fragments | 356 ms (100000 fragments repaired) |
| Rebuild two lost vertices at once | 538 ms (200000 fragments repaired) |
| Encode one 10 MB document | 22.0 ms |
| Read one 10 MB document (intact) | 6.29 ms |
| Read one 10 MB document with two lost vertices | 47.1 ms |

## Writes, reads and queries (one server, localhost HTTP)

A release server with default settings: every write is in the write-ahead log and fsynced before it's acknowledged. Times are end to end from an HTTP client on the same machine.

| Measurement | Result |
| --- | --- |
| Bulk insert 100000 orders (1,000 per request) | 32707 docs/s (15 MB/s, 3057 ms) |
| Single insert, one client: median / p99 latency | 2.25 ms / 3.19 ms (435.4 writes/s) |
| Single inserts, 8 concurrent clients (group commit) | 1936.4 writes/s |
| Read one document by ID, one client: median / p99 | 0.12 ms / 0.25 ms (7465.1 reads/s) |
| Reads by ID, 8 concurrent clients | 17602 reads/s |
| Indexed query (customer = ?) over 100000 orders: median / p99 | 0.65 ms / 0.95 ms |
| SQL `SELECT COUNT(*)`: median | 0.24 ms |
| SQL `GROUP BY status` with SUM and AVG over 100000 orders: median | 1094 ms |

## Storage: memory and disk

After writing the orders above, flushing and compacting. Disk is everything in the data directory's SSTables (encrypted, compressed, with indexes); memory is the in-memory fragments.

| Measurement | Result |
| --- | --- |
| Orders stored | 100000 documents, 53.1 MB of JSON |
| Disk used by all data (SSTables, encrypted and compressed) | 44.2 MB for 54.7 MB of JSON (81%) |
| Memory used by the in-memory fragments | 82.6 MB (1.51x the JSON) |

## 10 MB documents

Documents this large need `limits.max_document_kb` raised (the default is 1 MB). Five of each kind, each written and read over HTTP; medians.

| Measurement | Result |
| --- | --- |
| Structured JSON (a report with ~60,000 rows): write | 551 ms (18 MB/s) |
| Structured JSON (a report with ~60,000 rows): read | 163 ms (61 MB/s) |
| Structured JSON (a report with ~60,000 rows): memory per document | 15.0 MB (1.50x) |
| Structured JSON (a report with ~60,000 rows): disk per document | 1.6 MB (16%) |
| Random data, base64-encoded (incompressible): write | 84.2 ms (119 MB/s) |
| Random data, base64-encoded (incompressible): read | 24.2 ms (413 MB/s) |
| Random data, base64-encoded (incompressible): memory per document | 15.0 MB (1.50x) |
| Random data, base64-encoded (incompressible): disk per document | 7.5 MB (75%) |

## Crash recovery (one hex)

The server process is killed (no shutdown) and started again on the same data. Times are from starting the process until it answers /health with every document readable.

| Measurement | Result |
| --- | --- |
| Restart after a crash, 100011 writes not yet flushed (replayed from the WAL) | 3069 ms |
| Restart after a crash, 100000 documents flushed to SSTables | 511 ms |
| Restart after a graceful stop | 510 ms |

## Failover (three hexes on one machine)

The Overseer of a three-hex lattice is killed. A hex is marked lost after three missed discovery rounds; then the others elect a new Overseer.

| Measurement | Result |
| --- | --- |
| Overseer killed, discovery every 1 s: new Overseer elected | 2466 ms |
| Overseer killed, discovery every 1 s: new Overseer accepting writes | 2481 ms |
| Overseer killed, default settings (discovery every 10 s): new Overseer elected | 31377 ms |
| Overseer killed, default settings (discovery every 10 s): new Overseer accepting writes | 31394 ms |
