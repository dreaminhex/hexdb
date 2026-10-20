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
        // One operation per document: owned entities are part of their root's document.
        var roots = new List<IUpdateEntry>();
        var changedOwned = new Dictionary<IUpdateEntry, HashSet<INavigation>>(ReferenceEqualityComparer.Instance);
        foreach (var entry in entries)
        {
            var (root, navigation) = Root(entry);
            if (root is null) continue;
            if (!changedOwned.TryGetValue(root, out var navigations))
            {
                changedOwned[root] = navigations = new HashSet<INavigation>();
                roots.Add(root);
            }
            if (navigation is not null) navigations.Add(navigation);
        }

        var operations = new List<JsonObject>();
        var withPreconditions = false;
        foreach (var root in roots)
        {
            var op = Operation(root, changedOwned[root]);
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

    /// <summary>
    /// The document an entry belongs to: itself, or for an owned entity its
    /// root owner and the root's navigation that leads to it.
    /// </summary>
    private static (IUpdateEntry? Root, INavigation? Navigation) Root(IUpdateEntry entry)
    {
        INavigation? navigation = null;
        var current = entry;
        while (current.EntityType.FindOwnership() is { } ownership)
        {
            var owner = Owner(current, ownership);
            if (owner is null) return (null, null);
            navigation = ownership.PrincipalToDependent;
            current = owner;
        }
        return (current, navigation);
    }

#pragma warning disable EF1001 // EF Core's state manager finds an owned entity's owner and entries.
    private static IUpdateEntry? Owner(IUpdateEntry entry, IForeignKey ownership)
    {
        var internalEntry = (Microsoft.EntityFrameworkCore.ChangeTracking.Internal.InternalEntityEntry)entry;
        var key = ownership.Properties.Select(p => entry.GetCurrentValue(p)).ToArray();
        return internalEntry.StateManager.TryGetEntry(ownership.PrincipalKey, key);
    }

    private static IUpdateEntry? Entry(IUpdateEntry context, object entity, IEntityType type) =>
        ((Microsoft.EntityFrameworkCore.ChangeTracking.Internal.InternalEntityEntry)context).StateManager.TryGetEntry(entity, type, throwOnTypeMismatch: false);
#pragma warning restore EF1001

    /// <summary>The transaction operation for one document (null when there's nothing to write).</summary>
    private static JsonObject? Operation(IUpdateEntry entry, HashSet<INavigation> changedOwned)
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
                var data = Data(entry, entityType.GetProperties().Where(p => !p.IsPrimaryKey()), keepNulls: false);
                foreach (var navigation in OwnedNavigations(entityType))
                {
                    var value = OwnedJson(entry, navigation, keepNulls: false);
                    if (value is not null) data[navigation.GetJsonPropertyName()] = value;
                }
                op["data"] = data;
                break;
            case EntityState.Modified or EntityState.Unchanged:
                var changed = entry.EntityState == EntityState.Modified
                    ? entityType.GetProperties().Where(p => !p.IsPrimaryKey() && entry.IsModified(p)).ToList()
                    : new List<IProperty>();
                if (changed.Count == 0 && changedOwned.Count == 0) return null;
                op["op"] = "patch";
                // A null removes the field (a JSON merge patch); reads see a missing field as null.
                var patch = Data(entry, changed, keepNulls: true);
                // A changed owned navigation is written whole: its object (merged field by field,
                // with nulls for empty fields) or its array (replaced).
                foreach (var navigation in changedOwned)
                {
                    patch[navigation.GetJsonPropertyName()] = OwnedJson(entry, navigation, keepNulls: true);
                }
                op["data"] = patch;
                break;
            case EntityState.Deleted:
                op["op"] = "delete";
                break;
            default:
                return null;
        }
        if (entry.EntityState is EntityState.Modified or EntityState.Unchanged or EntityState.Deleted)
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

    private static IEnumerable<INavigation> OwnedNavigations(IEntityType entityType) =>
        entityType.GetNavigations().Where(n => n.ForeignKey.IsOwnership && !n.IsOnDependent);

    private static JsonObject Data(IUpdateEntry entry, IEnumerable<IProperty> properties, bool keepNulls)
    {
        var data = new JsonObject();
        foreach (var property in properties)
        {
            var value = JsonValues.ToJson(entry.GetCurrentValue(property), property);
            if (value is not null || keepNulls) data[property.GetJsonPropertyName()] = value;
        }
        return data;
    }

    /// <summary>An owned navigation's current value as JSON: an object, an array of objects, or null.</summary>
    private static JsonNode? OwnedJson(IUpdateEntry owner, INavigation navigation, bool keepNulls)
    {
        var entity = owner.ToEntityEntry().Entity;
        var value = navigation.GetGetter().GetClrValue(entity);
        if (value is null) return null;
        if (navigation.IsCollection)
        {
            var array = new JsonArray();
            foreach (var item in (System.Collections.IEnumerable)value)
            {
                if (item is not null) array.Add(OwnedObject(owner, item, navigation.TargetEntityType, keepNulls));
            }
            return array;
        }
        return OwnedObject(owner, value, navigation.TargetEntityType, keepNulls);
    }

    private static JsonObject OwnedObject(IUpdateEntry context, object entity, IEntityType type, bool keepNulls)
    {
        var json = new JsonObject();
        var entry = Entry(context, entity, type);
        foreach (var property in type.GetProperties().Where(p => !p.IsPrimaryKey()))
        {
            var current = entry is not null ? entry.GetCurrentValue(property) : property.GetGetter().GetClrValue(entity);
            var value = JsonValues.ToJson(current, property);
            if (value is not null || keepNulls) json[property.GetJsonPropertyName()] = value;
        }
        if (entry is not null)
        {
            foreach (var navigation in OwnedNavigations(type))
            {
                var value = OwnedJson(entry, navigation, keepNulls);
                if (value is not null || keepNulls) json[navigation.GetJsonPropertyName()] = value;
            }
        }
        return json;
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

    private IEnumerable<string> Tessellations => _context.Context.Model.GetEntityTypes().Where(e => !e.IsOwned()).Select(e => e.GetTessellation()).Distinct();

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
