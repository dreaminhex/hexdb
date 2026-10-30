// HexDB client for .NET 8+. Speaks HexDB's REST API with HttpClient and
// System.Text.Json.
//
//   var db = new HexDBClient(new Uri("http://127.0.0.1:7700"), apiKey: Environment.GetEnvironmentVariable("HEXDB_API_KEY"));
//   var orders = db.Tessellation("orders");
//   var doc = await orders.InsertAsync(new { customer = "ada", total = 12 });
//   var page = await orders.QueryAsync(new Query { Filter = new { total = new Dictionary<string, object> { ["$gt"] = 10 } }, Sort = "-total" });
//
//   var media = db.Files("media");                       // a files tessellation
//   var photo = await media.UploadFileAsync("photos/IMG_1.jpg", @"C:\Pictures\IMG_1.jpg");
//   var hits = await media.SearchAsync("glacier");        // names, tags and extracted metadata

using System.Net;
using System.Net.Http.Headers;
using System.Net.Http.Json;
using System.Runtime.CompilerServices;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.Json.Serialization;
using System.Text.RegularExpressions;

namespace HexDB.Client;

/// <summary>An error answer from HexDB: <see cref="Status"/> is the HTTP status, <see cref="Code"/> HexDB's error code.</summary>
public sealed class HexDBException : Exception
{
    public HexDBException(int status, string code, string message, TimeSpan? retryAfter = null) : base(message)
    {
        Status = status;
        Code = code;
        RetryAfter = retryAfter;
    }

    public int Status { get; }
    public string Code { get; }
    public TimeSpan? RetryAfter { get; }
}

/// <summary>A query: filter, sort, paging and projection.</summary>
public sealed class Query
{
    /// <summary>A filter object, e.g. <c>new { status = "paid" }</c> (see the HexDB manual for operators).</summary>
    [JsonPropertyName("filter")] public object? Filter { get; set; }
    /// <summary>"-total,name".</summary>
    [JsonPropertyName("sort")] public string? Sort { get; set; }
    [JsonPropertyName("limit")] public int Limit { get; set; } = 100;
    [JsonPropertyName("offset")] public int Offset { get; set; }
    [JsonPropertyName("after")] public string? After { get; set; }
    [JsonPropertyName("fields")] public IReadOnlyList<string>? Fields { get; set; }
}

/// <summary>One page of query results.</summary>
public sealed class QueryPage
{
    [JsonPropertyName("documents")] public List<JsonObject> Documents { get; set; } = new();
    [JsonPropertyName("total")] public int Total { get; set; }
    [JsonPropertyName("next")] public string? Next { get; set; }
}

/// <summary>What an upsert did; <see cref="Ids"/> follow the request's order.</summary>
public sealed class UpsertResult
{
    [JsonPropertyName("inserted")] public int Inserted { get; set; }
    [JsonPropertyName("replaced")] public int Replaced { get; set; }
    [JsonPropertyName("ids")] public List<string> Ids { get; set; } = new();
}

/// <summary>A stream message.</summary>
public sealed class StreamMessage
{
    [JsonPropertyName("offset")] public string Offset { get; set; } = "";
    [JsonPropertyName("time")] public long Time { get; set; }
    [JsonPropertyName("payload")] public JsonNode? Payload { get; set; }
    [JsonPropertyName("key")] public string? Key { get; set; }
    [JsonPropertyName("headers")] public Dictionary<string, string>? Headers { get; set; }
}

/// <summary>A connection to a HexDB server (any hex; replicas forward writes to the Overseer).</summary>
public sealed class HexDBClient : IDisposable
{
    private readonly HttpClient _http;
    private readonly bool _ownsHttp;
    private string? _token;

    internal static readonly JsonSerializerOptions Json = new() { DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull };

    /// <param name="url">Base URL of any hex.</param>
    /// <param name="apiKey">An API key (hxk_...) or session token.</param>
    /// <param name="http">An HttpClient to use (for custom certificates or handlers).</param>
    public HexDBClient(Uri url, string? apiKey = null, HttpClient? http = null)
    {
        _http = http ?? new HttpClient();
        _ownsHttp = http is null;
        _http.BaseAddress = url;
        _token = apiKey;
    }

    /// <summary>Retries for 429 and 503 answers on reads and idempotent writes.</summary>
    public int Retries { get; set; } = 3;

    public void Dispose()
    {
        if (_ownsHttp) _http.Dispose();
    }

    /// <summary>Send any request; returns the JSON answer (null for 204).</summary>
    public async Task<JsonNode?> RequestAsync(HttpMethod method, string path, object? body = null, string? idempotencyKey = null, CancellationToken cancel = default)
    {
        for (var attempt = 0; ; attempt++)
        {
            using var request = new HttpRequestMessage(method, path.TrimStart('/'));
            request.Headers.Accept.Add(new MediaTypeWithQualityHeaderValue("application/json"));
            if (_token is not null) request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", _token);
            if (idempotencyKey is not null) request.Headers.Add("Idempotency-Key", idempotencyKey);
            if (body is not null) request.Content = new StringContent(JsonSerializer.Serialize(body, Json), Encoding.UTF8, "application/json");
            HttpResponseMessage response;
            try
            {
                response = await _http.SendAsync(request, cancel).ConfigureAwait(false);
            }
            catch (HttpRequestException e)
            {
                throw new HexDBException(0, "unreachable", $"Couldn't reach HexDB at {_http.BaseAddress}: {e.Message}");
            }
            using (response)
            {
                if (response.StatusCode == HttpStatusCode.NoContent) return null;
                var text = await response.Content.ReadAsStringAsync(cancel).ConfigureAwait(false);
                if (response.IsSuccessStatusCode) return string.IsNullOrEmpty(text) ? null : JsonNode.Parse(text);
                var retryAfter = response.Headers.RetryAfter?.Delta;
                var retryable = (int)response.StatusCode is 429 or 503 && (method == HttpMethod.Get || idempotencyKey is not null);
                if (retryable && attempt < Retries)
                {
                    await Task.Delay(retryAfter ?? TimeSpan.FromSeconds(Math.Pow(2, attempt)), cancel).ConfigureAwait(false);
                    continue;
                }
                string code = "error", message = text;
                try
                {
                    var error = JsonNode.Parse(text)?["error"];
                    code = error?["code"]?.GetValue<string>() ?? code;
                    message = error?["message"]?.GetValue<string>() ?? message;
                }
                catch (JsonException) { }
                throw new HexDBException((int)response.StatusCode, code, message, retryAfter);
            }
        }
    }

    internal async Task<T> RequestAsync<T>(HttpMethod method, string path, object? body = null, string? idempotencyKey = null, CancellationToken cancel = default)
    {
        var node = await RequestAsync(method, path, body, idempotencyKey, cancel).ConfigureAwait(false);
        return node is null ? default! : node.Deserialize<T>(Json)!;
    }

    /// <summary>Sign in (with a one-time code if MFA is on); later requests use the session.</summary>
    public async Task LoginAsync(string login, string password, string? code = null, CancellationToken cancel = default)
    {
        var result = await RequestAsync(HttpMethod.Post, "/auth/login", new { login, password, code, return_token = true }, null, cancel).ConfigureAwait(false);
        _token = result!["token"]!.GetValue<string>();
    }

    public async Task LogoutAsync(CancellationToken cancel = default)
    {
        await RequestAsync(HttpMethod.Post, "/auth/logout", null, null, cancel).ConfigureAwait(false);
        _token = null;
    }

    public Task<JsonNode?> HealthAsync(CancellationToken cancel = default) => RequestAsync(HttpMethod.Get, "/health", cancel: cancel);

    public Task<JsonNode?> StatusAsync(CancellationToken cancel = default) => RequestAsync(HttpMethod.Get, "/status", cancel: cancel);

    public Tessellation Tessellation(string name) => new(this, name);

    /// <summary>A files tessellation (photos, audio, video, documents: any binary file).</summary>
    public Files Files(string name) => new(this, name);

    /// <summary>Send raw content and get the raw response (dispose it); throws <see cref="HexDBException"/> on an error answer.</summary>
    public async Task<HttpResponseMessage> SendRawAsync(HttpMethod method, string path, HttpContent? content = null, IDictionary<string, string>? headers = null, CancellationToken cancel = default)
    {
        using var request = new HttpRequestMessage(method, path.TrimStart('/')) { Content = content };
        if (_token is not null) request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", _token);
        foreach (var (name, value) in headers ?? new Dictionary<string, string>()) request.Headers.TryAddWithoutValidation(name, value);
        HttpResponseMessage response;
        try
        {
            response = await _http.SendAsync(request, HttpCompletionOption.ResponseHeadersRead, cancel).ConfigureAwait(false);
        }
        catch (HttpRequestException e)
        {
            throw new HexDBException(0, "unreachable", $"Couldn't reach HexDB at {_http.BaseAddress}: {e.Message}");
        }
        if (response.IsSuccessStatusCode) return response;
        using (response)
        {
            var text = await response.Content.ReadAsStringAsync(cancel).ConfigureAwait(false);
            string code = "error", message = text;
            try
            {
                var error = JsonNode.Parse(text)?["error"];
                code = error?["code"]?.GetValue<string>() ?? code;
                message = error?["message"]?.GetValue<string>() ?? message;
            }
            catch (JsonException) { }
            throw new HexDBException((int)response.StatusCode, code, message);
        }
    }

    public Stream Stream(string name) => new(this, name);

    /// <summary>Operations across tessellations, all or nothing.</summary>
    public Task<JsonNode?> TransactionAsync(IEnumerable<object> operations, string? idempotencyKey = null, CancellationToken cancel = default) =>
        RequestAsync(HttpMethod.Post, "/transactions", new { operations }, idempotencyKey, cancel);

    /// <summary>Run a GraphQL query or mutation; throws on GraphQL errors.</summary>
    public async Task<JsonNode?> GraphQLAsync(string query, object? variables = null, CancellationToken cancel = default)
    {
        var result = await RequestAsync(HttpMethod.Post, "/graphql", new { query, variables }, null, cancel).ConfigureAwait(false);
        var errors = result?["errors"]?.AsArray();
        if (errors is { Count: > 0 })
        {
            throw new HexDBException(200, errors[0]?["extensions"]?["code"]?.GetValue<string>() ?? "GRAPHQL", errors[0]?["message"]?.GetValue<string>() ?? "GraphQL error");
        }
        return result?["data"];
    }

    /// <summary>Run a saved function; returns its result.</summary>
    public async Task<JsonNode?> RunFunctionAsync(string name, object? parameters = null, CancellationToken cancel = default) =>
        (await RequestAsync(HttpMethod.Post, $"/functions/{Uri.EscapeDataString(name)}/run", new { @params = parameters ?? new { } }, null, cancel).ConfigureAwait(false))?["result"];

    /// <summary>Committed changes after <paramref name="after"/> (default: from now), as they happen (long polling).</summary>
    public async IAsyncEnumerable<JsonObject> ChangesAsync(long? after = null, string? tessellation = null, [EnumeratorCancellation] CancellationToken cancel = default)
    {
        var cursor = after;
        while (!cancel.IsCancellationRequested)
        {
            var query = $"/changes?wait=30&limit=500{(cursor is null ? "" : $"&after={cursor}")}{(tessellation is null ? "" : $"&tessellation={Uri.EscapeDataString(tessellation)}")}";
            var page = await RequestAsync(HttpMethod.Get, query, cancel: cancel).ConfigureAwait(false);
            cursor = page!["last_seq"]!.GetValue<long>();
            foreach (var change in page["changes"]!.AsArray()) yield return change!.AsObject();
        }
    }
}

/// <summary>A collection of documents.</summary>
public sealed class Tessellation
{
    private readonly HexDBClient _db;
    private readonly string _path;

    internal Tessellation(HexDBClient db, string name)
    {
        _db = db;
        Name = name;
        _path = "/" + Uri.EscapeDataString(name);
    }

    public string Name { get; }

    private static string Ttl(int? seconds) => seconds is > 0 ? $"?ttl={seconds}" : "";

    public async Task<JsonObject> InsertAsync(object document, int? ttlSeconds = null, string? idempotencyKey = null, CancellationToken cancel = default) =>
        (await _db.RequestAsync(HttpMethod.Post, _path + Ttl(ttlSeconds), document, idempotencyKey, cancel).ConfigureAwait(false))!.AsObject();

    /// <summary>Insert up to 1000 documents atomically; returns their IDs in order.</summary>
    public async Task<List<string>> InsertManyAsync(IEnumerable<object> documents, int? ttlSeconds = null, string? idempotencyKey = null, CancellationToken cancel = default) =>
        (await _db.RequestAsync(HttpMethod.Post, $"{_path}/_bulk{Ttl(ttlSeconds)}", documents, idempotencyKey, cancel).ConfigureAwait(false))!["ids"]!.Deserialize<List<string>>()!;

    /// <summary>A document by ID, or null.</summary>
    public async Task<JsonObject?> GetAsync(string id, CancellationToken cancel = default)
    {
        try
        {
            return (await _db.RequestAsync(HttpMethod.Get, $"{_path}/{Uri.EscapeDataString(id)}", cancel: cancel).ConfigureAwait(false))?.AsObject();
        }
        catch (HexDBException e) when (e.Status == 404)
        {
            return null;
        }
    }

    public async Task<JsonObject> ReplaceAsync(string id, object document, int? ttlSeconds = null, string? idempotencyKey = null, CancellationToken cancel = default) =>
        (await _db.RequestAsync(HttpMethod.Put, $"{_path}/{Uri.EscapeDataString(id)}{Ttl(ttlSeconds)}", document, idempotencyKey, cancel).ConfigureAwait(false))!.AsObject();

    /// <summary>Merge fields into a document (null removes a field).</summary>
    public async Task<JsonObject> PatchAsync(string id, object changes, int? ttlSeconds = null, string? idempotencyKey = null, CancellationToken cancel = default) =>
        (await _db.RequestAsync(HttpMethod.Patch, $"{_path}/{Uri.EscapeDataString(id)}{Ttl(ttlSeconds)}", changes, idempotencyKey, cancel).ConfigureAwait(false))!.AsObject();

    /// <summary>Delete a document; false if it didn't exist.</summary>
    public async Task<bool> DeleteAsync(string id, string? idempotencyKey = null, CancellationToken cancel = default)
    {
        try
        {
            await _db.RequestAsync(HttpMethod.Delete, $"{_path}/{Uri.EscapeDataString(id)}", null, idempotencyKey, cancel).ConfigureAwait(false);
            return true;
        }
        catch (HexDBException e) when (e.Status == 404)
        {
            return false;
        }
    }

    public Task<QueryPage> QueryAsync(Query query, CancellationToken cancel = default) =>
        _db.RequestAsync<QueryPage>(HttpMethod.Post, $"{_path}/_query", query, null, cancel);

    /// <summary>Every matching document, page by page, in ID order.</summary>
    public async IAsyncEnumerable<JsonObject> IterateAsync(object? filter = null, int pageSize = 500, [EnumeratorCancellation] CancellationToken cancel = default)
    {
        string? after = null;
        do
        {
            var page = await QueryAsync(new Query { Filter = filter, Limit = pageSize, After = after }, cancel).ConfigureAwait(false);
            foreach (var doc in page.Documents) yield return doc;
            after = page.Next;
        } while (after is not null);
    }

    public async Task<int> CountAsync(object? filter = null, CancellationToken cancel = default)
    {
        var query = filter is null ? "" : "?filter=" + Uri.EscapeDataString(JsonSerializer.Serialize(filter, HexDBClient.Json));
        return (await _db.RequestAsync(HttpMethod.Get, $"{_path}/count{query}", cancel: cancel).ConfigureAwait(false))!["count"]!.GetValue<int>();
    }

    /// <summary>Group and summarize, e.g. <c>new { group_by = new[] { "status" }, aggregates = ... }</c>.</summary>
    public Task<JsonNode?> AggregateAsync(object aggregation, CancellationToken cancel = default) =>
        _db.RequestAsync(HttpMethod.Post, $"{_path}/_aggregate", aggregation, null, cancel);

    /// <summary>Insert or replace documents matched by key fields.</summary>
    public Task<UpsertResult> UpsertAsync(IReadOnlyList<string> key, IEnumerable<object> documents, string? idempotencyKey = null, CancellationToken cancel = default) =>
        _db.RequestAsync<UpsertResult>(HttpMethod.Post, $"{_path}/_upsert", new { key, documents }, idempotencyKey, cancel);

    public Task<JsonNode?> UpdateWhereAsync(object filter, object update, CancellationToken cancel = default) =>
        _db.RequestAsync(HttpMethod.Post, $"{_path}/_update", new { filter, update }, null, cancel);

    /// <summary>
    /// The values behind vault tokens of tokenized fields (up to 1,000; needs the
    /// <c>detokenize</c> permission; audited). One-way and unknown tokens map to null.
    /// </summary>
    public async Task<Dictionary<string, string?>> DetokenizeAsync(IEnumerable<string> tokens, CancellationToken cancel = default) =>
        (await _db.RequestAsync(HttpMethod.Post, $"/tessellations/{Uri.EscapeDataString(Name)}/detokenize", new { tokens }, null, cancel).ConfigureAwait(false))!["values"]!
            .Deserialize<Dictionary<string, string?>>(HexDBClient.Json)!;
}

/// <summary>A publish/subscribe stream.</summary>
public sealed class Stream
{
    private readonly HexDBClient _db;
    private readonly string _path;

    internal Stream(HexDBClient db, string name)
    {
        _db = db;
        Name = name;
        _path = "/streams/" + Uri.EscapeDataString(name);
    }

    public string Name { get; }

    /// <summary>Publish messages (<c>new { payload, key, headers }</c>); returns their offsets.</summary>
    public async Task<List<string>> PublishAsync(IEnumerable<object> messages, CancellationToken cancel = default) =>
        (await _db.RequestAsync(HttpMethod.Post, $"{_path}/messages", messages, null, cancel).ConfigureAwait(false))!["offsets"]!.Deserialize<List<string>>()!;

    public async Task<(List<StreamMessage> Messages, string? Next)> ReadAsync(string? after = null, string? group = null, int limit = 100, int wait = 0, CancellationToken cancel = default)
    {
        var query = $"{_path}/messages?limit={limit}{(after is null ? "" : $"&after={after}")}{(group is null ? "" : $"&group={Uri.EscapeDataString(group)}")}{(wait > 0 ? $"&wait={wait}" : "")}";
        var page = await _db.RequestAsync(HttpMethod.Get, query, cancel: cancel).ConfigureAwait(false);
        return (page!["messages"].Deserialize<List<StreamMessage>>(HexDBClient.Json)!, page["next"]?.GetValue<string>());
    }

    public Task CommitAsync(string group, string offset, CancellationToken cancel = default) =>
        _db.RequestAsync(HttpMethod.Post, $"{_path}/groups/{Uri.EscapeDataString(group)}/commit", new { offset }, null, cancel);

    /// <summary>Handle messages in order as a consumer group, committing after each handled batch.</summary>
    public async Task ConsumeAsync(string group, Func<StreamMessage, Task> handle, int batch = 100, CancellationToken cancel = default)
    {
        while (!cancel.IsCancellationRequested)
        {
            var (messages, _) = await ReadAsync(group: group, limit: batch, wait: 30, cancel: cancel).ConfigureAwait(false);
            string? handled = null;
            foreach (var message in messages)
            {
                if (cancel.IsCancellationRequested) break;
                await handle(message).ConfigureAwait(false);
                handled = message.Offset;
            }
            if (handled is not null) await CommitAsync(group, handled, CancellationToken.None).ConfigureAwait(false);
        }
    }
}

/// <summary>
/// A files tessellation: each file's metadata is a document (query it through
/// <see cref="Documents"/>); its content is uploaded and downloaded here.
/// Paths are <c>folder/name</c>. Uploading to a path that has a file makes a new version.
/// </summary>
public sealed partial class Files
{
    private const long ResumableFrom = 16L * 1024 * 1024;
    private const int Part = 8 * 1024 * 1024;
    private readonly HexDBClient _db;
    private readonly string _path;

    internal Files(HexDBClient db, string name)
    {
        _db = db;
        Name = name;
        _path = "/" + Uri.EscapeDataString(name);
        Documents = db.Tessellation(name);
    }

    public string Name { get; }

    /// <summary>The files' documents, for queries, counts and aggregations.</summary>
    public Tessellation Documents { get; }

    [GeneratedRegex("^[0-9A-HJKMNP-TV-Z]{26}$")]
    private static partial Regex Ulid();

    private static string FilePath(string path) =>
        string.Join("/", path.Split('/', '\\').Where(p => p.Length > 0).Select(Uri.EscapeDataString));

    /// <summary>Create the files tessellation (no error if it exists).</summary>
    public async Task<JsonNode?> CreateAsync(CancellationToken cancel = default)
    {
        try
        {
            return await _db.RequestAsync(HttpMethod.Post, "/tessellations", new { name = Name, kind = "files" }, cancel: cancel).ConfigureAwait(false);
        }
        catch (HexDBException e) when (e.Status == 409)
        {
            return await _db.RequestAsync(HttpMethod.Get, $"/tessellations/{Uri.EscapeDataString(Name)}", cancel: cancel).ConfigureAwait(false);
        }
    }

    /// <summary>Upload a local file to <paramref name="path"/>, keeping its modification time.</summary>
    public async Task<JsonObject> UploadFileAsync(string path, string localPath, string? contentType = null, IProgress<long>? progress = null, CancellationToken cancel = default)
    {
        await using var file = File.OpenRead(localPath);
        return await UploadAsync(path, file, contentType, File.GetLastWriteTimeUtc(localPath), progress, cancel).ConfigureAwait(false);
    }

    /// <summary>Upload bytes to <paramref name="path"/>.</summary>
    public Task<JsonObject> UploadAsync(string path, byte[] content, string? contentType = null, DateTimeOffset? modified = null, IProgress<long>? progress = null, CancellationToken cancel = default) =>
        UploadAsync(path, new MemoryStream(content, writable: false), contentType, modified, progress, cancel);

    /// <summary>
    /// Upload a stream to <paramref name="path"/>. Seekable streams under 16 MB go in
    /// one request; larger or unseekable ones in resumable 8 MB parts.
    /// </summary>
    public async Task<JsonObject> UploadAsync(string path, System.IO.Stream content, string? contentType = null, DateTimeOffset? modified = null, IProgress<long>? progress = null, CancellationToken cancel = default)
    {
        var modifiedText = modified?.ToUnixTimeMilliseconds().ToString();
        long? size = content.CanSeek ? content.Length - content.Position : null;
        if (size is < ResumableFrom)
        {
            var url = $"{_path}/_files/{FilePath(path)}" + (modifiedText is null ? "" : $"?modified={modifiedText}");
            var body = new StreamContent(content);
            if (contentType is not null) body.Headers.ContentType = new MediaTypeHeaderValue(contentType);
            using var response = await _db.SendRawAsync(HttpMethod.Put, url, body, cancel: cancel).ConfigureAwait(false);
            progress?.Report(size.Value);
            return (await response.Content.ReadFromJsonAsync<JsonObject>(cancel).ConfigureAwait(false))!;
        }
        var session = await _db.RequestAsync(HttpMethod.Post, $"{_path}/_uploads", new { path, size, content_type = contentType, modified = modifiedText }, cancel: cancel).ConfigureAwait(false);
        var sessionUrl = $"{_path}/_uploads/{Uri.EscapeDataString(session!["id"]!.GetValue<string>())}";
        var buffer = new byte[Part];
        long offset = 0;
        while (true)
        {
            var filled = 0;
            int read;
            while (filled < Part && (read = await content.ReadAsync(buffer.AsMemory(filled, Part - filled), cancel).ConfigureAwait(false)) > 0) filled += read;
            if (filled == 0) break;
            for (var attempt = 0; ; attempt++)
            {
                try
                {
                    using var response = await _db.SendRawAsync(HttpMethod.Patch, $"{sessionUrl}?offset={offset}", new ByteArrayContent(buffer, 0, filled), cancel: cancel).ConfigureAwait(false);
                    break;
                }
                catch (HexDBException) when (attempt < 3)
                {
                    await Task.Delay(TimeSpan.FromSeconds(attempt + 1), cancel).ConfigureAwait(false);
                    var status = await _db.RequestAsync(HttpMethod.Get, sessionUrl, cancel: cancel).ConfigureAwait(false);
                    if (status!["offset"]!.GetValue<long>() >= offset + filled) break;
                }
            }
            offset += filled;
            progress?.Report(offset);
            if (filled < Part) break;
        }
        return (await _db.RequestAsync(HttpMethod.Post, $"{sessionUrl}/complete", cancel: cancel).ConfigureAwait(false))!.AsObject();
    }

    private string ContentPath(string pathOrId, int? version)
    {
        var query = version is null ? "" : $"?version={version}";
        return Ulid().IsMatch(pathOrId) ? $"{_path}/{Uri.EscapeDataString(pathOrId)}/content{query}" : $"{_path}/_files/{FilePath(pathOrId)}{query}";
    }

    /// <summary>A file's content (by <c>folder/name</c> or ID), or an earlier version's.</summary>
    public async Task<byte[]> DownloadAsync(string pathOrId, int? version = null, CancellationToken cancel = default)
    {
        using var response = await _db.SendRawAsync(HttpMethod.Get, ContentPath(pathOrId, version), cancel: cancel).ConfigureAwait(false);
        return await response.Content.ReadAsByteArrayAsync(cancel).ConfigureAwait(false);
    }

    /// <summary>Copy a file's content to a stream (for large files).</summary>
    public async Task DownloadToAsync(string pathOrId, System.IO.Stream destination, int? version = null, CancellationToken cancel = default)
    {
        using var response = await _db.SendRawAsync(HttpMethod.Get, ContentPath(pathOrId, version), cancel: cancel).ConfigureAwait(false);
        await response.Content.CopyToAsync(destination, cancel).ConfigureAwait(false);
    }

    /// <summary>Bytes <paramref name="start"/> to <paramref name="end"/> (exclusive) of a file.</summary>
    public async Task<byte[]> ReadRangeAsync(string pathOrId, long start, long end, CancellationToken cancel = default)
    {
        var headers = new Dictionary<string, string> { ["Range"] = $"bytes={start}-{end - 1}" };
        using var response = await _db.SendRawAsync(HttpMethod.Get, ContentPath(pathOrId, null), headers: headers, cancel: cancel).ConfigureAwait(false);
        return await response.Content.ReadAsByteArrayAsync(cancel).ConfigureAwait(false);
    }

    /// <summary>A file's document (metadata), or null.</summary>
    public async Task<JsonObject?> StatAsync(string pathOrId, CancellationToken cancel = default)
    {
        if (Ulid().IsMatch(pathOrId)) return await Documents.GetAsync(pathOrId, cancel).ConfigureAwait(false);
        var parts = pathOrId.Split('/', '\\').Where(p => p.Length > 0).ToList();
        var name = parts.Count > 0 ? parts[^1] : "";
        var folder = parts.Count > 1 ? "/" + string.Join("/", parts.Take(parts.Count - 1)) + "/" : "/";
        var page = await Documents.QueryAsync(new Query { Filter = new { path = folder, name }, Limit = 1 }, cancel).ConfigureAwait(false);
        return page.Documents.FirstOrDefault();
    }

    /// <summary>The files directly in a folder, by name.</summary>
    public async Task<List<JsonObject>> ListAsync(string folder = "/", int limit = 1000, CancellationToken cancel = default)
    {
        var parts = folder.Split('/', '\\').Where(p => p.Length > 0).ToList();
        var path = parts.Count > 0 ? "/" + string.Join("/", parts) + "/" : "/";
        return (await Documents.QueryAsync(new Query { Filter = new { path }, Sort = "name", Limit = limit }, cancel).ConfigureAwait(false)).Documents;
    }

    /// <summary>Files whose name, folders, tags or extracted metadata contain every word of <paramref name="text"/>.</summary>
    public async Task<List<JsonObject>> SearchAsync(string text, int limit = 100, CancellationToken cancel = default) =>
        (await Documents.QueryAsync(new Query { Filter = new Dictionary<string, object> { ["$text"] = text }, Limit = limit }, cancel).ConfigureAwait(false)).Documents;

    /// <summary>A small JPEG or PNG of an image (once its metadata has been read).</summary>
    public async Task<byte[]> ThumbnailAsync(string id, CancellationToken cancel = default)
    {
        using var response = await _db.SendRawAsync(HttpMethod.Get, $"{_path}/{Uri.EscapeDataString(id)}/thumbnail", cancel: cancel).ConfigureAwait(false);
        return await response.Content.ReadAsByteArrayAsync(cancel).ConfigureAwait(false);
    }

    public Task<JsonNode?> VersionsAsync(string id, CancellationToken cancel = default) =>
        _db.RequestAsync(HttpMethod.Get, $"{_path}/{Uri.EscapeDataString(id)}/versions", cancel: cancel);

    /// <summary>Make an earlier version current again (as a new version).</summary>
    public async Task<JsonObject> RestoreVersionAsync(string id, int version, CancellationToken cancel = default) =>
        (await _db.RequestAsync(HttpMethod.Post, $"{_path}/{Uri.EscapeDataString(id)}/versions/{version}/restore", cancel: cancel).ConfigureAwait(false))!.AsObject();

    /// <summary>Move a file to the trash; false if it doesn't exist.</summary>
    public Task<bool> DeleteAsync(string id, CancellationToken cancel = default) => Documents.DeleteAsync(id, cancel: cancel);

    /// <summary>Deleted files.</summary>
    public async Task<List<JsonObject>> TrashAsync(CancellationToken cancel = default)
    {
        var trash = await _db.RequestAsync(HttpMethod.Get, $"{_path}/_trash", cancel: cancel).ConfigureAwait(false);
        return trash!["documents"]!.AsArray().Select(d => d!.AsObject()).ToList();
    }

    /// <summary>Put a trashed file back (optionally under another name or folder).</summary>
    public async Task<JsonObject> RestoreAsync(string id, string? name = null, string? path = null, CancellationToken cancel = default)
    {
        var query = new List<string>();
        if (name is not null) query.Add($"name={Uri.EscapeDataString(name)}");
        if (path is not null) query.Add($"path={Uri.EscapeDataString(path)}");
        var url = $"{_path}/_trash/{Uri.EscapeDataString(id)}/restore" + (query.Count > 0 ? "?" + string.Join("&", query) : "");
        return (await _db.RequestAsync(HttpMethod.Post, url, cancel: cancel).ConfigureAwait(false))!.AsObject();
    }

    /// <summary>Delete a trashed file for good.</summary>
    public Task PurgeAsync(string id, CancellationToken cancel = default) =>
        _db.RequestAsync(HttpMethod.Delete, $"{_path}/_trash/{Uri.EscapeDataString(id)}", cancel: cancel);

    /// <summary>Read metadata again (one file, or all); returns how many were queued.</summary>
    public async Task<int> ReextractAsync(string? id = null, CancellationToken cancel = default) =>
        (await _db.RequestAsync(HttpMethod.Post, $"{_path}/_reextract" + (id is null ? "" : $"?id={Uri.EscapeDataString(id)}"), cancel: cancel).ConfigureAwait(false))!["queued"]!.GetValue<int>();
}
