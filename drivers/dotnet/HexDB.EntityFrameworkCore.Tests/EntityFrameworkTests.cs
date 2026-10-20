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

public abstract class Animal
{
    public string Id { get; set; } = null!;
    public string Name { get; set; } = "";
}

public class Dog : Animal
{
    public int Barks { get; set; }
}

public class Cat : Animal
{
    public bool Indoor { get; set; }
}

public class Kitten : Cat
{
    public int AgeWeeks { get; set; }
}

public class Location
{
    public double Lat { get; set; }
    public double Lng { get; set; }
}

public class Address
{
    public string Street { get; set; } = "";
    public string City { get; set; } = "";
    public Location? Location { get; set; }
}

public class InvoiceLine
{
    public string Sku { get; set; } = "";
    public int Quantity { get; set; }
    public decimal Price { get; set; }
}

public class Invoice
{
    public string Id { get; set; } = null!;
    public string Number { get; set; } = "";
    public Address? Billing { get; set; }
    public List<InvoiceLine> Lines { get; set; } = new();
}

public class Author
{
    public string Id { get; set; } = null!;
    public string Name { get; set; } = "";
    public List<Book> Books { get; set; } = new();
}

public class Book
{
    public string Id { get; set; } = null!;
    public string Title { get; set; } = "";
    public int Year { get; set; }
    public string AuthorId { get; set; } = null!;
    public Author? Author { get; set; }
    public List<Review> Reviews { get; set; } = new();
}

public class Review
{
    public string Id { get; set; } = null!;
    public int Stars { get; set; }
    public string BookId { get; set; } = null!;
    public Book? Book { get; set; }
}

public class ShopContext : DbContext
{
    public DbSet<Author> Authors => Set<Author>();
    public DbSet<Book> Books => Set<Book>();
    public DbSet<Review> Reviews => Set<Review>();
    public DbSet<Invoice> Invoices => Set<Invoice>();
    public DbSet<Order> Orders => Set<Order>();
    public DbSet<Customer> Customers => Set<Customer>();
    public DbSet<Animal> Animals => Set<Animal>();
    public DbSet<Dog> Dogs => Set<Dog>();

    protected override void OnConfiguring(DbContextOptionsBuilder options) =>
        options.UseHexDB(Environment.GetEnvironmentVariable("HEXDB_URL")!, Environment.GetEnvironmentVariable("HEXDB_API_KEY"), o => o.PageSize(7));

    protected override void OnModelCreating(ModelBuilder model)
    {
        model.Entity<Order>().ToTessellation("ef_orders");
        model.Entity<Customer>().ToTessellation("ef_customers").Property(c => c.Email).ToJsonProperty("email");
        model.Entity<Animal>().ToTessellation("ef_animals");
        model.Entity<Author>().ToTessellation("ef_authors");
        model.Entity<Book>().ToTessellation("ef_books");
        model.Entity<Review>().ToTessellation("ef_reviews");
        model.Entity<Invoice>(invoice =>
        {
            invoice.ToTessellation("ef_invoices");
            invoice.OwnsOne(i => i.Billing, billing => billing.OwnsOne(a => a.Location));
            invoice.OwnsMany(i => i.Lines).ToJsonProperty("lines");
        });
        model.Entity<Cat>();
        model.Entity<Kitten>();
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

    [SkippableFact]
    public async Task ExplicitTransactionsCommitAtomically()
    {
        Skip.IfNot(Configured, "set HEXDB_URL and HEXDB_API_KEY");
        await Seed();
        await using var db = new ShopContext();
        await using var other = new ShopContext();

        // Two saves in one transaction: nothing is written until Commit.
        await using (var transaction = await db.Database.BeginTransactionAsync())
        {
            Assert.Same(transaction, db.Database.CurrentTransaction);
            db.Orders.Add(new Order { Customer = "tx", Total = 1 });
            await db.SaveChangesAsync();
            db.Customers.Add(new Customer { Id = Guid.NewGuid(), Name = "Tx" });
            await db.SaveChangesAsync();
            Assert.Equal(0, await other.Orders.CountAsync(o => o.Customer == "tx"));
            Assert.Throws<InvalidOperationException>(() => db.Database.BeginTransaction());
            await transaction.CommitAsync();
        }
        Assert.Null(db.Database.CurrentTransaction);
        Assert.Equal(1, await other.Orders.CountAsync(o => o.Customer == "tx"));
        Assert.Equal(1, await other.Customers.CountAsync(c => c.Name == "Tx"));

        // Rolled back, or disposed without a commit: nothing is written.
        await using (var transaction = await db.Database.BeginTransactionAsync())
        {
            db.Orders.Add(new Order { Customer = "rolled-back" });
            await db.SaveChangesAsync();
            await transaction.RollbackAsync();
        }
        using (db.Database.BeginTransaction())
        {
            db.Orders.Add(new Order { Customer = "rolled-back" });
            db.SaveChanges();
        }
        Assert.Equal(0, await other.Orders.CountAsync(o => o.Customer == "rolled-back"));

        // A conflict surfaces at Commit, and nothing in the transaction is written.
        var customer = await db.Customers.SingleAsync(c => c.Name == "Tx");
        var stale = await other.Customers.SingleAsync(c => c.Name == "Tx");
        customer.Version++;
        await db.SaveChangesAsync();
        await using (var transaction = await other.Database.BeginTransactionAsync())
        {
            stale.Version = 99;
            other.Orders.Add(new Order { Customer = "conflicted" });
            await other.SaveChangesAsync();
            await Assert.ThrowsAsync<DbUpdateConcurrencyException>(() => transaction.CommitAsync());
        }
        await using var fresh = new ShopContext();
        Assert.Equal(0, await fresh.Orders.CountAsync(o => o.Customer == "conflicted"));
    }

    [SkippableFact]
    public async Task InheritanceSharesATessellation()
    {
        Skip.IfNot(Configured, "set HEXDB_URL and HEXDB_API_KEY");
        await Seed();
        await using (var db = new ShopContext())
        {
            db.Animals.AddRange(new Dog { Name = "Rex", Barks = 3 }, new Dog { Name = "Fido", Barks = 1 }, new Cat { Name = "Tom", Indoor = true },
                new Kitten { Name = "Mini", Indoor = true, AgeWeeks = 8 });
            await db.SaveChangesAsync();
        }
        await using (var db = new ShopContext())
        {
            var all = await db.Animals.OrderBy(a => a.Name).ToListAsync();
            Assert.Equal(new[] { "Fido", "Mini", "Rex", "Tom" }, all.Select(a => a.Name));
            Assert.IsType<Dog>(all[0]);
            Assert.IsType<Kitten>(all[1]);
            Assert.Equal(8, ((Kitten)all[1]).AgeWeeks);
            Assert.IsType<Cat>(all[3]);
            Assert.Equal(2, await db.Dogs.CountAsync());
            Assert.Equal("Rex", (await db.Dogs.SingleAsync(d => d.Barks > 2)).Name);
            // A derived set or OfType includes its own subtypes.
            Assert.Equal(new[] { "Mini", "Tom" }, await db.Animals.OfType<Cat>().OrderBy(c => c.Name).Select(c => c.Name).ToListAsync());
            Assert.Equal(1, await db.Set<Kitten>().CountAsync());
            var rex = await db.Dogs.SingleAsync(d => d.Name == "Rex");
            rex.Barks = 10;
            await db.SaveChangesAsync();
        }
        await using (var db = new ShopContext())
        {
            Assert.Equal(10, (await db.Dogs.SingleAsync(d => d.Name == "Rex")).Barks);
        }
    }

    [SkippableFact]
    public async Task OwnedTypesAreNestedInTheDocument()
    {
        Skip.IfNot(Configured, "set HEXDB_URL and HEXDB_API_KEY");
        await Seed();
        string id;
        await using (var db = new ShopContext())
        {
            var invoice = new Invoice
            {
                Number = "INV-1",
                Billing = new Address { Street = "Karl Johans gate 1", City = "Oslo", Location = new Location { Lat = 59.91, Lng = 10.75 } },
                Lines = { new InvoiceLine { Sku = "A", Quantity = 2, Price = 9.5m }, new InvoiceLine { Sku = "B", Quantity = 1, Price = 20m } },
            };
            db.Invoices.AddRange(invoice, new Invoice { Number = "INV-2" });
            await db.SaveChangesAsync();
            id = invoice.Id;
        }

        // Stored as nested JSON, with no keys of its own.
        using (var client = new HexDBClient(new Uri(Environment.GetEnvironmentVariable("HEXDB_URL")!), Environment.GetEnvironmentVariable("HEXDB_API_KEY")))
        {
            var doc = await client.RequestAsync(HttpMethod.Get, "/ef_invoices/" + id);
            Assert.Equal("Oslo", doc!["Billing"]!["City"]!.GetValue<string>());
            Assert.Equal(59.91, doc["Billing"]!["Location"]!["Lat"]!.GetValue<double>());
            Assert.Equal(2, doc["lines"]!.AsArray().Count);
            Assert.Equal("A", doc["lines"]![0]!["Sku"]!.GetValue<string>());
            Assert.Null(doc["lines"]![0]!["InvoiceId"]);
            Assert.Null(doc["lines"]![0]!["Id"]);
        }

        // Read back, tracked and not, and filtered on an owned property.
        await using (var db = new ShopContext())
        {
            var invoice = await db.Invoices.SingleAsync(i => i.Billing!.City == "Oslo");
            Assert.Equal("INV-1", invoice.Number);
            Assert.Equal(10.75, invoice.Billing!.Location!.Lng);
            Assert.Equal(new[] { "A", "B" }, invoice.Lines.Select(l => l.Sku));
            Assert.Equal(9.5m, invoice.Lines[0].Price);
            var empty = await db.Invoices.SingleAsync(i => i.Number == "INV-2");
            Assert.Null(empty.Billing);
            Assert.Empty(empty.Lines);

            var detached = await db.Invoices.AsNoTracking().SingleAsync(i => i.Number == "INV-1");
            Assert.Equal("Oslo", detached.Billing!.City);
            Assert.Equal(2, detached.Lines.Count);
            Assert.Equal(new[] { "Oslo", null }, await db.Invoices.OrderBy(i => i.Number).Select(i => i.Billing == null ? null : i.Billing.City).ToListAsync());

            // Change an owned field, add and remove items, then save.
            invoice.Billing.City = "Bergen";
            invoice.Lines.RemoveAt(0);
            invoice.Lines.Add(new InvoiceLine { Sku = "C", Quantity = 5, Price = 1m });
            empty.Billing = new Address { Street = "Bryggen", City = "Bergen" };
            await db.SaveChangesAsync();
        }
        await using (var db = new ShopContext())
        {
            var invoice = await db.Invoices.SingleAsync(i => i.Number == "INV-1");
            Assert.Equal("Bergen", invoice.Billing!.City);
            Assert.Equal(59.91, invoice.Billing.Location!.Lat);
            Assert.Equal(new[] { "B", "C" }, invoice.Lines.Select(l => l.Sku));
            Assert.Equal(2, await db.Invoices.CountAsync(i => i.Billing!.City == "Bergen"));

            // Removing an owned reference removes it from the document.
            invoice.Billing = null;
            await db.SaveChangesAsync();
        }
        await using (var db = new ShopContext())
        {
            Assert.Null((await db.Invoices.SingleAsync(i => i.Number == "INV-1")).Billing);
        }
    }

    [SkippableFact]
    public async Task RelationshipsLoadWithInclude()
    {
        Skip.IfNot(Configured, "set HEXDB_URL and HEXDB_API_KEY");
        await Seed();
        // An object graph saves in one go; foreign keys come from the navigations.
        await using (var db = new ShopContext())
        {
            var ursula = new Author { Name = "Ursula" };
            for (var i = 0; i < 10; i++)
            {
                var book = new Book { Title = "Book " + i, Year = 1960 + i };
                for (var r = 0; r < i % 3; r++) book.Reviews.Add(new Review { Stars = 3 + r });
                ursula.Books.Add(book);
            }
            db.Authors.AddRange(ursula, new Author { Name = "Nobody" });
            await db.SaveChangesAsync();
            Assert.All(ursula.Books, b => Assert.Equal(ursula.Id, b.AuthorId));
        }

        await using (var db = new ShopContext())
        {
            // Collection, then a collection of each: across more than one page (PageSize 7).
            var authors = await db.Authors.Include(a => a.Books).ThenInclude(b => b.Reviews).OrderBy(a => a.Name).ToListAsync();
            Assert.Equal(new[] { "Nobody", "Ursula" }, authors.Select(a => a.Name));
            Assert.Empty(authors[0].Books);
            var ursula = authors[1];
            Assert.Equal(10, ursula.Books.Count);
            Assert.All(ursula.Books, b => Assert.Same(ursula, b.Author));
            Assert.Equal(9, ursula.Books.Sum(b => b.Reviews.Count));
            Assert.All(ursula.Books.SelectMany(b => b.Reviews), r => Assert.NotNull(r.Book));

            // A reference, filtered on a foreign key.
            var books = await db.Books.Include(b => b.Author).Where(b => b.AuthorId == ursula.Id && b.Year >= 1965).OrderBy(b => b.Year).ToListAsync();
            Assert.Equal(5, books.Count);
            Assert.All(books, b => Assert.Same(ursula, b.Author));
        }

        await using (var db = new ShopContext())
        {
            // Not tracked: connected in both directions all the same.
            var books = await db.Books.AsNoTracking().Include("Author").Include(b => b.Reviews).OrderBy(b => b.Year).ToListAsync();
            Assert.Equal(10, books.Count);
            Assert.All(books, b => Assert.Equal("Ursula", b.Author!.Name));
            Assert.Equal(new[] { 0, 1, 2 }, books.Take(3).Select(b => b.Reviews.Count));
            Assert.All(books.SelectMany(b => b.Reviews), r => Assert.NotNull(r.Book));
            Assert.Empty(db.ChangeTracker.Entries());

            // Navigations can't be filtered on: use the foreign key.
            await Assert.ThrowsAsync<InvalidOperationException>(() => db.Books.Where(b => b.Author!.Name == "Ursula").ToListAsync());
        }

        await using (var db = new ShopContext())
        {
            // Deleting a principal deletes its loaded dependents (EF Core's cascade).
            var nobody = await db.Authors.SingleAsync(a => a.Name == "Nobody");
            var ursula = await db.Authors.Include(a => a.Books).SingleAsync(a => a.Name == "Ursula");
            var first = ursula.Books.OrderBy(b => b.Year).First();
            first.Author = nobody;
            await db.SaveChangesAsync();
            Assert.Equal(nobody.Id, (await new ShopContext().Books.SingleAsync(b => b.Title == first.Title)).AuthorId);
            db.Authors.Remove(ursula);
            await db.SaveChangesAsync();
        }
        await using (var db = new ShopContext())
        {
            Assert.Equal(1, await db.Books.CountAsync());
            Assert.Equal(1, await db.Authors.CountAsync());
        }
    }
}
