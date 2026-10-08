// Runs the EF Core provider against a real server:
//   node drivers/testing/server.mjs dotnet test drivers/dotnet
// HEXDB_URL and HEXDB_API_KEY name the server (set by server.mjs); without
// them the tests are skipped.

using System.ComponentModel.DataAnnotations;
using HexDB.Client;
using Microsoft.EntityFrameworkCore;
using Xunit;

public enum OrderStatus
{
    New,
    Paid,
    Shipped,
}

public class Order
{
    // Left null: EF Core generates a key (a ULID) only when it has its default value.
    public string Id { get; set; } = null!;
    public string Customer { get; set; } = "";
    public double Total { get; set; }
    public OrderStatus Status { get; set; }
    public DateTime PlacedAt { get; set; }
    public bool Rush { get; set; }
    public List<string> Tags { get; set; } = new();
    public string? Note { get; set; }
}

public class Customer
{
    public Guid Id { get; set; }
    public string Name { get; set; } = "";
    public string Email { get; set; } = "";
    [ConcurrencyCheck] public int Version { get; set; }
}

public class ShopContext : DbContext
{
    public DbSet<Order> Orders => Set<Order>();
    public DbSet<Customer> Customers => Set<Customer>();

    protected override void OnConfiguring(DbContextOptionsBuilder options) =>
        options.UseHexDB(Environment.GetEnvironmentVariable("HEXDB_URL")!, Environment.GetEnvironmentVariable("HEXDB_API_KEY"), o => o.PageSize(7));

    protected override void OnModelCreating(ModelBuilder model)
    {
        model.Entity<Order>().ToTessellation("ef_orders");
        model.Entity<Customer>().ToTessellation("ef_customers").Property(c => c.Email).ToJsonProperty("email");
    }
}

// The tests share tessellations, so they run one at a time.
[Collection("ef")]
public class EntityFrameworkTests
{
    private static readonly bool Configured = Environment.GetEnvironmentVariable("HEXDB_URL") is not null && Environment.GetEnvironmentVariable("HEXDB_API_KEY") is not null;

    private static async Task<List<Order>> Seed()
    {
        await using var db = new ShopContext();
        await db.Database.EnsureDeletedAsync();
        await db.Database.EnsureCreatedAsync();
        var start = new DateTime(2026, 1, 1, 0, 0, 0, DateTimeKind.Utc);
        var orders = Enumerable.Range(0, 30).Select(i => new Order
        {
            Customer = i % 3 == 0 ? "ada" : i % 3 == 1 ? "grace" : "alan",
            Total = i * 10.5,
            Status = (OrderStatus)(i % 3),
            PlacedAt = start.AddDays(i),
            Rush = i % 5 == 0,
            Tags = i % 2 == 0 ? new() { "even", "t" + i } : new() { "odd" },
            Note = i % 4 == 0 ? null : "note " + i,
        }).ToList();
        db.Orders.AddRange(orders);
        Assert.Equal(30, await db.SaveChangesAsync());
        Assert.All(orders, o => Assert.Equal(26, o.Id.Length));
        return orders;
    }

    [SkippableFact]
    public async Task QueriesTranslateToHexDB()
    {
        Skip.IfNot(Configured, "set HEXDB_URL and HEXDB_API_KEY");
        var seeded = await Seed();
        using var db = new ShopContext();

        Assert.Equal(30, db.Orders.Count());
        Assert.Equal(30L, db.Orders.LongCount());
        Assert.Equal(10, db.Orders.Count(o => o.Customer == "ada"));
        Assert.True(db.Orders.Any(o => o.Total > 300));
        Assert.False(db.Orders.Any(o => o.Total > 1000));

        // Comparisons, && and ||, enums, dates, booleans, nulls.
        var paid = db.Orders.Where(o => o.Status == OrderStatus.Paid && o.Total >= 100).ToList();
        Assert.Equal(seeded.Count(o => o.Status == OrderStatus.Paid && o.Total >= 100), paid.Count);
        Assert.Equal(seeded.Count(o => o.Customer == "ada" || o.Rush), db.Orders.Count(o => o.Customer == "ada" || o.Rush));
        var since = new DateTime(2026, 1, 20, 0, 0, 0, DateTimeKind.Utc);
        Assert.Equal(seeded.Count(o => o.PlacedAt >= since), db.Orders.Count(o => o.PlacedAt >= since));
        Assert.Equal(6, db.Orders.Count(o => o.Rush));
        Assert.Equal(24, db.Orders.Count(o => !o.Rush));
        Assert.Equal(8, db.Orders.Count(o => o.Note == null));
        Assert.Equal(22, db.Orders.Count(o => o.Note != null));

        // Strings, $in, and collection contains.
        Assert.Equal(10, db.Orders.Count(o => o.Customer.StartsWith("gr")));
        Assert.Equal(20, db.Orders.Count(o => o.Customer.Contains("a") && !o.Customer.Contains("grace")));
        var names = new[] { "ada", "alan" };
        Assert.Equal(20, db.Orders.Count(o => names.Contains(o.Customer)));
        Assert.Equal(15, db.Orders.Count(o => o.Tags.Contains("even")));

        // Sorting and paging (the page size is 7, so this reads several pages).
        var page = db.Orders.OrderByDescending(o => o.Total).Skip(5).Take(10).ToList();
        Assert.Equal(seeded.OrderByDescending(o => o.Total).Skip(5).Take(10).Select(o => o.Id), page.Select(o => o.Id));
        var sorted = db.Orders.OrderBy(o => o.Customer).ThenByDescending(o => o.PlacedAt).ToList();
        Assert.Equal(seeded.OrderBy(o => o.Customer, StringComparer.Ordinal).ThenByDescending(o => o.PlacedAt).Select(o => o.Id), sorted.Select(o => o.Id));
        Assert.Equal(30, db.Orders.ToList().Count);

        // Single results, projections, Find.
        Assert.Equal(seeded[3].Id, db.Orders.Single(o => o.Id == seeded[3].Id).Id);
        Assert.Null(db.Orders.FirstOrDefault(o => o.Customer == "nobody"));
        Assert.Throws<InvalidOperationException>(() => db.Orders.Single(o => o.Customer == "ada"));
        Assert.Equal(0.0, db.Orders.OrderBy(o => o.Total).First().Total);
        var totals = db.Orders.Where(o => o.Customer == "alan").OrderBy(o => o.Total).Select(o => new { o.Id, o.Total }).ToList();
        Assert.Equal(seeded.Where(o => o.Customer == "alan").OrderBy(o => o.Total).Select(o => o.Total), totals.Select(t => t.Total));
        var found = db.Orders.Find(seeded[7].Id)!;
        Assert.Same(found, db.Orders.Find(seeded[7].Id));
        Assert.Equal(new List<string> { "odd" }, found.Tags);
        Assert.Equal(seeded[7].PlacedAt, found.PlacedAt);

        // A captured variable is a parameter: the cached query runs again with new values.
        foreach (var customer in new[] { "ada", "grace" })
        {
            Assert.All(db.Orders.Where(o => o.Customer == customer).ToList(), o => Assert.Equal(customer, o.Customer));
        }

        // Async, and no-tracking queries.
        Assert.Equal(10, (await db.Orders.Where(o => o.Customer == "grace").ToListAsync()).Count);
        Assert.Equal(10, await db.Orders.CountAsync(o => o.Status == OrderStatus.Shipped));
        Assert.NotNull(await db.Orders.AsNoTracking().FirstOrDefaultAsync(o => o.Total > 200));

        // What HexDB can't answer is refused, not evaluated by loading everything.
        Assert.Throws<InvalidOperationException>(() => db.Orders.Where(o => o.Customer.ToUpper() == "ADA").ToList());
        Assert.Throws<InvalidOperationException>(() => db.Orders.Sum(o => o.Total));
        Assert.Contains("HexDB", db.Orders.Where(o => o.Rush).ToQueryString());
    }

    [SkippableFact]
    public async Task ChangesSaveAsOneTransaction()
    {
        Skip.IfNot(Configured, "set HEXDB_URL and HEXDB_API_KEY");
        var seeded = await Seed();
        using (var db = new ShopContext())
        {
            var order = db.Orders.Single(o => o.Id == seeded[1].Id);
            order.Note = null;
            order.Total = 999;
            order.Tags.Add("edited");
            db.Orders.Remove(db.Orders.Single(o => o.Id == seeded[2].Id));
            db.Orders.Add(new Order { Customer = "new", Total = 1 });
            Assert.Equal(3, db.SaveChanges());
        }
        using (var db = new ShopContext())
        {
            var order = db.Orders.Find(seeded[1].Id)!;
            Assert.Equal((999.0, (string?)null), (order.Total, order.Note));
            Assert.Contains("edited", order.Tags);
            Assert.Null(db.Orders.Find(seeded[2].Id));
            Assert.Equal(30, db.Orders.Count());
            Assert.Equal(1, db.Orders.Count(o => o.Customer == "new"));
        }
    }

    [SkippableFact]
    public async Task GuidKeysConcurrencyTokensAndAtomicity()
    {
        Skip.IfNot(Configured, "set HEXDB_URL and HEXDB_API_KEY");
        await using (var setup = new ShopContext())
        {
            await setup.Database.EnsureDeletedAsync();
            await setup.Database.EnsureCreatedAsync();
        }
        using var client = new HexDBClient(new Uri(Environment.GetEnvironmentVariable("HEXDB_URL")!), Environment.GetEnvironmentVariable("HEXDB_API_KEY"));
        await client.RequestAsync(HttpMethod.Post, "/tessellations/ef_customers/indexes", new { fields = new[] { "email" }, unique = true });

        var ada = new Customer { Id = Guid.NewGuid(), Name = "Ada", Email = "ada@example.com" };
        await using (var db = new ShopContext())
        {
            db.Customers.Add(ada);
            await db.SaveChangesAsync();
        }
        // The Guid is stored as a ULID; the custom field name is used.
        var stored = (await client.Tessellation("ef_customers").QueryAsync(new Query()));
        Assert.Equal("ada@example.com", stored.Documents[0]["email"]!.GetValue<string>());
        Assert.Equal(26, stored.Documents[0]["id"]!.GetValue<string>().Length);

        // Two contexts change the same customer: the second save is refused.
        await using var first = new ShopContext();
        await using var second = new ShopContext();
        var a = await first.Customers.SingleAsync(c => c.Id == ada.Id);
        var b = await second.Customers.SingleAsync(c => c.Id == ada.Id);
        a.Name = "Ada L.";
        a.Version++;
        await first.SaveChangesAsync();
        b.Name = "Ada B.";
        b.Version++;
        await Assert.ThrowsAsync<DbUpdateConcurrencyException>(() => second.SaveChangesAsync());

        // One failing change (a duplicate email) fails the whole save. The edit to
        // Ada carries a concurrency check, but the conflict isn't a concurrency one.
        await using (var db = new ShopContext())
        {
            var current = await db.Customers.SingleAsync(c => c.Id == ada.Id);
            current.Name = "Ada (lost)";
            current.Version++;
            db.Customers.Add(new Customer { Id = Guid.NewGuid(), Name = "Grace", Email = "grace@example.com" });
            db.Customers.Add(new Customer { Id = Guid.NewGuid(), Name = "Copy", Email = "ada@example.com" });
            var error = await Assert.ThrowsAsync<DbUpdateException>(() => db.SaveChangesAsync());
            Assert.Contains("409", error.Message);
        }
        await using (var db = new ShopContext())
        {
            Assert.Equal(1, await db.Customers.CountAsync());
            Assert.Equal("Ada L.", (await db.Customers.SingleAsync()).Name);
        }
    }
}
