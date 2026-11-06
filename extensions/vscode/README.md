# HexDB for VS Code

Work with [HexDB](https://hexdb.app) lattices from VS Code: see every hex and where it runs, browse tessellations, run SQL and GraphQL, and edit stored functions.

## Connecting

Open the HexDB view in the activity bar and choose **Add Connection**. Paste a connection URI:

```text
hexdbs://db.example.com?apiKey=hxk_...      HTTPS, with an API key
hexdbs://analyst:s3cret%21@db.example.com   HTTPS, signing in with a login and password
hexdb://127.0.0.1:7700                      HTTP; you're asked how to sign in
```

An `http://` or `https://` address works too. The same URI works with every HexDB driver; see [Connection URIs](https://github.com/dreaminhex/hexdb/blob/main/MANUAL.md#connection-uris) in the manual. Accounts with MFA are asked for a one-time code when they sign in. Connections are tested before they're saved, and their credentials are kept in VS Code's secret storage, not in settings.

**Connect to the Playground** adds the public demo lattice (four hexes in four regions, read-only apart from its `sandbox` tessellation, reset every hour).

## What you can do

- **Lattice.** Each connection shows its lattice: every hex with its role (Overseer, Harvester, Replicant), status and region. **Show Lattice** opens a card per hex, with its location, replication state, lag, slices and version, refreshed every few seconds. "Connected" marks the hex that answered your connection.
- **Tessellations.** Each tessellation lists its columns, marking indexed ones. **Browse Documents** shows its documents a page at a time. Click a document's `id` to open it as JSON; with write permission, saving it (Ctrl+S) replaces the document.
- **SQL and GraphQL.** The SQL and GraphQL buttons on a connection's row (or **New SQL/GraphQL Query on This Tessellation** when you right-click a tessellation) open an editor bound to the connection (shown in the status bar; click it to change). Ctrl+Enter (Cmd+Enter) runs the statement at the cursor, or the selection. Saved `.sql`, `.graphql` and `.gql` files work too. SQL editors complete tessellation and column names.
- **Results.** Each run opens a tab in the results panel, with a table or JSON view, a row filter, Load More for the next page, copying, and export to CSV or JSON.
- **Functions.** Run any function, giving its parameters. Administrators can create functions (Python, TypeScript or JavaScript scripts, or query, aggregate and transaction functions), edit a script's code or any function's JSON definition, save with Ctrl+S, and delete them.
- **Quarantine.** Administrators of HexDB 2.4 or later see, under each connection, the uploads the malware scan (ClamAV and YARA) flagged: what matched, the engines that scanned it, who uploaded it and when. Click one for its full record. **Release** stores a false positive as a normal file in its tessellation; **Delete** removes it for good; **Rescan Stored Files** scans everything again. The HexDB icon in the activity bar shows how many files are waiting, and a notification says when a new one is quarantined. Signed-in connections are checked every minute; nothing signs in on its own.

What you see and can change follows your HexDB permissions: tessellations you can't read aren't listed, documents you can't write open read-only, only administrators can change functions, and only administrators see the quarantine.

## Settings

| Setting | Default | Meaning |
| --- | --- | --- |
| `hexdb.pageSize` | 200 | Rows fetched per page of SQL results |
| `hexdb.latticeRefreshSeconds` | 5 | How often an open lattice view refreshes; 0 turns it off |

## Development

```sh
npm install
npm test                  # type checks, builds, unit tests
npm run test:integration  # runs inside VS Code against the public playground (or HEXDB_TEST_URI)
npm run package           # builds hexdb-<version>.vsix
```

Press F5 in VS Code with this folder open to start an Extension Development Host. The extension bundles the JavaScript driver from `drivers/node`.

Licensed under the Apache License 2.0.
