# HexDB JDBC driver

A JDBC 4.2 driver for [HexDB](https://github.com/dreaminhex/hexdb)'s read-only SQL, for Java tools and applications: DBeaver, IntelliJ and DataGrip, Spark, Jupyter (via JayDeBeApi), reporting tools, and plain JDBC code. It sends SQL to `POST /sql` over HTTP or HTTPS, so joins, subqueries, `UNION`, `WITH` and expressions work as described in the [manual](../../MANUAL.md#10-sql), with the caller's permissions, row filters and hidden fields.

It needs Java 11 or later and has no dependencies.

## Install

- **Maven Central** (`com.dreaminhex:hexdb-jdbc`), once published:

  ```xml
  <dependency>
    <groupId>com.dreaminhex</groupId>
    <artifactId>hexdb-jdbc</artifactId>
    <version>2.1.3</version>
  </dependency>
  ```

- **Release download.** Each [GitHub release](https://github.com/dreaminhex/hexdb/releases) has `hexdb-jdbc-<version>.jar`. In a database tool, add it as a driver with the class `com.dreaminhex.hexdb.jdbc.HexDBDriver` and the URL template `jdbc:hexdb://{host}:{port}`.
- **From source:** `mvn -f drivers/java package` writes `drivers/java/target/hexdb-jdbc-<version>.jar`.

## Connect

```java
Properties props = new Properties();
props.setProperty("apiKey", System.getenv("HEXDB_API_KEY"));
try (Connection db = DriverManager.getConnection("jdbc:hexdb://127.0.0.1:7700", props);
     PreparedStatement q = db.prepareStatement(
         "SELECT c.name, COUNT(*) AS orders, SUM(o.total) AS spent "
       + "FROM orders o JOIN customers c ON o.customer = c.code "
       + "WHERE o.status = ? GROUP BY c.name ORDER BY spent DESC")) {
    q.setString(1, "paid");
    try (ResultSet r = q.executeQuery()) {
        while (r.next()) {
            System.out.println(r.getString("name") + ": " + r.getInt("orders") + ", " + r.getBigDecimal("spent"));
        }
    }
}
```

**URLs.**
- `jdbc:hexdb://host:port` connects over HTTP; add `?ssl=true` for HTTPS.
- `jdbc:hexdb:https://host:port` (or `http://`) names the scheme directly.
- Properties can go in the URL (`?apiKey=...&pageSize=500`) or in the `Properties` object, which wins.

**Properties.**

| Property | Meaning |
| --- | --- |
| `apiKey` | An API key (create one on the admin UI's Account page). |
| `user`, `password` | Sign in with a login instead. Accounts with MFA need an API key. `DriverManager.getConnection(url, user, password)` sets these. |
| `ssl` | `true` for HTTPS with `jdbc:hexdb://`. |
| `caFile` | A PEM file of CA certificates to trust, besides the JVM's own (for a lattice with a private CA). |
| `pageSize` | Rows per request, 1 to 10,000 (default 1,000). `Statement.setFetchSize` overrides it per statement. |
| `timeout` | Seconds a request may take (default 60). |
| `connectTimeout` | Seconds to connect (default 10). |

## What it supports

- **Statements.** `Statement` and `PreparedStatement` with `?` parameters, which are sent as values with the statement, never spliced into it. `setMaxRows`, `setFetchSize` and `closeOnCompletion` work.
- **Results.** Forward-only and read-only, fetched a page at a time as you read.
  - Getters convert: numbers read as any numeric type or as text, `getBoolean` reads booleans, `0`/`1` and `true`/`false`, objects and arrays read as JSON text.
  - Dates are ISO 8601 strings in HexDB. `getDate`, `getTime`, `getTimestamp` and `getObject(i, LocalDate.class)` (also `LocalDateTime`, `OffsetDateTime`, `Instant`) parse them.
  - `wasNull` reports SQL NULLs.
- **Types.** Booleans are `BOOLEAN`, integers `BIGINT`, other numbers `DOUBLE`, text `VARCHAR`, and objects and arrays `LONGVARCHAR` (type name `JSON`). A column's type comes from the values HexDB returns.
- **Metadata.** `getTables` lists the tessellations you can read, as tables in no catalog or schema. `getColumns` lists each one's columns, from its schema or from sampled documents. `getPrimaryKeys` gives `id`. `getTypeInfo` and the capability methods (outer joins, subqueries, unions, the function lists) describe HexDB's SQL. Metadata HexDB doesn't have, such as procedures and foreign keys, comes back empty with the standard columns.
- **Escapes.** JDBC escape sequences are rewritten: `{d '...'}`, `{t '...'}` and `{ts '...'}` become string literals, `{fn f(...)}` becomes `f(...)`, `{oj ...}` becomes the join inside it, and `{escape 'c'}` becomes `ESCAPE 'c'`.
- **Errors.** `SQLException`s carry HexDB's message and an SQLSTATE: `42000` for SQL HexDB can't run or a permission refusal, `42S02` for a missing tessellation, `28000` for bad credentials, `08001` when the server can't be reached, `07002` for an unset parameter, and `0A000` for a JDBC feature the driver doesn't implement.

## Limits

- **Read-only.** `executeUpdate` throws. There are no transactions: autocommit stays on, and `commit`, `rollback` and `setAutoCommit(false)` are accepted and do nothing, so tools that call them keep working.
- **Cursors.** No scrollable or updatable result sets, batches, generated keys, stored procedures or BLOBs.
- **Cancelling.** Each page is one short request, so `cancel` has nothing to stop; the `timeout` property bounds every request.

## Tests

```sh
cargo build -p hexdb_api
node drivers/testing/server.mjs mvn -f drivers/java test
```

`server.mjs` starts a throwaway server and sets `HEXDB_URL`, `HEXDB_API_KEY`, `HEXDB_ADMIN_LOGIN` and `HEXDB_ADMIN_PASSWORD`. Without them, the tests that need a server are skipped. CI runs them on every push.

The `Unsupported*` classes are generated from the JDK's `java.sql` interfaces by [tools/generate_stubs.py](tools/generate_stubs.py) (instructions inside). They give every method HexDB's driver doesn't implement a body that says so.

Licensed under the Apache License 2.0.
