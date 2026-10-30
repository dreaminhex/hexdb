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
    public async Task TokenizedFields()
    {
        Skip.If(Url is null || Key is null, "set HEXDB_URL and HEXDB_API_KEY");
        using var db = Client();
        var accounts = db.Tessellation($"{_prefix}_accounts");
        await accounts.InsertAsync(new { card = "0000000000000000" });
        await db.RequestAsync(HttpMethod.Post, $"/tessellations/{accounts.Name}/schemas", new { fields = new { card = new { type = "string", tokenize = "vault" } } });
        var doc = await accounts.InsertAsync(new { card = "4111111111111111" });
        var token = doc["card"]!.GetValue<string>();
        Assert.StartsWith("tok_", token);
        Assert.EndsWith("_1111", token);
        var values = await accounts.DetokenizeAsync(new[] { token });
        Assert.Equal("4111111111111111", values[token]);
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

    [SkippableFact]
    public async Task Files()
    {
        Skip.If(Url is null || Key is null, "set HEXDB_URL and HEXDB_API_KEY");
        using var db = Client();
        var media = db.Files($"{_prefix}-media");
        Assert.Equal("files", (await media.CreateAsync())!["kind"]!.GetValue<string>());
        await media.CreateAsync();

        var text = System.Text.Encoding.UTF8.GetBytes("Glacier lagoon field notes");
        var doc = await media.UploadAsync("notes/hello.txt", text);
        var id = doc["id"]!.GetValue<string>();
        Assert.Equal("/notes/", doc["path"]!.GetValue<string>());
        Assert.Equal(text, await media.DownloadAsync("notes/hello.txt"));
        Assert.Equal(text, await media.DownloadAsync(id));
        Assert.Equal("lagoon", System.Text.Encoding.UTF8.GetString(await media.ReadRangeAsync(id, 8, 14)));
        Assert.Equal(id, (await media.StatAsync("notes/hello.txt"))!["id"]!.GetValue<string>());
        Assert.Null(await media.StatAsync("notes/missing.txt"));
        Assert.Equal(new[] { "hello.txt" }, (await media.ListAsync("notes")).Select(f => f["name"]!.GetValue<string>()));

        var v2 = await media.UploadAsync("notes/hello.txt", System.Text.Encoding.UTF8.GetBytes("second draft"));
        Assert.Equal(2, v2["version"]!.GetValue<int>());
        Assert.Equal(text, await media.DownloadAsync("notes/hello.txt", version: 1));
        Assert.Equal(3, (await media.RestoreVersionAsync(id, 1))["version"]!.GetValue<int>());

        var big = new byte[17 * 1024 * 1024];
        Random.Shared.NextBytes(big);
        var bigDoc = await media.UploadAsync("raw/big.bin", big);
        Assert.Equal(big.Length, bigDoc["size"]!.GetValue<int>());
        using var copy = new MemoryStream();
        await media.DownloadToAsync(bigDoc["id"]!.GetValue<string>(), copy);
        Assert.Equal(big, copy.ToArray());

        for (var i = 0; i < 50 && (await media.Documents.GetAsync(id))!["extract"]!["status"]!.GetValue<string>() != "done"; i++) await Task.Delay(200);
        Assert.Equal(new[] { "hello.txt" }, (await media.SearchAsync("glacier")).Select(f => f["name"]!.GetValue<string>()));

        Assert.True(await media.DeleteAsync(id));
        Assert.Equal(new[] { id }, (await media.TrashAsync()).Select(f => f["id"]!.GetValue<string>()));
        Assert.Equal("hello.txt", (await media.RestoreAsync(id))["name"]!.GetValue<string>());
        await media.DeleteAsync(id);
        await media.PurgeAsync(id);
        Assert.Empty(await media.TrashAsync());
    }
}
