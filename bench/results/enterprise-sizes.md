# HexDB Enterprise sizes: what to expect

HexDB Enterprise runs each hex on a Fly.io Machine. These numbers come from running the full benchmark (`cargo run --release -p hexdb_bench`, HexDB 2.1.2) in a Debian 13 container limited to each size's CPU and memory, the way Fly limits a Machine: a CPU quota per 80 ms period (`docker run --cpu-period 80000 --cpu-quota ...`). The host is the same laptop as the other reports (Intel Core i9-12900H, Samsung 980 PRO NVMe SSD, Docker on WSL2).

They're measurements under the same limits, not measurements on Fly. See [What changes on Fly](#what-changes-on-fly) for what's different there.

## The sizes

| Size | Fly Machine | Memory | `memory.ram_mb` (JSON kept in memory) | Volume | Volume limits (Fly) |
| --- | --- | --- | --- | --- | --- |
| Small | 2 shared vCPUs | 2 GB | 512 MB, about 1.1 million 480-byte documents | 20 GB | 4,000 IOPS, 16 MiB/s |
| Standard | 4 shared vCPUs | 4 GB | 1 GB, about 2.2 million | 50 GB | 8,000 IOPS, 32 MiB/s |
| Large | 2 dedicated vCPUs | 8 GB | 2 GB, about 4.5 million | 100 GB | 16,000 IOPS, 64 MiB/s |
| X-Large | 4 dedicated vCPUs | 16 GB | 4 GB, about 8.9 million | 250 GB | 16,000 IOPS, 64 MiB/s |

Documents beyond `memory.ram_mb` are read from the SSTables on disk through the cache, so a lattice can hold far more than this; these are the documents served from memory.

Shared vCPUs on Fly get 6.25% of a core each as a baseline, and can run at 100% while they have a burst balance. The balance builds up while a Machine runs below its baseline, up to 500 seconds, which allows about 9 minutes at full speed. So a Small or Standard hex runs at a dedicated size's speed for bursts and at its baseline when busy for a long time. Dedicated vCPUs always run at full speed.

## Results

| | Small, sustained | Standard, sustained | Small or Large, bursting or dedicated (2 vCPUs) | Standard or X-Large, bursting or dedicated (4 vCPUs) |
| --- | --- | --- | --- | --- |
| CPU limit in the test | 12.5% of one CPU | 25% of one CPU | 2 CPUs | 4 CPUs |
| Bulk writes (1,000 per request) | 2,900 documents/s | 5,900 documents/s | 19,300 documents/s | 21,300 documents/s |
| Single write, median | 5.8 ms | 5.8 ms | 5.6 ms | 5.4 ms |
| Single writes from 8 clients | 300/s | 530/s | 820/s | 820/s |
| Read by ID, median / p99 | 0.26 ms / 70 ms | 0.20 ms / 59 ms | 0.19 ms / 0.43 ms | 0.19 ms / 0.42 ms |
| Reads by ID from 8 clients | 850/s | 1,900/s | 10,300/s | 11,800/s |
| Indexed lookup, median | 0.63 ms | 0.64 ms | 0.88 ms | 1.0 ms |
| `GROUP BY` over the documents | 2.7 s (20,000 documents) | 1.4 s (20,000 documents) | 1.5 s (100,000 documents) | 1.2 s (100,000 documents) |
| AES-256-GCM encryption, one core | 210 MB/s | 460 MB/s | 1,910 MB/s | 1,870 MB/s |
| Restart after a crash, data in SSTables | 0.31 s | 0.15 s | 0.06 s | 0.06 s |

Reports: [Small, sustained](fly-small-sustained-quick.md), [Standard, sustained](fly-standard-sustained-quick.md), [2 vCPUs](fly-large-full.md), [4 vCPUs](fly-xlarge-full.md). The sustained runs use the benchmark's quick mode (20,000 documents instead of 100,000), so their `GROUP BY` times cover fewer documents.

How to read them:

- **With full-speed CPUs, writes wait for the disk, not the CPU.** 2, 4 and 20 threads all write about 820 documents a second from 8 clients, because each commit waits for its fsync. Single-write latency is the disk's.
- **Shared CPUs at their baseline are where the sizes differ.** Throughput falls with the CPU share, and the slowest reads (p99) take 60 to 70 ms: a request that arrives after the Machine has used its quota for the current 80 ms period waits for the next one. Medians stay low, because most requests fit inside the quota.
- **Small and Standard suit bursty work.** Development, staging and apps with steady but modest traffic spend most of their time on their burst balance. Sustained imports, heavy reporting or constant traffic belong on Large or X-Large.

## What changes on Fly

- **The client shared the CPU limit here.** The benchmark's clients ran inside the same limited container as the server. On Fly the clients are elsewhere, so the server keeps its whole CPU share, and throughput (especially reads from 8 clients) should be higher than shown.
- **Per-core speed.** The laptop's cores run at up to 5 GHz. Fly runs server CPUs, which are usually slower per core, so CPU-bound work (encryption, compression, `GROUP BY`) is likely to be slower there. Fly doesn't publish its CPU models, so this can't be predicted exactly.
- **The disk.** Here, Docker keeps its data on WSL2's virtual disk, which adds to every fsync (the same laptop writes a document in 2.3 ms natively on Windows, against 5.3 ms in Docker). Fly volumes are NVMe drives on the Machine's host, so single-write latency may be lower there. Each size's IOPS limit is far above what HexDB's commits need. Bandwidth matters for long bulk loads: at about 20,000 documents a second, HexDB writes roughly 10 MB/s of JSON to its write-ahead log, and more as it flushes and compacts, which is close to the Small size's 16 MiB/s.
- **Failover.** Hosted lattices use the default discovery interval of 10 seconds, so a new Overseer takes writes about 30 seconds after the old one is lost. That depends on the interval, not the size.
