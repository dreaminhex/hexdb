# HexDB.Client

The .NET client for [HexDB](https://github.com/dreaminhex/hexdb), a document database with REST, GraphQL and read-only SQL, encryption at rest and replication with automatic failover.

It covers documents (insert, get, replace, patch, delete), queries with filters, sorting and paging, counts, aggregations, upserts, detokenizing vault tokens, transactions, idempotency keys, the change feed, streams, functions and GraphQL. It retries `429` and `503` answers for reads, and for writes that carry an idempotency key.

```sh
dotnet add package HexDB.Client
```

```csharp
using HexDB.Client;

using var db = new HexDBClient(new Uri("http://127.0.0.1:7700"), Environment.GetEnvironmentVariable("HEXDB_API_KEY"));
var orders = db.Tessellation("orders");
await orders.InsertAsync(new { customer = "ada", total = 12 });
var page = await orders.QueryAsync(new Query { Filter = new { status = "paid" }, Sort = "-total" });
```

Create an API key on the admin UI's Account page. Requires .NET 8 or later.

## Connection URIs

The client also takes a HexDB connection URI, the same string every HexDB driver accepts:

```csharp
using var db = new HexDBClient("hexdbs://db.example.com?apiKey=hxk_...");
using var signedIn = await HexDBClient.ConnectAsync("hexdbs://analyst:s3cret%21@db.example.com");
```

`hexdb://` is HTTP (port 7700 by default) and `hexdbs://` is HTTPS (port 443 by default). A path is kept as the base of every request. The client uses the `apiKey` and `timeout` options, and ignores options meant for other drivers. `ConnectAsync` signs in before it returns when the URI has a login. The constructor signs in on the first request instead, so a wrong password shows up there. Plain `http://` and `https://` URLs work too, and an `apiKey` argument is used instead of the URI's. `HexDBConnectionUri.Parse` checks a URI without connecting, and its `ToString()` hides the password and API key.

For Entity Framework Core, use [HexDB.EntityFrameworkCore](https://www.nuget.org/packages/HexDB.EntityFrameworkCore). The [driver documentation](https://github.com/dreaminhex/hexdb/tree/main/drivers) and the [manual](https://github.com/dreaminhex/hexdb/blob/main/MANUAL.md) cover the rest. Licensed under the Apache License 2.0.
