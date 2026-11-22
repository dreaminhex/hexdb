# HexDB benchmark results

Machine: 12th Gen Intel(R) Core(TM) i9-12900H (20 threads), linux x86_64 (debian-docker). HexDB 2.6.0, release build. Run with `cargo run --release -p hexdb_bench` (427 s).

## Binary sizes

Release builds (`cargo build --release`), not stripped further.

| Measurement | Result |
| --- | --- |
| Server (`hexdb_api`): storage engine, REST, GraphQL, SQL, replication | 114.2 MB |
| CLI (`hexdb`) | 30.9 MB |
| ODBC driver | 4.0 MB |
| Admin UI (built files) | 2.8 MB |
| Total | 151.9 MB |

## Encryption and compression (one core)

Documents are stored on disk as AES-256-GCM(zstd(JSON)), one document at a time. These run in-process on one thread.

| Measurement | Result |
| --- | --- |
| AES-256-GCM encryption (1 MB blocks) | 1975 MB/s |
| AES-256-GCM decryption (1 MB blocks) | 2179 MB/s |
| Order documents (avg 479 bytes of JSON): compressed size | 68% of the JSON (1.47x smaller) |
| Order documents: zstd compression | 101 MB/s |
| Order documents: zstd decompression | 315 MB/s |
| 10 MB structured JSON document: compressed size | 17% of the JSON (5.9x smaller); compress 439 MB/s, decompress 1553 MB/s |
| 10 MB random (base64) document: compressed size | 75% of the JSON (1.3x smaller); compress 814 MB/s, decompress 828 MB/s |

## Vertices: Reed-Solomon fragments in memory (one core)

Each document in memory is split into 4 data and 2 parity fragments across six vertices, each with a BLAKE3 hash. Any 2 of the 6 can be lost. A lost or corrupt vertex is rebuilt from the other fragments; these measure that in-process.

| Measurement | Result |
| --- | --- |
| Encode 100000 order documents (45.7 MB) into fragments | 414 ms (110 MB/s) |
| Memory used per byte of document (data + parity) | 1.51x |
| Read every document back (hashes checked) | 266 MB/s |
| Read every document with one vertex lost (rebuilt on the fly) | 111 MB/s |
| Rebuild a lost vertex: repair all 100000 documents' fragments | 318 ms (100000 fragments repaired) |
| Rebuild two lost vertices at once | 393 ms (200000 fragments repaired) |
| Encode one 10 MB document | 12.9 ms |
| Read one 10 MB document (intact) | 3.25 ms |
| Read one 10 MB document with two lost vertices | 24.4 ms |

## Writes, reads and queries (one server, localhost HTTP)

A release server with default settings: every write is in the write-ahead log and fsynced before it's acknowledged. Times are end to end from an HTTP client on the same machine.

| Measurement | Result |
| --- | --- |
| Bulk insert 100000 orders (1,000 per request) | 22971 docs/s (10 MB/s, 4353 ms) |
| Single insert, one client: median / p99 latency | 5.39 ms / 6.45 ms (183.0 writes/s) |
| Single inserts, 8 concurrent clients (group commit) | 790.3 writes/s |
| Read one document by ID, one client: median / p99 | 0.21 ms / 0.53 ms (4045.7 reads/s) |
| Reads by ID, 8 concurrent clients | 11029 reads/s |
| Indexed query (customer = ?) over 100000 orders: median / p99 | 0.90 ms / 1.27 ms |
| SQL `SELECT COUNT(*)`: median | 0.27 ms |
| SQL `GROUP BY status` with SUM and AVG over 100000 orders: median | 490 ms |

## Storage: memory and disk

After writing the orders above, flushing and compacting. Disk is everything in the data directory's SSTables (encrypted, compressed, with indexes); memory is the in-memory fragments.

| Measurement | Result |
| --- | --- |
| Orders stored | 100000 documents, 53.1 MB of JSON |
| Disk used by all data (SSTables, encrypted and compressed) | 18.2 MB for 54.7 MB of JSON (33%) |
| Memory used by the in-memory fragments | 82.6 MB (1.51x the JSON) |

## 10 MB documents

Documents this large need `limits.max_document_kb` raised (the default is 1 MB). Five of each kind, each written and read over HTTP; medians.

| Measurement | Result |
| --- | --- |
| Structured JSON (a report with ~60,000 rows): write | 875 ms (11 MB/s) |
| Structured JSON (a report with ~60,000 rows): read | 159 ms (63 MB/s) |
| Structured JSON (a report with ~60,000 rows): memory per document | 15.0 MB (1.50x) |
| Structured JSON (a report with ~60,000 rows): disk per document | 1.6 MB (16%) |
| Random data, base64-encoded (incompressible): write | 87.0 ms (115 MB/s) |
| Random data, base64-encoded (incompressible): read | 13.3 ms (750 MB/s) |
| Random data, base64-encoded (incompressible): memory per document | 15.0 MB (1.50x) |
| Random data, base64-encoded (incompressible): disk per document | 7.5 MB (75%) |

## Crash recovery (one hex)

The server process is killed (no shutdown) and started again on the same data. Times are from starting the process until it answers /health with every document readable.

| Measurement | Result |
| --- | --- |
| Restart after a crash, 100012 writes not yet flushed (replayed from the WAL) | 2709 ms |
| Restart after a crash, 100000 documents flushed to SSTables | 262 ms |
| Restart after a graceful stop | 322 ms |

## Failover (three hexes on one machine)

The Overseer of a three-hex lattice is killed. A hex is marked lost after three missed discovery rounds, each waiting up to 3 s for the unanswered probe, and not while another hex still sees it (one more round), so a hex cut off alone can't split the lattice; then the others elect a new Overseer.

| Measurement | Result |
| --- | --- |
| Overseer killed, discovery every 1 s: new Overseer elected | 2946 ms |
| Overseer killed, discovery every 1 s: new Overseer accepting writes | 2964 ms |
| Overseer killed, discovery every 5 s (the default): new Overseer elected | 14874 ms |
| Overseer killed, discovery every 5 s (the default): new Overseer accepting writes | 16101 ms |
