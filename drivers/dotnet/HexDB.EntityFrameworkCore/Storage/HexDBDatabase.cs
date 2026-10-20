// SaveChanges: every added, modified and deleted entity becomes one operation
// of a single HexDB transaction (POST /transactions), so the whole save is
// atomic (inside an explicit transaction, the operations wait for Commit).
// Updates are patches of the changed properties; properties marked as
// concurrency tokens become `if_match` preconditions, so a document changed
// by someone else since it was read fails the save with
// DbUpdateConcurrencyException.

using System.Text.Json.Nodes;
using HexDB.Client;
using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.Infrastructure;
using Microsoft.EntityFrameworkCore.Metadata;
using Microsoft.EntityFrameworkCore.Storage;
using Microsoft.EntityFrameworkCore.Update;

namespace HexDB.EntityFrameworkCore.Storage;

public class HexDBDatabase : Database
{
    private readonly HexDBTransactionManager _transactions;

    public HexDBDatabase(DatabaseDependencies dependencies, IDbContextTransactionManager transactions) : base(dependencies) =>
        _transactions = (HexDBTransactionManager)transactions;

    public override int SaveChanges(IList<IUpdateEntry> entries) => SaveChangesAsync(entries).GetAwaiter().GetResult();

    public override async Task<int> SaveChangesAsync(IList<IUpdateEntry> entries, CancellationToken cancellationToken = default)
    {
        var operations = new List<JsonObject>();
        var withPreconditions = false;
        foreach (var entry in entries)
        {
            var op = Operation(entry);
            if (op is null) continue;
            withPreconditions |= op.ContainsKey("if_match");
            operations.Add(op);
        }
        if (operations.Count == 0) return 0;
        // Inside an explicit transaction the operations wait for Commit.
        if (_transactions.Current is { } transaction)
        {
            transaction.Queue(operations, withPreconditions, entries);
            return operations.Count;
        }
        await _transactions.SendAsync(operations, withPreconditions, entries.ToList(), cancellationToken).ConfigureAwait(false);
        return operations.Count;
    }

    /// <summary>The transaction operation for one entry (null when there's nothing to write).</summary>
    private static JsonObject? Operation(IUpdateEntry entry)
    {
        var entityType = entry.EntityType;
        var key = entityType.FindPrimaryKey()!.Properties[0];
        var id = JsonValues.ToJson(entry.GetCurrentValue(key), key)?.GetValue<string>()
            ?? throw new InvalidOperationException($"A '{entityType.DisplayName()}' has no key value.");
        var op = new JsonObject { ["tessellation"] = entityType.GetTessellation(), ["id"] = id };
        switch (entry.EntityState)
        {
            case EntityState.Added:
                op["op"] = "insert";
                op["data"] = Data(entry, entityType.GetProperties().Where(p => !p.IsPrimaryKey()), original: false);
                break;
            case EntityState.Modified:
                var changed = entityType.GetProperties().Where(p => !p.IsPrimaryKey() && entry.IsModified(p)).ToList();
                if (changed.Count == 0) return null;
                op["op"] = "patch";
                // A null removes the field (a JSON merge patch); reads see a missing field as null.
                op["data"] = Data(entry, changed, original: false, keepNulls: true);
                break;
            case EntityState.Deleted:
                op["op"] = "delete";
                break;
            default:
                return null;
        }
        if (entry.EntityState is EntityState.Modified or EntityState.Deleted)
        {
            var tokens = entityType.GetProperties().Where(p => p.IsConcurrencyToken && !p.IsPrimaryKey()).ToList();
            if (tokens.Count > 0)
            {
                var match = new JsonObject();
                foreach (var token in tokens)
                {
                    match[token.GetJsonPropertyName()] = JsonValues.ToJson(entry.GetOriginalValue(token), token);
                }
                op["if_match"] = match;
            }
        }
        return op;
    }

    private static JsonObject Data(IUpdateEntry entry, IEnumerable<IProperty> properties, bool original, bool keepNulls = false)
    {
        var data = new JsonObject();
        foreach (var property in properties)
        {
            var value = JsonValues.ToJson(original ? entry.GetOriginalValue(property) : entry.GetCurrentValue(property), property);
            if (value is not null || keepNulls) data[property.GetJsonPropertyName()] = value;
        }
        return data;
    }
}

/// <summary>EnsureCreated creates the model's tessellations; EnsureDeleted deletes them.</summary>
public class HexDBDatabaseCreator : IDatabaseCreator
{
    private readonly HexDBConnection _connection;
    private readonly ICurrentDbContext _context;

    public HexDBDatabaseCreator(HexDBConnection connection, ICurrentDbContext context)
    {
        _connection = connection;
        _context = context;
    }

    private IEnumerable<string> Tessellations => _context.Context.Model.GetEntityTypes().Select(e => e.GetTessellation()).Distinct();

    public bool EnsureCreated() => EnsureCreatedAsync().GetAwaiter().GetResult();

    public async Task<bool> EnsureCreatedAsync(CancellationToken cancellationToken = default)
    {
        var created = false;
        foreach (var name in Tessellations)
        {
            try
            {
                await _connection.Client.RequestAsync(HttpMethod.Post, "/tessellations", new { name }, cancel: cancellationToken).ConfigureAwait(false);
                created = true;
            }
            catch (HexDBException e) when (e.Status == 409)
            {
                // It exists already.
            }
        }
        return created;
    }

    public bool EnsureDeleted() => EnsureDeletedAsync().GetAwaiter().GetResult();

    public async Task<bool> EnsureDeletedAsync(CancellationToken cancellationToken = default)
    {
        var deleted = false;
        foreach (var name in Tessellations)
        {
            try
            {
                await _connection.Client.RequestAsync(HttpMethod.Delete, $"/tessellations/{Uri.EscapeDataString(name)}", cancel: cancellationToken).ConfigureAwait(false);
                deleted = true;
            }
            catch (HexDBException e) when (e.Status == 404)
            {
                // There was nothing to delete.
            }
        }
        return deleted;
    }

    public bool CanConnect() => CanConnectAsync().GetAwaiter().GetResult();

    public async Task<bool> CanConnectAsync(CancellationToken cancellationToken = default)
    {
        try
        {
            await _connection.Client.HealthAsync(cancellationToken).ConfigureAwait(false);
            return true;
        }
        catch (HexDBException)
        {
            return false;
        }
    }
}
