// Connection URIs: the shared test vectors (drivers/testing/connection-uris.json,
// copied next to the tests) and how the client uses a parsed URI. No server needed:
// requests go to a stub handler.

using System.Net;
using System.Text;
using System.Text.Json.Nodes;
using HexDB.Client;
using Xunit;

public class ConnectionUriTests
{
    private static readonly JsonObject Vectors =
        JsonNode.Parse(File.ReadAllText(Path.Combine(AppContext.BaseDirectory, "connection-uris.json")))!.AsObject();

    public static TheoryData<string, string> ValidUris()
    {
        var data = new TheoryData<string, string>();
        foreach (var vector in Vectors["valid"]!.AsArray()) data.Add(vector!["uri"]!.GetValue<string>(), vector.ToJsonString());
        return data;
    }

    public static TheoryData<string, string> InvalidUris()
    {
        var data = new TheoryData<string, string>();
        foreach (var vector in Vectors["invalid"]!.AsArray()) data.Add(vector!["uri"]!.GetValue<string>(), vector["reason"]!.GetValue<string>());
        return data;
    }

    [Theory]
    [MemberData(nameof(ValidUris))]
    public void ParsesValidUris(string uri, string expectedJson)
    {
        var expected = JsonNode.Parse(expectedJson)!;
        var parsed = HexDBConnectionUri.Parse(uri);
        var url = expected["url"]!.GetValue<string>();
        Assert.Equal(new Uri(url), parsed.Url);
        Assert.Equal(new Uri(url).AbsoluteUri.TrimEnd('/'), parsed.Url.AbsoluteUri.TrimEnd('/'));
        Assert.Equal(expected["login"]?.GetValue<string>(), parsed.Login);
        Assert.Equal(expected["password"]?.GetValue<string>(), parsed.Password);
        Assert.Equal(expected["apiKey"]?.GetValue<string>(), parsed.ApiKey);
        Assert.Equal(expected["timeoutSeconds"]?.GetValue<int>() is { } seconds ? (TimeSpan?)TimeSpan.FromSeconds(seconds) : null, parsed.Timeout);
        Assert.Equal(expected["pageSize"]?.GetValue<int>(), parsed.PageSize);
        Assert.Equal(expected["caFile"]?.GetValue<string>(), parsed.CaFile);
        Assert.True(HexDBConnectionUri.TryParse(uri, out var again));
        Assert.Equal(parsed.Url, again.Url);
    }

    [Theory]
    [MemberData(nameof(InvalidUris))]
    public void RefusesInvalidUris(string uri, string reason)
    {
        var error = Assert.ThrowsAny<FormatException>(() => HexDBConnectionUri.Parse(uri));
        // The message names the problem: it mentions the part of the URI the reason is about.
        var subject = new[] { "apiKey", "timeout", "pageSize", "port", "fragment", "host", "//", "hexdb" }
            .First(word => reason.Contains(word, StringComparison.OrdinalIgnoreCase));
        Assert.Contains(subject, error.Message, StringComparison.OrdinalIgnoreCase);
        Assert.False(HexDBConnectionUri.TryParse(uri, out _));
        Assert.ThrowsAny<FormatException>(() => new HexDBClient(uri));
    }

    [Fact]
    public void ToStringHidesSecrets()
    {
        var text = HexDBConnectionUri.Parse("hexdbs://ada:p%40ss@DB.example.com:8443/hexdb/?timeout=5").ToString();
        Assert.Equal("hexdbs://ada:***@db.example.com:8443/hexdb?timeout=5", text);
        Assert.Equal("hexdb://localhost?apiKey=***", HexDBConnectionUri.Parse("hexdb://localhost:7700?apiKey=hxk_secret").ToString());
    }

    [Fact]
    public void AcceptsPlainUrls()
    {
        var parsed = HexDBConnectionUri.Parse("https://db.example.com:8443/base");
        Assert.Equal(new Uri("https://db.example.com:8443/base"), parsed.Url);
        Assert.Null(parsed.Login);
        Assert.Null(parsed.ApiKey);
    }

    [Fact]
    public async Task ConnectAsyncSignsIn()
    {
        var stub = new StubHandler();
        using var db = await HexDBClient.ConnectAsync("hexdbs://ada:p%40ss@db.example.com/hexdb", new HttpClient(stub));
        var login = Assert.Single(stub.Requests);
        Assert.Equal((HttpMethod.Post, "https://db.example.com/hexdb/auth/login"), (login.Method, login.Url));
        var body = JsonNode.Parse(login.Body!)!;
        Assert.Equal(("ada", "p@ss"), (body["login"]!.GetValue<string>(), body["password"]!.GetValue<string>()));

        await db.HealthAsync();
        Assert.Equal(("https://db.example.com/hexdb/health", "Bearer tok_1"), (stub.Requests[1].Url, stub.Requests[1].Authorization));
    }

    [Fact]
    public async Task ConnectAsyncReportsAFailedSignIn()
    {
        var stub = new StubHandler { LoginStatus = HttpStatusCode.Unauthorized };
        var error = await Assert.ThrowsAsync<HexDBException>(() => HexDBClient.ConnectAsync("hexdb://ada:wrong@localhost", new HttpClient(stub)));
        Assert.Equal(401, error.Status);
    }

    [Fact]
    public async Task ConstructorSignsInOnTheFirstRequest()
    {
        var stub = new StubHandler();
        using var db = new HexDBClient("hexdb://ada:pw@localhost", http: new HttpClient(stub));
        Assert.Empty(stub.Requests);
        await db.HealthAsync();
        await db.StatusAsync();
        Assert.Equal(
            new[] { "http://localhost:7700/auth/login", "http://localhost:7700/health", "http://localhost:7700/status" },
            stub.Requests.Select(r => r.Url));
        Assert.Equal("Bearer tok_1", stub.Requests[2].Authorization);
    }

    [Fact]
    public async Task ConstructorUsesTheApiKey()
    {
        var stub = new StubHandler();
        using var db = new HexDBClient("hexdb://127.0.0.1:7710?apiKey=hxk_abc", http: new HttpClient(stub));
        await db.HealthAsync();
        var request = Assert.Single(stub.Requests);
        Assert.Equal(("http://127.0.0.1:7710/health", "Bearer hxk_abc"), (request.Url, request.Authorization));
    }

    [Fact]
    public async Task AnApiKeyArgumentWinsOverTheUri()
    {
        var stub = new StubHandler();
        using var db = new HexDBClient("hexdb://ada:pw@localhost", "hxk_given", new HttpClient(stub));
        await db.HealthAsync();
        var request = Assert.Single(stub.Requests);
        Assert.Equal(("http://localhost:7700/health", "Bearer hxk_given"), (request.Url, request.Authorization));
    }

    [Fact]
    public async Task PlainUrlsWorkAsBefore()
    {
        var stub = new StubHandler();
        using var db = new HexDBClient("http://127.0.0.1:7700", http: new HttpClient(stub));
        await db.HealthAsync();
        var request = Assert.Single(stub.Requests);
        Assert.Equal(("http://127.0.0.1:7700/health", (string?)null), (request.Url, request.Authorization));
    }

    private sealed record Recorded(HttpMethod Method, string Url, string? Authorization, string? Body);

    // Answers /auth/login with a token and everything else with {}, recording each request.
    private sealed class StubHandler : HttpMessageHandler
    {
        public List<Recorded> Requests { get; } = new();

        public HttpStatusCode LoginStatus { get; init; } = HttpStatusCode.OK;

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
        {
            var body = request.Content is null ? null : await request.Content.ReadAsStringAsync(cancellationToken);
            Requests.Add(new Recorded(request.Method, request.RequestUri!.AbsoluteUri, request.Headers.Authorization?.ToString(), body));
            var isLogin = request.RequestUri.AbsolutePath.EndsWith("/auth/login", StringComparison.Ordinal);
            var (status, json) = isLogin
                ? LoginStatus == HttpStatusCode.OK
                    ? (HttpStatusCode.OK, """{"token":"tok_1"}""")
                    : (LoginStatus, """{"error":{"code":"unauthorized","message":"Wrong login or password."}}""")
                : (HttpStatusCode.OK, "{}");
            return new HttpResponseMessage(status) { Content = new StringContent(json, Encoding.UTF8, "application/json") };
        }
    }
}
