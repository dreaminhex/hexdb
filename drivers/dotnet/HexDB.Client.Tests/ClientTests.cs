// Runs the .NET driver against a real server:
//   node drivers/testing/server.mjs dotnet test drivers/dotnet
// HEXDB_URL and HEXDB_API_KEY name the server (set by server.mjs); without
// them the tests are skipped.

using System.Text.Json.Nodes;
using HexDB.Client;
using Xunit;

public class ClientTests
{
    private static readonly string? Url = Environment.GetEnvironmentVariable("HEXDB_URL");
    private static readonly string? Key = Environment.GetEnvironmentVariable("HEXDB_API_KEY");
    private readonly string _prefix = $"net{Environment.TickCount64 % 10_000_000}{Random.Shared.Next(1000)}";

    private static HexDBClient Client() => new(new Uri(Url!), Key);

    [SkippableFact]
    public async Task Documents()
    {
        Skip.If(Url is null || Key is null, "set HEXDB_URL and HEXDB_API_KEY");
        using var db = Client();
        var notes = db.Tessellation($"{_prefix}_notes");
        var doc = await notes.InsertAsync(new { title = "hello" });
        var id = doc["id"]!.GetValue<string>();
        Assert.Equal(26, id.Length);
        Assert.Equal("hello", (await notes.GetAsync(id))!["title"]!.GetValue<string>());
        Assert.Equal("hi", (await notes.PatchAsync(id, new { title = "hi" }))["title"]!.GetValue<string>());
        Assert.True(await notes.DeleteAsync(id));
        Assert.Null(await notes.GetAsync(id));
        Assert.False(await notes.DeleteAsync(id));
    }

    [SkippableFact]
    public async Task QueriesAggregationsAndUpserts()
    {
        Skip.If(Url is null || Key is null, "set HEXDB_URL and HEXDB_API_KEY");
        using var db = Client();
        var orders = db.Tessellation($"{_prefix}_orders");
        var ids = await orders.InsertManyAsync(Enumerable.Range(0, 20).Select(i => (object)new { n = i, total = i * 10, status = i % 2 == 0 ? "new" : "paid" }));
        Assert.Equal(20, ids.Count);
        var page = await orders.QueryAsync(new Query { Filter = new { status = "paid" }, Sort = "-total", Limit = 2 });
        Assert.Equal(10, page.Total);
        Assert.Equal(190, page.Documents[0]["total"]!.GetValue<int>());
        Assert.Equal(5, await orders.CountAsync(new Dictionary<string, object> { ["total"] = new Dictionary<string, object> { ["$lt"] = 50 } }));
        var count = 0;
        await foreach (var _ in orders.IterateAsync(pageSize: 6)) count++;
        Assert.Equal(20, count);
        var agg = await orders.AggregateAsync(new { group_by = new[] { "status" }, aggregates = new { sum = new Dictionary<string, string> { ["$sum"] = "total" } } });
        Assert.Equal(2, agg!["rows"]!.AsArray().Count);
        var upsert = await orders.UpsertAsync(new[] { "n" }, new object[] { new { n = 0, status = "void" }, new { n = 100 } });
        Assert.Equal((1, 1), (upsert.Inserted, upsert.Replaced));
        Assert.Equal(ids[0], upsert.Ids[0]);
    }

    [SkippableFact]
    public async Task TransactionsGraphQLAndErrors()
    {
        Skip.If(Url is null || Key is null, "set HEXDB_URL and HEXDB_API_KEY");
        using var db = Client();
        var ledger = $"{_prefix}_ledger";
        var op = new object[] { new { op = "insert", tessellation = ledger, data = new { amount = 5 } } };
        await db.TransactionAsync(op, idempotencyKey: $"{_prefix}-tx");
        await db.TransactionAsync(op, idempotencyKey: $"{_prefix}-tx");
        Assert.Equal(1, await db.Tessellation(ledger).CountAsync());
        var data = await db.GraphQLAsync("query($t: String!) { count(tessellation: $t) }", new { t = ledger });
        Assert.Equal(1, data!["count"]!.GetValue<int>());
        var error = await Assert.ThrowsAsync<HexDBException>(() => new HexDBClient(new Uri(Url!)).Tessellation(ledger).CountAsync());
        Assert.Equal((401, "unauthorized"), (error.Status, error.Code));
    }

    [SkippableFact]
    public async Task Streams()
    {
        Skip.If(Url is null || Key is null, "set HEXDB_URL and HEXDB_API_KEY");
        using var db = Client();
        var name = $"{_prefix}-events";
        await db.RequestAsync(HttpMethod.Post, "/streams", new { name });
        var events = db.Stream(name);
        var offsets = await events.PublishAsync(new object[] { new { payload = new { n = 1 } }, new { payload = new { n = 2 }, key = "k" } });
        Assert.Equal(2, offsets.Count);
        var (messages, _) = await events.ReadAsync();
        Assert.Equal(new[] { 1, 2 }, messages.Select(m => m.Payload!["n"]!.GetValue<int>()));
        var got = new List<int>();
        using var stop = new CancellationTokenSource();
        await events.ConsumeAsync("workers", m =>
        {
            got.Add(m.Payload!["n"]!.GetValue<int>());
            if (got.Count == 2) stop.Cancel();
            return Task.CompletedTask;
        }, cancel: stop.Token);
        Assert.Equal(new[] { 1, 2 }, got);
        Assert.Empty((await events.ReadAsync(group: "workers")).Messages);
    }

    [SkippableFact]
    public async Task ChangeFeed()
    {
        Skip.If(Url is null || Key is null, "set HEXDB_URL and HEXDB_API_KEY");
        using var db = Client();
        var feed = $"{_prefix}_feed";
        using var stop = new CancellationTokenSource(TimeSpan.FromSeconds(40));
        var first = Task.Run(async () =>
        {
            await foreach (var change in db.ChangesAsync(tessellation: feed, cancel: stop.Token)) return change;
            return null;
        });
        await Task.Delay(500);
        var doc = await db.Tessellation(feed).InsertAsync(new { x = 1 });
        var change = await first;
        Assert.Equal(doc["id"]!.GetValue<string>(), change!["id"]!.GetValue<string>());
    }
}
