# Vendored crates

HexDB carries patched copies of these crates, wired in with `[patch.crates-io]` in the workspace `Cargo.toml` and excluded from the workspace (they aren't linted or tested with HexDB's own crates).

## nom-exif 3.8.0

- Upstream: https://github.com/mindeng/nom-exif (MIT, see `nom-exif/LICENSE`).
- Used by `hexdb_core::extract` for EXIF in images and track metadata in MP4, QuickTime and Matroska files.
- Why: upstream depends on chrono, which is unmaintained (RUSTSEC-2026-0543), with no feature to turn it off.
- Changes:
  - `src/time.rs` (new): `OffsetDateTime` and `NaiveDateTime` on top of jiff. They keep chrono's behaviour where callers see it: values compare by instant, `to_rfc3339` prints what chrono printed (a fraction only when there is one; `+00:00`, never `Z`), and parsing accepts what chrono's `%+` and `%Y:%m:%d %H:%M:%S %z` accepted.
  - `src/values.rs`, `src/bbox/mvhd.rs`, `src/ebml/webm.rs`, `src/mov.rs`: chrono types replaced by those. `EntryValue::DateTime` holds an `OffsetDateTime`, `EntryValue::NaiveDateTime` a `jiff::civil::DateTime`. `MvhdBox::creation_time_local` (unused) is gone.
  - Upstream's unit tests and doc examples are removed or not built: they need sample files that aren't in the published crate, and chrono as a dev-dependency. `src/time.rs` has tests of its own (`cargo test --manifest-path vendor/nom-exif/Cargo.toml`).
- Checked: `hexdb_core`'s extraction tests give the same results with this copy and with upstream 3.8.0 (dates in EXIF, WebM and the rest).
- Last compared with upstream: 2026-11-23, upstream's main branch was still at the v3.8.0 tag (no fixes to port, still on chrono). The copy uses jiff 1.0 since HexDB 2.7.
- Upkeep: when upstream releases a fix (it parses untrusted files), port it here, or drop this copy once upstream no longer needs chrono. Review by 2027-03-31.
