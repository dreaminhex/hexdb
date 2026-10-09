# HexDB.Client

The .NET client for [HexDB](https://github.com/dreaminhex/hexdb), a document database with REST, GraphQL and read-only SQL, encryption at rest and replication with automatic failover.

It covers documents (insert, get, replace, patch, delete), queries with filters, sorting and paging, counts, aggregations, upserts, transactions, idempotency keys, the change feed, streams, functions and GraphQL. It retries `429` and `503` answers for reads, and for writes that carry an idempotency key.

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

For Entity Framework Core, use [HexDB.EntityFrameworkCore](https://www.nuget.org/packages/HexDB.EntityFrameworkCore). The [driver documentation](https://github.com/dreaminhex/hexdb/tree/main/drivers) and the [manual](https://github.com/dreaminhex/hexdb/blob/main/MANUAL.md) cover the rest. Licensed under the Apache License 2.0.
