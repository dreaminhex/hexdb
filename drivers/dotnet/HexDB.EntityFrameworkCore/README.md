# HexDB.EntityFrameworkCore

An Entity Framework Core 8 provider for [HexDB](https://github.com/dreaminhex/hexdb), built on [HexDB.Client](https://www.nuget.org/packages/HexDB.Client). Each entity type maps to a tessellation, LINQ filters, sorting and paging run on the server, and each `SaveChanges` is sent as one atomic transaction, with concurrency tokens becoming preconditions.

```sh
dotnet add package HexDB.EntityFrameworkCore
```

```csharp
protected override void OnConfiguring(DbContextOptionsBuilder options) =>
    options.UseHexDB("http://127.0.0.1:7700", Environment.GetEnvironmentVariable("HEXDB_API_KEY"));

db.Orders.Add(new Order { Customer = "ada", Total = 12 });   // string Id left null: a ULID is generated
await db.SaveChangesAsync();
var big = await db.Orders.Where(o => o.Total > 10).OrderByDescending(o => o.Total).ToListAsync();
```

A query HexDB can't answer throws instead of quietly loading everything into memory. Relationships, owned types, inheritance, composite keys and explicit transactions aren't supported yet. The [manual](https://github.com/dreaminhex/hexdb/blob/main/MANUAL.md#entity-framework-core) covers the mapping, the supported LINQ and concurrency tokens.

Requires .NET 8 or later and EF Core 8. Licensed under the Apache License 2.0.
