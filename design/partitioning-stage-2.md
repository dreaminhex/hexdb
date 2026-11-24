# Design: partitioned writes, stage 2

Status: accepted 2026-11-23 (decisions below), before any code. It builds on design/partitioning.md (accepted, decisions 2026-11-19). Stage 1 is built: the partition map and its epoch, signed on forwarded requests, `GET /status/partitions`.

Stage 2: several partitions, single-document writes routed by slice, fan-out reads, a merged change feed with vector cursors. Cross-partition transactions and unique indexes are refused until stage 3. Everything stays behind `partitioning.enabled`, off by default, and a lattice with one partition behaves exactly as today.

## What the code assumes today

A survey of the code found five assumptions that stage 2 breaks. Each would lose or misplace data if ignored, so each gets a rule below.

1. **System tessellations are recognized by a leading `_`** in `PartitionMap::partition_of` (partitioning.rs:110). But `users` and `roles` are system tessellations without the prefix (users.rs:30, catalog.rs:168). With two partitions they'd be hashed across them.
2. **System records are committed in the same batch as user documents.** `_idempotency` records (writes.rs:979, transactions.rs:313), `_vault` entries for tokenized fields (writes.rs:1381, from commit_inner :1198), and `_trash`/`_deletes` entries on every delete (deletes.rs:968, from commit_inner :1191). By the map's rule they belong to partition 0, but they're written atomically with documents in other partitions.
3. **An Overseer holds all 64 slices.** `set_role` (engine.rs:738), `restore_replication_state` (replica.rs:357), the follower's `step`, `rebuild` and `finish_rebuild` (replication.rs:553, 1154, 1289), the cursor written when a hex leads (replication.rs:574, `held_slices: ALL_SLICES`) and `local_identity` (discovery.rs:266) all assume it. A partition Overseer would look permanently "rebuilding".
4. **A replica's `/changes` uses its own sequence numbers.** Replicated batches get local sequence numbers in `commit_inner(replicated = true)` (writes.rs:1155, 1251). Cursors already differ from hex to hex, so a merged feed can't be built from whatever hex answers.
5. **Text search is a filter, not a ranking.** `$text` is a filter operator (filter.rs:19, 339) with no score. The design's "text search: top k by score, merged" has nothing to merge. In stage 2, `$text` fans out like any other filter. Ranked search is separate work.

## Rules

### Which partition holds what

- `partition_of` takes the engine's `is_system_tessellation` (the `_` prefix, the default system kinds `users` and `roles`, or a catalog kind of `system`) instead of looking at the name. System tessellations live in partition 0. This fixes (1) and is safe to ship alone, even with one partition.
- **Partition-local system tessellations.** Records written in the same commit as a document stay in that document's partition, so the commit stays one commit in one WAL. That covers `_idempotency`, `_vault`, `_trash`, `_deletes`, `_files_trash`, and the cursors of what runs per partition (`_plugin_cursors`, `_trigger_cursors`, `_stream_offsets` for sources). They're system tessellations, but placed by the partition that wrote them, not by hash. Reads that need all of them fan out:
  - `GET /deletes` and the trash merge every partition's lists. A restore goes to the partition that holds the batch, named in its ID.
  - Detokenizing asks every partition and takes the first answer. Equal values give equal tokens everywhere, because the tokenization key is lattice-wide, so duplicate vault entries in two partitions agree.
  - An idempotency key is looked up in the partition the request routes to. A single-document request always routes to the same partition, so a retry finds its record.
- Everything else system-wide (users, roles, API keys, sessions, the configuration, schemas and indexes through the catalog, functions, schedules, streams' and queues' definitions, the partition map, the audit log) is in partition 0. Every hex follows it (below).

### Groups, elections and quorum

- `PeerHex` gains `partition: Option<u32>`, from the map's `hexes` lists. A hex not in the map is unassigned. It serves reads by routing them, holds nothing, and can't be elected.
- `elect()` runs over the live peers of this hex's own partition. One Overseer per partition. The rules within a group are today's: the sitting Overseer stays, then most current, then rank.
- `check_quorum()` counts the hex's own group against `replication.quorum`, and `wait_for_acks` counts its group's replicas. A group that loses quorum stops writes for its slices only.
- Places that look for "the Overseer" get the one for a partition: forwarding (forward.rs:64), `post_to_overseer` (replication.rs:142), retire (retire.rs:85), the handover on updates (updates.rs:190), metrics lag (metrics.rs:290), file content placement (engine/files.rs:904) and slice placement among replicas (`wanted_slices`, replication.rs:1001), which picks only from the group's replicas.
- **Owned slices replace `ALL_SLICES`.** A partition Overseer holds its partition's mask. Every place in (3) compares against the owned mask, not all 64. The Oracle keeps no data and follows partition 0 only.

### Replication

- Within a group, replication is today's: replicas long-poll their Overseer's `/lattice/changes` and take its snapshot, both limited to the partition's slices plus its partition-local system tessellations. Slicing among a group's replicas works as now, within the partition's mask.
- **Every hex also follows partition 0's system tessellations.** Every hex authenticates requests and checks schemas, so every hex needs users, roles, API keys and the catalog. A second follower stream, `/lattice/changes?system=1`, comes from partition 0's Overseer. It's applied with its own `ReplicaCursor`, keyed by partition in `_replication`, as a replica applies one stream today: local sequence numbers, cursor saved in the same batch. Partition 0's own members get this from their main stream. The catalog (DDL) syncs from partition 0 as `sync_catalog` does now.
- `min_acks` applies per group to its own writes. Writes to system tessellations wait for partition 0's group.
- Failover stays within a group, and the guarantee is today's per partition: no acknowledged write lost while a quorum of the group survives.

### Routing writes

- The forwarding middleware decides by partition instead of "am I the Overseer". It maps `(tessellation, id)` to a slice, the slice to a partition, and the partition to its Overseer (held up to 30 s during a handover, as now). It forwards signed, with the epoch. A partition Overseer that gets a write for a slice it doesn't own answers 421 `partition_epoch` (stage 1's check, extended from "epochs differ" to "this slice isn't mine").
- **Inserts:** the routing hex generates the ULID and forwards an insert with that ID (an internal form of the existing client-chosen-ID insert), so the document goes to the partition its ID hashes to. A file upload routes the same way, with its ID generated first.
- **Bulk writes** are split by partition: one write per partition, each atomic within its partition, and the answer lists the outcome per document, as a bulk write's per-document results do now. A bulk write with an `Idempotency-Key` that spans partitions is refused in stage 2 (`cross_partition`), because a retry after a partial failure could apply part of it twice. It becomes a transaction in stage 3.
- **`update_where`, `delete_where`:** they fan out, running in each partition as its own commit, and the answer sums the counts. Not atomic across partitions. The answer says so (`"partitions": n`), and the manual says so.
- **Cascading deletes and enforced parent links** whose documents fall in different partitions are refused (`cross_partition`) in stage 2, before anything is written. A child and its parent rarely share a partition (their IDs hash independently), so in practice a multi-partition lattice can't use enforced links until stage 3. See "Decisions needed".
- **Transactions** compute the partition of every operation up front. If they span more than one, the transaction is refused with 409 `cross_partition` and nothing is written. `DeleteWhere` operations can't be placed up front, so they're refused on a multi-partition lattice.
- **Unique indexes:** creating one is refused on a multi-partition lattice, and so is turning partitioning on with one. Upserts keyed by a unique index go with them. They come back in stage 3 with reservation documents.
- **Queues** live in the partition their name hashes to. Queue calls route there, and dead-lettering to a queue in another partition is refused until stage 3.

### Reads

- Single-document reads route to the document's partition: a replica of its group, or its Overseer for `consistency=strong`.
- **Fan-out** happens at the routing hex, in the API layer. One request goes to a member of each partition (this hex for its own partition, a caught-up replica elsewhere), and the answers are merged:
  - Queries: each partition returns its top O+L by the sort, then a k-way merge, skip O, take L. The `after` cursor becomes an opaque token mapping each partition to its last ULID. `with_total` sums.
  - Counts sum.
  - Aggregations: a new internal partial mode returns the accumulators (count, sum, min, max, avg as sum and n, countDistinct as the value set within `limits.sql_max_mb`), combined at the router.
  - SQL: the single-table forms map to the above. Joins (`run_general`) scan each table through the fan-out query, so the join runs at the router within `sql_max_rows` and `sql_max_mb`.
  - Vector search: top k from each partition, merged by `_score`.
  - GraphQL lists, counts and aggregates use the same paths.
- A partition that doesn't answer within the request's deadline fails the read with 503 `partition_unavailable`, naming the partition. Never a partial answer.
- Row and field security is applied in each partition before results leave it, as today.

### The change feed

- The public `/changes` (and its SSE and WebSocket forms) on a multi-partition lattice merges the partitions' feeds. Each is read from its partition's Overseer, so sequence numbers are that partition's history, not a replica's local ones. That fixes (4).
- The cursor becomes a vector: base64url JSON of `{partition: [history_id, seq]}`. A plain number still means partition 0 on a one-partition lattice, so existing clients keep working. A history ID that changed (failover) is mapped with `Continuation` as replicas and plugins do now.
- Order holds within a partition, not across partitions. Each change carries `partition`.
- **Plugins, triggers and stream sources** run on every partition's Overseer, for that partition's changes, with cursors in that partition's local `_plugin_cursors`, `_trigger_cursors` and `_stream_offsets`. A plugin with acknowledgements acknowledges per partition (2.5.6's protocol, unchanged inside a partition). Schedules and functions run on partition 0's Overseer.

### Setting the map

- Stage 2 adds the map's writer: `PUT /status/partitions` (administrators, password re-check) and `hexdb partitions set`. Partition 0's Overseer commits a new map version with epoch + 1.
- **A multi-partition map is accepted only while the lattice holds no user documents.** Stage 2 doesn't move data. A slice that changed partitions would leave its documents in the wrong place, and moving them safely is stage 4. The check runs on partition 0's Overseer, under the commit lock, across every partition's counts, and is refused (409 `not_empty`) otherwise.
- Every hex must be on a version that understands partitions before a multi-partition map can be set, as the epoch rule already requires.

### Backups and point-in-time restore

- Each partition's Overseer backs up and ships its own partition. A multi-partition lattice backup is a set of per-partition backups taken at nearly the same time. It is **not** one consistent point: a restore can show partition A a moment ahead of partition B. Consistent cuts are stage 5.
- Restore rebuilds each partition from its own backup, plus the map. The admin UI and `hexdb backup` say the backups aren't mutually consistent until stage 5.

## Order of work

Each step is useful and testable alone, and the first two change nothing for one-partition lattices:

1. `partition_of` uses `is_system_tessellation`, and the partition-local system tessellations are defined. Tests: users and roles go to partition 0; the idempotency, vault and trash records of a write land with the write.
2. Owned slices replace `ALL_SLICES` everywhere listed. Tests: a one-partition lattice behaves as before (the whole suite).
3. Groups: `PeerHex.partition`, per-group elections, quorum and acknowledgements. Tests: a 2x3 lattice elects two Overseers, and failover in one group leaves the other alone.
4. Replication per group, plus the system stream from partition 0. Tests: a user created on partition 0 can sign in on partition 1's hexes, and a schema change reaches every group.
5. Write routing and the refusals (`cross_partition`, `not_empty`). Tests: every write kind lands in the right partition from any hex, and refused writes leave nothing behind.
6. Fan-out reads and merges. Tests: equivalence, the same data in one partition and in three gives the same answers for queries, paging, counts, aggregations, SQL (joins included) and vector search (property tests).
7. The merged change feed and per-partition plugins, triggers and stream sources. Tests: no change lost or doubled across an Overseer failover in one partition, and cursors from before the failover resume.
8. Backups per partition, docs, and release as a preview.

## Testing beyond the steps

- Fault injection at each routing and replication step: a partition Overseer killed mid-write, before and after the WAL append, with `min_acks`. The WAL pause hooks from 2.5.6 extend to name a partition.
- A randomized workload against a 2x3 lattice in CI on a schedule: single-document writes from every hex, failovers in each group, and a history check that every acknowledged write is readable afterwards and none appears twice.
- The whole existing suite runs with `partitioning.enabled = true` and one partition, to show nothing changed.

## Decisions (2026-11-23)

1. **Empty lattices only.** A multi-partition map can be set only before the lattice holds user documents, until stage 4 can move data.
2. **Enforced parent links and cascading deletes** across partitions are refused until stage 3.
3. **Preview.** Stage 2 ships off by default as a preview, not for production data, because backups of a multi-partition lattice aren't mutually consistent until stage 5.
