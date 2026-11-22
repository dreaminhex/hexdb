# HexDB 2.5.9 vs 2.6.0

Machine: 12th Gen Intel(R) Core(TM) i9-12900H (20 threads), windows x86_64. 4 rounds (single, lattice, queues), alternating which version runs first; 1220 s in all.

- A: HexDB 2.5.9 (`C:\Users\usual\AppData\Local\Temp\hexdb-v259\win\hexdb-v2.5.9-x86_64-pc-windows-msvc\hexdb_api.exe`)
- B: HexDB 2.6.0 (`D:\Software Projects\hexdb\target\bench\hexdb_api-2.6.0.exe`)

Run with `cargo run --release -p hexdb_bench -- compare <A> <B> 4`. Both versions ran on this machine in one go; numbers from other machines aren't comparable. The lattice scenario runs three hexes on this one machine: it shows what synchronous acknowledgements and relayed downloads cost, not what three zones' network latency adds.

## Errors

| Operation | A | B |
| --- | --- | --- |
| Lattice: downloads (status or bytes wrong) | 0 | 0 |
| Lattice: writes | 0 | 0 |
| Lattice: writes during downloads | 0 | 0 |
| Queues | 0 | 0 |

No request failed: every operation counted returned its expected status (and every download its exact bytes).

## Latency (all rounds pooled; ms)

| Operation | A p50 / p95 / p99 | B p50 / p95 / p99 | p50 change | p99 change |
| --- | --- | --- | --- | --- |
| File download, first byte: from the Overseer | 10.65 / 14.54 / 15.50 | 9.35 / 15.44 / 29.39 | -12.1% | +89.6% |
| File download, first byte: through a sliced replica | 17.86 / 33.44 / 35.98 | 9.51 / 16.74 / 30.56 | -46.8% | -15.0% |
| Indexed query (customer = ?), 50 results | 0.47 / 0.63 / 0.71 | 0.48 / 0.64 / 0.73 | +1.9% | +2.7% |
| Insert one document | 2.31 / 2.58 / 2.82 | 2.31 / 2.59 / 2.82 | +0.3% | +0.1% |
| Insert, 8 clients | 4.15 / 4.45 / 5.83 | 4.16 / 4.47 / 4.82 | +0.2% | -17.3% |
| Lattice (min_acks = 2): insert, 8 clients | 11.23 / 11.90 / 15.54 | 11.22 / 11.73 / 16.68 | -0.1% | +7.3% |
| Lattice: insert while files download | 11.24 / 13.13 / 15.16 | 11.23 / 12.34 / 15.05 | -0.1% | -0.7% |
| Queue receive (up to 10), counts polled 20/s | 82.65 / 88.72 / 96.04 | 82.31 / 87.96 / 90.92 | -0.4% | -5.3% |
| Queue receive (up to 10), no one watching | 80.80 / 87.57 / 93.20 | 80.62 / 87.41 / 91.11 | -0.2% | -2.2% |
| Read by ID | 0.13 / 0.22 / 0.26 | 0.13 / 0.21 / 0.26 | -1.7% | -2.3% |
| Read by ID, 8 clients | 0.50 / 0.58 / 0.63 | 0.50 / 0.57 / 0.62 | -0.4% | -1.8% |
| SQL GROUP BY with SUM and AVG | 283.95 / 291.31 / 299.64 | 288.39 / 294.42 / 314.88 | +1.6% | +5.1% |
| SQL SELECT COUNT(*) | 0.29 / 0.52 / 13.08 | 0.32 / 0.49 / 12.92 | +12.6% | -1.3% |
| Sorted page with a range on the sort field | 8.81 / 10.53 / 11.83 | 8.67 / 10.32 / 11.10 | -1.6% | -6.2% |

## Throughput (median of rounds, with the range)

| Operation | A | B | Change |
| --- | --- | --- | --- |
| Bulk insert (1,000 per request), docs/s | 28651 (28321 to 28771) | 28373 (28198 to 28813) | -1.0% |
| File downloads, MB/s: from the Overseer (2 clients) | 560 (546 to 563) | 536 (502 to 537) | -4.4% |
| File downloads, MB/s: through a sliced replica (2 clients) | 535 (519 to 537) | 522 (500 to 530) | -2.5% |
| Inserts, 8 clients, writes/s | 1917 (1874 to 1919) | 1902 (1894 to 1933) | -0.7% |
| Lattice (min_acks = 2): writes/s, 8 clients | 704 (693 to 705) | 702 (691 to 716) | -0.3% |
| Lattice: writes/s while files download | 704 (673 to 707) | 707 (697 to 708) | +0.3% |
| Queue messages/s, counts polled 20/s | 241 (237 to 241) | 241 (239 to 242) | +0.0% |
| Queue messages/s, no one watching | 245 (242 to 245) | 244 (244 to 245) | -0.2% |
| Reads by ID, 8 clients, reads/s | 16115 (15714 to 16137) | 16267 (15793 to 16381) | +0.9% |

## Server memory (median of rounds)

| Measurement | A | B | Change |
| --- | --- | --- | --- |
| Resident set after the workload | 674 MB | 671 MB | -0.4% |
| Peak resident set | 685 MB | 682 MB | -0.3% |
| Replica's peak resident set while relaying downloads | 127 MB | 128 MB | +0.4% |

A change within the throughput ranges above is noise, not a difference between the versions.
