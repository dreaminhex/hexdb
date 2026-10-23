# HexDB benchmark results

Machine: 12th Gen Intel(R) Core(TM) i9-12900H (1 threads), linux x86_64 (fly-standard-sustained). HexDB 2.1.2, release build. Run with `cargo run --release -p hexdb_bench -- quick` (90 s).

## Binary sizes

Release builds (`cargo build --release`), not stripped further.

| Measurement | Result |
| --- | --- |
| Server (`hexdb_api`): storage engine, REST, GraphQL, SQL, replication | 56.9 MB |
| CLI (`hexdb`) | 12.3 MB |
| ODBC driver | 3.8 MB |
| Total | 73.0 MB |

## Encryption and compression (one core)

Documents are stored on disk as AES-256-GCM(zstd(JSON)), one document at a time. These run in-process on one thread.

| Measurement | Result |
| --- | --- |
| AES-256-GCM encryption (1 MB blocks) | 458 MB/s |
| AES-256-GCM decryption (1 MB blocks) | 527 MB/s |
| Order documents (avg 478 bytes of JSON): compressed size | 68% of the JSON (1.46x smaller) |
| Order documents: zstd compression | 28 MB/s |
| Order documents: zstd decompression | 61 MB/s |
| 10 MB structured JSON document: compressed size | 16% of the JSON (6.1x smaller); compress 117 MB/s, decompress 143 MB/s |
| 10 MB random (base64) document: compressed size | 75% of the JSON (1.3x smaller); compress 961 MB/s, decompress 138 MB/s |

## Vertices: Reed-Solomon fragments in memory (one core)

Each document in memory is split into 4 data and 2 parity fragments across six vertices, each with a BLAKE3 hash. Any 2 of the 6 can be lost. A lost or corrupt vertex is rebuilt from the other fragments; these measure that in-process.

| Measurement | Result |
| --- | --- |
| Encode 20000 order documents (9.1 MB) into fragments | 318 ms (29 MB/s) |
| Memory used per byte of document (data + parity) | 1.51x |
| Read every document back (hashes checked) | 57 MB/s |
| Read every document with one vertex lost (rebuilt on the fly) | 23 MB/s |
| Rebuild a lost vertex: repair all 20000 documents' fragments | 312 ms (20000 fragments repaired) |
| Rebuild two lost vertices at once | 327 ms (40000 fragments repaired) |
| Encode one 10 MB document | 15.4 ms |
| Read one 10 MB document (intact) | 64.1 ms |
| Read one 10 MB document with two lost vertices | 94.2 ms |

## Writes, reads and queries (one server, localhost HTTP)

A release server with default settings: every write is in the write-ahead log and fsynced before it's acknowledged. Times are end to end from an HTTP client on the same machine.

| Measurement | Result |
| --- | --- |
| Bulk insert 20000 orders (1,000 per request) | 5915.1 docs/s (3 MB/s, 3381 ms) |
| Single insert, one client: median / p99 latency | 5.79 ms / 9.89 ms (168.4 writes/s) |
| Single inserts, 8 concurrent clients (group commit) | 533.5 writes/s |
| Read one document by ID, one client: median / p99 | 0.20 ms / 59.2 ms (986.5 reads/s) |
| Reads by ID, 8 concurrent clients | 1924.5 reads/s |
| Indexed query (customer = ?) over 20000 orders: median / p99 | 0.64 ms / 60.2 ms |
| SQL `SELECT COUNT(*)`: median | 0.37 ms |
| SQL `GROUP BY status` with SUM and AVG over 20000 orders: median | 1350 ms |

## Storage: memory and disk

After writing the orders above, flushing and compacting. Disk is everything in the data directory's SSTables (encrypted, compressed, with indexes); memory is the in-memory fragments.

| Measurement | Result |
| --- | --- |
| Orders stored | 20000 documents, 10.6 MB of JSON |
| Disk used by all data (SSTables, encrypted and compressed) | 9.9 MB for 12.2 MB of JSON (81%) |
| Memory used by the in-memory fragments | 18.4 MB (1.51x the JSON) |

## 10 MB documents

Documents this large need `limits.max_document_kb` raised (the default is 1 MB). Five of each kind, each written and read over HTTP; medians.

| Measurement | Result |
| --- | --- |
| Structured JSON (a report with ~60,000 rows): write | 2791 ms (4 MB/s) |
| Structured JSON (a report with ~60,000 rows): read | 637 ms (16 MB/s) |
| Structured JSON (a report with ~60,000 rows): memory per document | 15.0 MB (1.50x) |
| Structured JSON (a report with ~60,000 rows): disk per document | 1.6 MB (16%) |
| Random data, base64-encoded (incompressible): write | 311 ms (32 MB/s) |
| Random data, base64-encoded (incompressible): read | 74.6 ms (134 MB/s) |
| Random data, base64-encoded (incompressible): memory per document | 15.0 MB (1.50x) |
| Random data, base64-encoded (incompressible): disk per document | 7.5 MB (75%) |

## Crash recovery (one hex)

The server process is killed (no shutdown) and started again on the same data. Times are from starting the process until it answers /health with every document readable.

| Measurement | Result |
| --- | --- |
| Restart after a crash, 20011 writes not yet flushed (replayed from the WAL) | 2475 ms |
| Restart after a crash, 20000 documents flushed to SSTables | 151 ms |
| Restart after a graceful stop | 104 ms |
