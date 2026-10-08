# HexDB drivers

Client libraries for HexDB's REST API, plus an Entity Framework Core provider and an ODBC driver (below). The Node.js, Python and .NET drivers cover the same ground:

- documents: insert, get, replace, patch, delete;
- queries with filters, sorting and paging;
- counts, aggregations and upserts;
- transactions and idempotency keys;
- the change feed;
- streams: publish, read, consumer groups;
- functions and GraphQL.

| Language | Folder | Package | Requirements |
|---|---|---|---|
| JavaScript / TypeScript | [node](node) | `hexdb` (npm) | Node.js 18+, Deno, Bun or a browser; no dependencies |
| Python | [python](python) | `hexdb` (PyPI) | Python 3.9+; standard library only |
| .NET (C#, F#) | [dotnet](dotnet) | `HexDB.Client` (NuGet) | .NET 8+ |
| Entity Framework Core | [dotnet/HexDB.EntityFrameworkCore](dotnet/HexDB.EntityFrameworkCore) | `HexDB.EntityFrameworkCore` (NuGet) | .NET 8+, EF Core 8 |
| ODBC | [odbc](odbc) | release archives (`odbc/`) | 64-bit Windows, Linux (unixODBC) |

The three drivers authenticate with an API key (create one on the admin UI's Account page) or by signing in with a login and password. They retry `429` and `503` answers for reads, and for writes that carry an idempotency key.

## Quick examples

```ts
import { HexDB } from "hexdb"
const db = new HexDB({ url: "http://127.0.0.1:7700", apiKey: process.env.HEXDB_API_KEY })
const orders = db.tessellation("orders")
await orders.insert({ customer: "ada", total: 12 })
const page = await orders.query({ filter: { total: { $gt: 10 } }, sort: "-total" })
```

```python
from hexdb import HexDB
db = HexDB("http://127.0.0.1:7700", api_key=os.environ["HEXDB_API_KEY"])
orders = db.tessellation("orders")
orders.insert({"customer": "ada", "total": 12})
page = orders.query(filter={"total": {"$gt": 10}}, sort="-total")
```

```csharp
using HexDB.Client;
using var db = new HexDBClient(new Uri("http://127.0.0.1:7700"), Environment.GetEnvironmentVariable("HEXDB_API_KEY"));
var orders = db.Tessellation("orders");
await orders.InsertAsync(new { customer = "ada", total = 12 });
var page = await orders.QueryAsync(new Query { Filter = new { status = "paid" }, Sort = "-total" });
```

## Entity Framework Core

`HexDB.EntityFrameworkCore` is an EF Core 8 provider over the .NET driver. Each entity type is a tessellation and each `SaveChanges` is one atomic transaction:

```csharp
protected override void OnConfiguring(DbContextOptionsBuilder options) =>
    options.UseHexDB("http://127.0.0.1:7700", Environment.GetEnvironmentVariable("HEXDB_API_KEY"));

db.Orders.Add(new Order { Customer = "ada", Total = 12 });   // string Id left null: a ULID is generated
await db.SaveChangesAsync();
var big = await db.Orders.Where(o => o.Total > 10).OrderByDescending(o => o.Total).ToListAsync();
```

Filters, sorting and paging run on the server; a query HexDB can't answer throws instead of running in memory. Relationships, owned types, inheritance and explicit transactions aren't supported yet. See the [manual](../MANUAL.md#entity-framework-core) for the mapping, the supported LINQ and concurrency tokens.

## Running the tests

Each suite runs against a real server. `testing/server.mjs` starts a throwaway one (from `target/debug/hexdb_api`, so run `cargo build -p hexdb_api` first) and sets `HEXDB_URL` and `HEXDB_API_KEY` for the command it runs:

```sh
cd drivers/node && npm install && npm test
node drivers/testing/server.mjs python -m unittest discover -s drivers/python/tests -t drivers/python
node drivers/testing/server.mjs dotnet test drivers/dotnet
```

## ODBC

The [ODBC driver](odbc) sends SQL to `POST /sql` (see the [manual](../MANUAL.md#10-sql)) over HTTP, so tools that read through ODBC can query HexDB. See [odbc/README.md](odbc/README.md) to install it and connect. There's no JDBC driver.
