# Design: partitioned writes

Status: proposal for review (2026-11-16). Nothing here is built.

## Why

Today every lattice has one Overseer. It holds every document and takes every write; replicas follow its change feed. Slicing (`replication.slicing`) spreads the replicas' storage across 64 slices, but the Overseer still keeps all 64 and commits every write. So one hex's disk and write throughput are the lattice's limits.

Partitioning splits the 64 slices into groups, each with its own Overseer, so storage and writes scale with the number of groups.

## Goals

- Writes and storage scale with the number of partitions.
- Every guarantee HexDB gives today holds across partitions: durability with `min_acks`, failover without losing acknowledged writes, serializable transactions, unique indexes, row and field security, idempotency, the change feed, backups that restore to one consistent point.
- A lattice that doesn't turn it on behaves exactly as it does today (one partition holds all 64 slices), and the upgrade from 2.x is a no-op.
- Partitions can be added (and removed) on a live lattice without downtime or lost writes.

## Non-goals (first version)

- Choosing a partition key per tessellation. Placement stays by hash of (tessellation, document ID), as slices are now. A per-tessellation key (all of one customer's orders together) is a later addition on the same map.
- Geo-partitioning (pinning partitions to regions).
- Partitioning system tessellations (users, roles, config, audit, functions, schedules, streams and queues configs). They stay in one partition (below).

## Concepts

- **Slice**: unchanged. 64 slices, slice = hash(tessellation, ID) mod 64.
- **Partition**: a set of slices with its own Overseer, history (`history_id`), WAL, sequence numbers and replicas. Partition 0 also holds the system tessellations.
- **Partition map**: slice -> partition, plus each partition's member hexes, and an **epoch** that increases with every change. It's a system document in partition 0, replicated to every hex, with the epoch in every inter-hex request.
- **Group**: the hexes of one partition. Each has at least 3 hexes for quorum. Within a group everything works as today: election, `min_acks`, handover, retire, optional slicing among its replicas.

A hex belongs to one group. (Hexes that serve several partitions are possible later; one per hex keeps elections and resource accounting simple.)

## Routing

Any hex can take any request (as replicas do now) and routes it:

1. **Single-document requests** (get, replace, patch, delete by ID): slice of (tess, id) -> partition -> that partition's Overseer, or a replica for reads.
2. **Inserts**: the receiving hex generates the ULID first (as the Overseer does now), then routes. An insert with a client-chosen ID routes the same way.
3. **Bulk writes**: split by partition. Without `Idempotency-Key`: one write per partition, not atomic across partitions, as each document already is today. With a key: as a transaction (below), so a retry is safe.
4. **Writes to system tessellations**: partition 0's Overseer.
5. **Epoch mismatch**: a partition Overseer that gets a request for a slice it no longer owns answers 421 with the current epoch. The router refreshes its map and resends. A write is only refused this way before anything is written, so resending is safe (the same rule as the `stepping_down` resend in 2.5.6).

The forwarding signature already covers method, path and client address. It gains the epoch, so a replayed request can't land in a partition that has since moved.

## Reads across partitions

Queries, counts, aggregations, SQL, GraphQL lists, vector search and full-text search fan out to every partition (or only to those the filter's `id` predicate allows) and merge:

| Operation | Per partition | Merge |
|---|---|---|
| Query, sorted with limit L, offset O | top O+L by the sort | k-way merge, then skip O, take L |
| Paging with `after` | from the cursor | cursor = map of partition -> last key (opaque token) |
| Count, `with_total` | count | sum |
| Aggregations: count, sum, min, max | partial | combine |
| avg | sum and count | divide |
| percentile, distinct | exact values (bounded by `limits.sql_max_mb`) | compute on the merged values |
| Group by | partial per group | combine per group |
| SQL joins | rows of each table, filters pushed down | join at the routing hex, within `sql_max_rows` / `sql_max_mb` |
| Vector top-k | top k | merge by score |
| Text search | top k by score | merge (scores are per-partition BM25; a global IDF is a later improvement) |

Consistency of reads: each partition answers from its own state, so a fan-out query sees each partition at a slightly different moment, as reading from replicas does today. A query that must see one consistent point uses a transaction (snapshot reads are a later addition). A partition that doesn't answer in time fails the query with 503 `partition_unavailable` and names it. Partial results are never returned silently.

Row and field security (`access::Scope`) is applied in each partition before results leave it, as now.

## Transactions across partitions: two-phase commit

A transaction whose documents are all in one partition runs there as today: one commit, the fast path.

Otherwise the routing hex picks a **coordinator**: the Overseer of the partition of the transaction's first document.

1. **Prepare.** The coordinator writes a transaction record `{id, participants, state: preparing}` in its WAL, replicated with `min_acks`. Each participant checks its operations (versions, `if_match`, schemas, uniqueness, row security) under its commit lock. If they pass, it writes a durable **prepare record** holding the new versions, and locks those documents: other writes to them wait or get 409. Then it votes yes. Any no vote, or a timeout, aborts.
2. **Decide.** On all yes votes, the coordinator durably records `committed` (replicated before it answers anyone). This is the commit point.
3. **Commit.** Participants apply their prepared writes as ordinary commits (on their change feeds, idempotency records included) and release the locks. The coordinator answers the client once every participant has acknowledged, or once the decision is durable and a retry is safe (the idempotency key is stored with the decision).

Failures:
- **A participant fails before voting**: the coordinator's prepare times out, and it decides `aborted`.
- **A participant's Overseer fails after voting yes**: its prepare record is replicated, so the new Overseer inherits the prepared transaction and its locks, and asks the coordinator for the decision.
- **The coordinator fails**: the decision is in its partition's replicated WAL. Its successor finishes the commit, or aborts a transaction still `preparing`.
- **Locks held too long**: a prepared transaction with no decision after `transactions.prepare_timeout` (30 s) makes the participant ask the coordinator. It never aborts a yes vote on its own: only the coordinator decides.

Unique indexes span partitions. Each unique value gets a **reservation document** in the partition its own hash maps to, (tess, index, value) -> document ID. Writing a document with a unique field is a two-partition transaction: the document plus its reservation. That's the price of uniqueness, and only tessellations with unique indexes pay it.

## The change feed, streams, plugins, triggers

- Each partition has its own change feed and sequence numbers. The lattice feed (`/changes`) merges them. Its cursor becomes a vector (partition -> sequence), encoded as one opaque string; a plain number still works on a single-partition lattice. Order is per partition. Across partitions, the effects of one cross-partition transaction appear together: participants publish under the transaction's ID, and the merge holds them back until all have arrived.
- Stream sources and process plugins keep vector cursors (`_plugin_cursors`, `__source:N`). The ack protocol from 2.5.6 acknowledges per partition.
- Triggers run on the partition Overseer that commits the write. Schedules and functions run on partition 0.
- Queues: each queue lives in the partition its name hashes to. Dead-letter moves to a queue in another partition use a two-partition transaction.

## Elections, failover and membership

- Each group elects its own Overseer exactly as today, and failover stays within the group, so the existing guarantees carry over per partition.
- Partition 0's Overseer also owns the partition map. Map changes are writes there, replicated to every hex, each with a new epoch.
- A group that loses quorum stops taking writes for its slices. The others carry on, and reads that need it fail with `partition_unavailable` rather than answer partially.
- Retiring a hex, rolling updates and handover work per group.

## Adding and removing partitions (rebalancing)

Moving slice s from partition A to partition B, with no write lost or doubled:

1. **Copy**: B's Overseer copies slice s from A, as a sliced replica copies a slice it gains today: snapshot, then A's change feed for s.
2. **Catch up** until B is within a few changes of A.
3. **Fence**: A stops taking writes for s. A write for s on A answers 421 with "moving", and routers hold it for up to 10 s, as writes are held during a handover. A records its last sequence for s, and B applies up to it.
4. **Flip**: partition 0 commits a new map (s -> B, epoch + 1). Routers resend held writes to B.
5. **Release**: A keeps s read-only for a grace period (in-flight reads, a rollback if B fails), then deletes it.

A crash at any step leaves s owned by exactly one partition. Before the flip it's A, and B's copy is discarded. After it, it's B, which has everything up to the fence. A cross-partition transaction that touches s during a move waits for the fence or the flip. Moving one slice at a time bounds the fence's effect to 1/64 of the data.

## Backups and restore

A consistent backup across partitions needs one cut, the same point in every partition:
- The backup coordinator (partition 0) pauses new cross-partition decisions for a moment, collects each partition's sequence, and resumes. Single-partition writes never pause.
- Each partition backs up up to its sequence.
- Every cross-partition transaction is entirely before or entirely after the cut, because decisions were paused while it was taken.

Restore rebuilds each partition and the map at that epoch.

## Hosted lattices

Partitioning would become a plan option (hexes x partitions). The control plane (website) would provision groups of 3 hexes per partition, in the regions already chosen. The router doesn't change: any hex routes.

## Migration

- 2.x lattices: one partition, every slice, epoch 1. No data moves. The vector cursor of a one-partition lattice is the plain number it is today.
- Turning partitioning on: add a group of hexes as partition 1 (empty), then rebalance slices onto it one at a time. Every step can be undone by moving slices back.
- Mixed versions: map changes wait until every hex reports a version that understands partitions.

## Testing plan

Most of the work is here.

- **Deterministic fault injection** at every step of 2PC (before and after each durable record), every rebalancing step, and every election, extending the WAL pause hooks the 2.5.6 review introduced.
- **History checking**: a randomized workload (single-document and cross-partition transactions, failovers, rebalancing, network partitions via a proxy). Its client history is checked for linearizability per document and serializability for transactions, Jepsen-style. Run in CI on a schedule, not on every push.
- **Equivalence**: for queries, aggregations and SQL, the same data in 1 partition and in N gives the same answers (property tests).
- **Restore drills**: back up during rebalancing and cross-partition writes, restore into a clean lattice, compare.

## Stages

Each stage ships off by default, behind `partitioning.enabled`, and is useful alone:

1. Partition map, epochs, routing with one partition. No behavior change, and the map is in place.
2. Multiple partitions, single-document writes, fan-out reads, merged change feed with vector cursors. Cross-partition transactions and unique indexes are refused.
3. Two-phase commit, unique reservations, idempotent bulk writes across partitions.
4. Rebalancing: adding and removing partitions on a live lattice.
5. Consistent backups and restore, then hosting integration.

## Decisions (2026-11-19)

1. One partition per hex.
2. Stage 2 ships without cross-partition transactions: they're refused with a clear error until stage 3.
3. The partition key is the document hash; per-tessellation keys come later.
4. Unique indexes stay global, at the cost of a two-partition write per unique field.
5. The smallest partitioned plan is 2 partitions x 3 hexes (6 machines).

## Open questions for review (answered above)

1. Is one partition per hex acceptable, or should a hex be able to host several (fewer machines for small lattices, more complex accounting)?
2. Should stage 2 ship without cross-partition transactions (refused with a clear error), or wait for stage 3?
3. Is the partition key fixed as the document hash, with per-tessellation keys later, or wanted from the start?
4. Are global unique indexes worth a two-partition write per unique field? The alternative is unique within a partition only, which is surprising.
5. For hosting: what's the smallest partitioned plan (2 partitions x 3 hexes = 6 machines)?
