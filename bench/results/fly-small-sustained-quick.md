# HexDB benchmark results

Machine: 12th Gen Intel(R) Core(TM) i9-12900H (1 threads), linux x86_64 (fly-small-sustained). HexDB 2.1.2, release build. Run with `cargo run --release -p hexdb_bench -- quick` (183 s).

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
| AES-256-GCM encryption (1 MB blocks) | 213 MB/s |
| AES-256-GCM decryption (1 MB blocks) | 246 MB/s |
| Order documents (avg 478 bytes of JSON): compressed size | 68% of the JSON (1.46x smaller) |
| Order documents: zstd compression | 13 MB/s |
| Order documents: zstd decompression | 38 MB/s |
| 10 MB structured JSON document: compressed size | 16% of the JSON (6.1x smaller); compress 42 MB/s, decompress 128 MB/s |
| 10 MB random (base64) document: compressed size | 75% of the JSON (1.3x smaller); compress 121 MB/s, decompress 65 MB/s |

## Vertices: Reed-Solomon fragments in memory (one core)

Each document in memory is split into 4 data and 2 parity fragments across six vertices, each with a BLAKE3 hash. Any 2 of the 6 can be lost. A lost or corrupt vertex is rebuilt from the other fragments; these measure that in-process.

| Measurement | Result |
| --- | --- |
| Encode 20000 order documents (9.1 MB) into fragments | 641 ms (14 MB/s) |
| Memory used per byte of document (data + parity) | 1.51x |
| Read every document back (hashes checked) | 29 MB/s |
| Read every document with one vertex lost (rebuilt on the fly) | 11 MB/s |
| Rebuild a lost vertex: repair all 20000 documents' fragments | 560 ms (20000 fragments repaired) |
| Rebuild two lost vertices at once | 718 ms (40000 fragments repaired) |
| Encode one 10 MB document | 82.6 ms |
| Read one 10 MB document (intact) | 75.8 ms |
| Read one 10 MB document with two lost vertices | 171 ms |

## Writes, reads and queries (one server, localhost HTTP)

A release server with default settings: every write is in the write-ahead log and fsynced before it's acknowledged. Times are end to end from an HTTP client on the same machine.

| Measurement | Result |
| --- | --- |
| Bulk insert 20000 orders (1,000 per request) | 2905.3 docs/s (1 MB/s, 6884 ms) |
| Single insert, one client: median / p99 latency | 5.82 ms / 31.7 ms (137.1 writes/s) |
| Single inserts, 8 concurrent clients (group commit) | 300.7 writes/s |
| Read one document by ID, one client: median / p99 | 0.26 ms / 70.1 ms (441.2 reads/s) |
| Reads by ID, 8 concurrent clients | 847.6 reads/s |
| Indexed query (customer = ?) over 20000 orders: median / p99 | 0.63 ms / 71.3 ms |
| SQL `SELECT COUNT(*)`: median | 0.37 ms |
| SQL `GROUP BY status` with SUM and AVG over 20000 orders: median | 2723 ms |

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
| Structured JSON (a report with ~60,000 rows): write | 5684 ms (2 MB/s) |
| Structured JSON (a report with ~60,000 rows): read | 1280 ms (8 MB/s) |
| Structured JSON (a report with ~60,000 rows): memory per document | 15.0 MB (1.50x) |
| Structured JSON (a report with ~60,000 rows): disk per document | 1.6 MB (16%) |
| Random data, base64-encoded (incompressible): write | 718 ms (14 MB/s) |
| Random data, base64-encoded (incompressible): read | 157 ms (64 MB/s) |
| Random data, base64-encoded (incompressible): memory per document | 15.0 MB (1.50x) |
| Random data, base64-encoded (incompressible): disk per document | 7.5 MB (75%) |

## Crash recovery (one hex)

The server process is killed (no shutdown) and started again on the same data. Times are from starting the process until it answers /health with every document readable.

| Measurement | Result |
| --- | --- |
| Restart after a crash, 20011 writes not yet flushed (replayed from the WAL) | 5124 ms |
| Restart after a crash, 20000 documents flushed to SSTables | 313 ms |
| Restart after a graceful stop | 288 ms |
