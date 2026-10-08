# HexDB ODBC driver

An ODBC 3.8 driver for HexDB, written in Rust, for applications that read data through ODBC (BI and reporting tools, spreadsheets, Python's pyodbc, R's odbc, and others). The driver sends SQL to the server's [`POST /sql`](../../MANUAL.md#10-sql) endpoint over HTTP or HTTPS and reads table and column lists from `GET /sql/tables` and `GET /sql/columns`. It's read-only, like HexDB's SQL.

It's tested with pyodbc through unixODBC on Linux and through the Windows driver manager (in CI). Desktop tools such as Excel, Power BI and Tableau haven't been tested with it yet.

## Build

```sh
cargo build --release -p hexdb_odbc
```

This produces `target/release/hexdb_odbc.dll` on Windows, `libhexdb_odbc.so` on Linux, and `libhexdb_odbc.dylib` on macOS. Release archives include it in their `odbc` folder. The driver is 64-bit and needs no other libraries.

## Install

**Windows.** From an elevated (Run as administrator) 64-bit PowerShell:

```powershell
.\install-windows.ps1 -Dll .\hexdb_odbc.dll
# optionally with a data source:
.\install-windows.ps1 -Dll .\hexdb_odbc.dll -Dsn HexDB -Server http://127.0.0.1:7700
# remove the driver and its data sources:
.\install-windows.ps1 -Uninstall
```

The script:
- copies the DLL to `C:\Program Files\HexDB\ODBC`;
- registers it as `HexDB` with the Windows ODBC driver manager;
- with `-Dsn`, creates a user DSN (or a system DSN with `-DsnScope System`).

The driver has no configuration dialog. Create data sources with the script, or connect with a connection string.

**Linux (unixODBC).** Install `unixodbc`, copy the library somewhere permanent, and register it in `/etc/odbcinst.ini`:

```ini
[HexDB]
Description = HexDB ODBC driver
Driver = /usr/local/lib/libhexdb_odbc.so
```

A data source in `/etc/odbc.ini` or `~/.odbc.ini`:

```ini
[hexdb]
Driver = HexDB
Server = http://127.0.0.1:7700
```

unixODBC also accepts the library path directly: `Driver=/usr/local/lib/libhexdb_odbc.so;Server=...`.

**macOS.** Install a driver manager (`brew install unixodbc`), then register `libhexdb_odbc.dylib` as on Linux. The release workflow builds the macOS library, but it hasn't been tested with a driver manager.

## Connect

```text
Driver={HexDB};Server=http://127.0.0.1:7700;ApiKey=hxk_...
DSN=hexdb;ApiKey=hxk_...
Driver={HexDB};Server=https://db.example.com;UID=analyst;PWD=...
```

| Key | Meaning |
| --- | --- |
| `Server` | The server's address: `http://host:port` or `https://host:port`. A bare `host:port` means `http://` |
| `ApiKey` (or `Token`) | An API key (create one on the admin UI's Account page) or a session token |
| `UID`, `PWD` | Sign in with a login and password instead. Accounts with MFA need an API key |
| `DSN` | A data source to read the other keys from; keys in the connection string win |
| `PageSize` | Rows per request, 1 to 10,000 (default 1,000) |
| `MaxStringLength` | The size reported for text columns (default 4,000). Longer values are still returned in full through `SQLGetData`; raise it for tools that size buffers from it |
| `Timeout` | Seconds to wait for each request (default 60) |
| `CAFile` | A PEM file of certificates to trust for HTTPS instead of the system's trust store |

The completed connection string the driver returns to applications leaves out `ApiKey` and `PWD`, so tools that save connection strings don't store credentials.

## What it supports

- **Queries.**
  - Any statement `POST /sql` accepts (see the [manual](../../MANUAL.md#10-sql)), with `?` parameters bound through `SQLBindParameter`.
  - Results arrive a page at a time as the application fetches.
  - Forward-only cursors; `SQLFetch` and `SQLFetchScroll(SQL_FETCH_NEXT)` with row arrays (column-wise or row-wise binding); `SQLGetData` in pieces for long values.
- **ODBC escape sequences:** `{d '...'}`, `{t '...'}` and `{ts '...'}` become string literals, because HexDB stores dates as text; `{fn f(...)}` becomes `f(...)`; `{escape 'c'}` becomes `ESCAPE 'c'`.
- **Types.**

  | HexDB values | Reported as |
  | --- | --- |
  | Booleans | `SQL_BIT` |
  | Integers | `SQL_BIGINT` |
  | Other numbers | `SQL_DOUBLE` |
  | Text | `SQL_WVARCHAR` |
  | Objects and arrays | `SQL_WLONGVARCHAR`, holding JSON text |

  A column's type comes from the tessellation's schema when it declares one; otherwise it's inferred from the first page of results. Declare a schema for types that don't vary.
- **Conversions.**
  - Values convert to the usual C types: character, wide character, integers, floating point, bit and binary.
  - Date, time and timestamp structures are parsed from ISO 8601 text.
- **Catalog.**
  - `SQLTables` lists the tessellations you can read; they belong to no catalog or schema.
  - `SQLColumns` describes their columns.
  - `SQLPrimaryKeys` and `SQLSpecialColumns` name `id`.
  - `SQLGetTypeInfo` lists the five types above.
  - Foreign keys, statistics and procedures come back empty.
- **Diagnostics.** Errors carry HexDB's message and an SQLSTATE:

  | SQLSTATE | Meaning |
  | --- | --- |
  | `42000` | SQL HexDB can't run, or a permission refusal |
  | `42S02` | A missing tessellation |
  | `28000` | Bad credentials |
  | `08001` | The server can't be reached |
  | `07002` | An unbound parameter |

## Limits

- **Read-only.** There are no transactions to manage: `SQLEndTran` does nothing, and autocommit stays on.
- **Cursors.** No scrollable cursors, bookmarks, positioned updates, asynchronous execution or arrays of parameters.
- **API surface.** Only the Unicode entry points are exported; driver managers convert ANSI calls. Explicit descriptor handles and descriptor functions aren't supported.
- **Text encoding.** `SQL_C_CHAR` data is UTF-8. Applications that expect the Windows ANSI code page should ask for wide characters (`SQL_C_WCHAR`), which most do.

## Tests

- **Direct calls.** `cargo test -p hexdb_odbc` calls the driver's functions directly, the way a driver manager does, against a throwaway server.
- **Through a driver manager.** `drivers/odbc/tests/pyodbc_smoke.py` goes through a real driver manager with pyodbc:

  ```sh
  cargo build -p hexdb_api -p hexdb_odbc
  node drivers/testing/server.mjs python3 drivers/odbc/tests/pyodbc_smoke.py
  ```

  On Linux it also registers the driver and a DSN in temporary `odbcinst.ini` and `odbc.ini` files. On Windows, install the driver first and set `HEXDB_ODBC_DRIVER=HexDB`.
- **CI.** Runs both: unixODBC on Linux, and the Windows driver manager after installing the driver with the script.
