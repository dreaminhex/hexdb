# HexDB.EntityFrameworkCore

An Entity Framework Core 9 provider for [HexDB](https://github.com/dreaminhex/hexdb), built on [HexDB.Client](https://www.nuget.org/packages/HexDB.Client). Each entity type maps to a tessellation, LINQ filters, sorting and paging run on the server, and each `SaveChanges` is sent as one atomic transaction, with concurrency tokens becoming preconditions. Relationships (with `Include`), owned types (nested in the document), inheritance (one tessellation per hierarchy) and explicit transactions work too.

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

A query HexDB can't answer throws instead of quietly loading everything into memory. Many-to-many skip navigations, filtered includes, complex types and composite keys aren't supported yet. The [manual](https://github.com/dreaminhex/hexdb/blob/main/MANUAL.md#entity-framework-core) covers the mapping, relationships, owned types, inheritance, the supported LINQ, transactions and concurrency tokens.

Requires .NET 8 or later and EF Core 9. Licensed under the Apache License 2.0.
