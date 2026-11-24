# Design: point-in-time restore

Status: accepted for 2.7 (the user's decisions 2026-11-23: any S3-compatible store, encrypted before upload, 7-day default retention).

## Why

`POST /backup` writes a consistent copy to a folder, at the moment it runs. A lattice that loses its data between backups loses everything since the last one, and a backup on the same machines doesn't survive losing them. Point-in-time restore keeps a continuous copy off the lattice, in an S3-compatible store, from which the lattice can be rebuilt as it was at any moment within the retention window.

## What is shipped

Everything shipped is already encrypted with the lattice's storage key (WAL records, SSTables, metadata), or sealed before upload (file chunks). The store never sees a document, a name or a key.

1. **Base backups.** A lattice backup (`engine.backup`, scope `lattice`, so on a hex that holds everything) every `backup.continuous.base_hours` (24), uploaded file by file under `<prefix>/history/<history id>/base/<sequence>/`, with its `backup.json` manifest last, without its file content (shipped as packs, below). A base is usable once its manifest is there.
2. **WAL segments.** Every closed segment, under `<prefix>/history/<history id>/wal/<first sequence>.wal`, as soon as it's closed. The Overseer rotates the WAL when the open segment has records older than `backup.continuous.max_lag_seconds` (60), so at most about a minute of acknowledged writes isn't off the lattice yet. Segments are immutable once closed, so an upload is never repeated with other contents.
3. **File content, as pack files.** File chunks live in append-only pack files, one series per vertex; a pack is sealed at 256 MB and never changes again. Each pass uploads every sealed pack once (`<prefix>/history/<history id>/packs/vertex-<n>/<pack>`) and, of the open pack, the bytes appended since the last pass (`.../<pack>.<offset>`), after syncing the store. Packs go up before the WAL segments of the same pass, and a chunk is durable before the commit that refers to it, so every shipped segment's chunks are already shipped. The bytes are the pack's own: fragments encrypted with the content key, Reed-Solomon coded. (Chunks can't be tracked one by one across restarts: the store's generation counter lives in memory.)

A small `<prefix>/history/<history id>/state.hxe` (sealed with the storage key) records the newest uploaded segment and base, so a new Overseer continues where the last one stopped; it's also rebuilt from a listing if missing.

## Who ships

The Overseer, in a background task at low priority (like flushing). After a failover the new Overseer reads the state and uploads what's missing: segments it holds that aren't in the store (a new Overseer starts its own history, with its own segment numbers under `<prefix>/history/<history id>/`, so two Overseers never write the same names), and a base backup at once, since its history is new.

Uploads use any S3-compatible API (AWS S3, Cloudflare R2, Backblaze B2, MinIO, Tigris, Google Cloud Storage's interoperability API): PUT, GET, ListObjectsV2 and DELETE, signed with AWS Signature Version 4. HexDB implements SigV4 itself on the hashing and HTTP libraries it already has (the common Rust object-store crates bring back chrono, which 2.6 removed).

## Configuration

Lattice configuration (encrypted, versioned, audited; secrets write-only):

| Key | Default | Meaning |
| --- | --- | --- |
| `backup.continuous.enabled` | false | Ship to the store |
| `backup.continuous.endpoint` | empty | e.g. `https://s3.us-east-1.amazonaws.com`, `https://<account>.r2.cloudflarestorage.com` |
| `backup.continuous.bucket`, `prefix`, `region` | empty, the lattice's name, `us-east-1` | Where |
| `backup.continuous.access_key_id`, `secret_access_key` | empty | Credentials (secret) |
| `backup.continuous.retention_days` | 7 | How far back a restore can go |
| `backup.continuous.base_hours` | 24 | How often a base backup is taken |
| `backup.continuous.max_lag_seconds` | 60 | Most acknowledged writes may wait before they're shipped |

`GET /backups/continuous` (the `maintenance` permission) reports the newest shipped segment and its time, the oldest moment a restore can reach, the last base, the lag, and the last error. The dashboard shows the lag; it raises an alert when shipping has failed for 10 minutes.

## Restore

`hexdb restore --from <endpoint/bucket/prefix> --until <RFC 3339 time> --to <empty folder>` (credentials from the environment, the storage key from the configuration as for any restore):

1. Lists the bases and picks the newest whose sequence's time is at or before `--until`.
2. Downloads it, then every segment after it, in order, up to the first record later than `--until`. That segment is cut after the last record at or before it: a record is one commit, so a transaction is never cut in half.
3. Downloads every pack shipped in that history: a sealed one whole, an open one by joining its pieces in offset order (a gap is an error). Chunks written after `--until` come along unused; garbage collection removes them.
4. Writes a fresh catalog history ID (as any restore does), so replicas and plugins treat it as a new history.

The folder then starts as a hex, as a restored backup does today. `--until` defaults to the newest moment available.

## Retention

The Overseer deletes bases older than the retention window, except the newest one older than it (a restore at the window's start needs it), and the segments only deleted bases needed. A pack's objects are deleted once the pack itself was deleted on the lattice (garbage collection) before the oldest kept base: no restore point can need it then, since its live chunks had moved to packs shipped since. Histories of earlier Overseers are deleted whole once nothing in them is newer than the window. Deleting is the last step of a pass, after uploads.

While shipping is on, the Overseer keeps every WAL segment it hasn't shipped: retired segments go to the archive even with `storage.change_history_hours = 0`, and the archive's age and size limits never remove one that isn't in the store yet.

## Testing

- SigV4 against AWS's published test vectors.
- Against MinIO in Docker (CI): ship, write, fail over mid-shipping, restore to several moments, and compare documents and file contents with what the lattice held at each moment.
- Cutting: a restore at a time inside a transaction's record includes it or not, never half.
