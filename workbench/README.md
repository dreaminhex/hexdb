# HexDB Workbench

A desktop app for people who work with several HexDB lattices: development, staging, production. Pilot (0.1).

- **Environments.** Each is one lattice: a name, a color, its address, and optionally an SSH tunnel. The color frames the environment's window and fills a banner across its top, so production never looks like development.
- **The admin UI, in its own window.** Opening an environment shows that lattice's own admin UI, the version that matches the server. Each environment's window keeps its own cookies and storage, so signing in to one never signs you in to another. The lattice's pages get no access to the workbench.
- **SSH tunnels with host-key checks.** For lattices that only listen on a private address. The system's OpenSSH client opens the tunnel, so your keys, agent and `~/.ssh/config` work as they do in a terminal. Host keys are always checked against your `known_hosts` and the workbench's own. **Host key** shows a server's fingerprints, and the workbench trusts it only after you confirm they match what its administrator gave you. ssh never prompts.
- **API keys in the OS keychain.** Windows Credential Manager, the macOS Keychain, or the Secret Service on Linux. Never in the workbench's settings file (`environments.json` in the app's config folder).
- **Schema and index compare.** Shows which tessellations each of two lattices has, and where their kinds, index definitions or current schemas differ. Read-only. Each side needs an API key with read access.

Not yet: auto-update, store listings, a CLI.

## Building

Needs Node.js 22 and Rust. On Linux, install `libwebkit2gtk-4.1-dev librsvg2-dev libdbus-1-dev pkg-config`. Windows 10+ and macOS 14+ have their webviews built in.

```bash
cd workbench
npm install
npx tauri dev          # run it, reloading the UI as you edit
npx tauri build        # installers in src-tauri/target/release/bundle/
```

The Rust side (`src-tauri/`) is its own Cargo workspace, kept out of the server's so building HexDB never needs the desktop libraries: `cargo clippy --all-targets` and `cargo test` there.

## Releases

Tag `workbench-vX.Y.Z` and the Workbench workflow builds installers for Windows (MSI and setup), macOS (a universal DMG) and Linux (AppImage, deb, rpm). It signs them when the signing secrets are set (see the workflow's header) and publishes a prerelease with `SHA256SUMS`, signed with the project's packaging key. Without the Windows and Apple certificates the installers are unsigned, and the workflow warns about it.

## Known advisories

- **glib 0.18 (GHSA-wrw7-89jp-8q8g)**, Linux builds only: unsoundness in `glib::VariantStrIter`. It comes in through the GTK3 bindings that Tauri's Linux backend uses (wry, tao, webkit2gtk, muda). The fix is only in glib 0.20, which has no GTK3 bindings, so no update can reach it until Tauri moves on, and Dependabot's security update for it fails for that reason. Nothing in the workbench or its dependencies calls `VariantStrIter` (checked 2026-11-23). Check again when Tauri updates its Linux backend.
