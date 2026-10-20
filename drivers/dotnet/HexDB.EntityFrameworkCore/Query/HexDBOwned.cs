// Owned types: an owned entity is stored inside its owner's document, as a
// nested object (OwnsOne) or an array of objects (OwnsMany), named after the
// navigation (or ToJsonProperty on it). Owned entities have no tessellation and
// no id of their own: their key is their owner's key (plus their position, for
// collections), which EF Core needs for change tracking but HexDB doesn't store.
//
// Reading: each document's ValueBuffer carries the document's JSON in its last
// slot. Wherever a query materializes an entity, LoadOwned materializes its
// owned entities from that JSON with EF Core's own materializer (so they're
// tracked and fixed up like any entity), recursively for owned types inside
// owned types.

using System.Linq.Expressions;
using System.Reflection;
using System.Text.Json.Nodes;
using HexDB.EntityFrameworkCore.Storage;
using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.Metadata;
using Microsoft.EntityFrameworkCore.Query;
using Microsoft.EntityFrameworkCore.Storage;

namespace HexDB.EntityFrameworkCore.Query;

/// <summary>How to read the owned entities of one entity type hierarchy.</summary>
public sealed class OwnedPlan
{
    private readonly Dictionary<Type, (IEntityType Type, List<OwnedNavigationPlan> Navigations)> _byClrType = new();

    private OwnedPlan()
    {
    }

    /// <summary>The plan for a type and its derived types, or null when none of them owns anything.</summary>
    internal static OwnedPlan? Build(IEntityType entityType, Func<IEntityType, Func<QueryContext, ValueBuffer, object?>> materializer)
    {
        var plan = new OwnedPlan();
        var any = false;
        foreach (var type in entityType.GetDerivedTypesInclusive())
        {
            var navigations = new List<OwnedNavigationPlan>();
            foreach (var navigation in type.GetNavigations().Where(n => n.TargetEntityType.IsOwned() && n.ForeignKey.IsOwnership && !n.IsOnDependent))
            {
                navigations.Add(new OwnedNavigationPlan(navigation, materializer(navigation.TargetEntityType), Build(navigation.TargetEntityType, materializer)));
                any = true;
            }
            plan._byClrType[type.ClrType] = (type, navigations);
        }
        return any ? plan : null;
    }

    internal bool TryGet(Type clrType, out IEntityType type, out List<OwnedNavigationPlan> navigations)
    {
        if (_byClrType.TryGetValue(clrType, out var found))
        {
            (type, navigations) = found;
            return true;
        }
        type = null!;
        navigations = null!;
        return false;
    }
}

/// <summary>One owned navigation: where its JSON is and how to materialize it.</summary>
public sealed class OwnedNavigationPlan
{
    internal OwnedNavigationPlan(INavigation navigation, Func<QueryContext, ValueBuffer, object?> materialize, OwnedPlan? inner)
    {
        Navigation = navigation;
        Target = navigation.TargetEntityType;
        JsonName = navigation.GetJsonPropertyName();
        Materialize = materialize;
        Inner = inner;
    }

    public INavigation Navigation { get; }
    public IEntityType Target { get; }
    public string JsonName { get; }
    public Func<QueryContext, ValueBuffer, object?> Materialize { get; }
    public OwnedPlan? Inner { get; }
}

/// <summary>Materializing owned entities from their owner's JSON.</summary>
public static class OwnedLoader
{
    internal static readonly MethodInfo LoadMethod = typeof(OwnedLoader).GetMethod(nameof(Load))!;

    /// <summary>The JSON a ValueBuffer was made from (its last slot), if any.</summary>
    internal static JsonObject? Json(ValueBuffer buffer) => buffer.Count > 0 ? buffer[buffer.Count - 1] as JsonObject : null;

    /// <summary>
    /// Materialize an entity's owned entities (called right after the entity
    /// itself is materialized). Returns the entity.
    /// </summary>
    public static TEntity Load<TEntity>(QueryContext queryContext, TEntity entity, ValueBuffer buffer, OwnedPlan plan, bool tracking)
    {
        if (entity is null || Json(buffer) is not { } json || !plan.TryGet(entity.GetType(), out var type, out var navigations)) return entity;
        var ownerKey = type.FindPrimaryKey()!.Properties.Select(p => buffer[p.GetIndex()]).ToArray();
        foreach (var owned in navigations)
        {
            var node = json[owned.JsonName];
            if (owned.Navigation.IsCollection)
            {
                var accessor = owned.Navigation.GetCollectionAccessor()!;
                accessor.GetOrCreate(entity, forMaterialization: true);
                if (node is JsonArray items)
                {
                    for (var i = 0; i < items.Count; i++)
                    {
                        if (items[i] is not JsonObject item) continue;
                        var child = One(queryContext, owned, item, ownerKey, i + 1, tracking);
                        if (child is not null && !tracking) accessor.Add(entity, child, forMaterialization: true);
                    }
                }
                if (tracking) queryContext.SetNavigationIsLoaded(entity, owned.Navigation);
                else owned.Navigation.SetIsLoadedWhenNoTracking(entity);
            }
            else
            {
                var child = node is JsonObject item ? One(queryContext, owned, item, ownerKey, null, tracking) : null;
                if (!tracking)
                {
                    if (child is not null) Set(entity, owned.Navigation, child);
                    owned.Navigation.SetIsLoadedWhenNoTracking(entity);
                }
                else if (child is null)
                {
                    queryContext.SetNavigationIsLoaded(entity, owned.Navigation);
                }
            }
        }
        return entity;
    }

    private static object? One(QueryContext queryContext, OwnedNavigationPlan owned, JsonObject json, object?[] ownerKey, int? ordinal, bool tracking)
    {
        var buffer = Buffer(owned.Target, json, ownerKey, ordinal);
        var child = owned.Materialize(queryContext, buffer);
        if (child is not null && owned.Inner is not null) Load(queryContext, child, buffer, owned.Inner, tracking);
        return child;
    }

    /// <summary>An owned entity's ValueBuffer: its owner's key, its position, its fields, and its JSON.</summary>
    internal static ValueBuffer Buffer(IEntityType ownedType, JsonObject json, object?[] ownerKey, int? ordinal)
    {
        var ownership = ownedType.FindOwnership()!;
        var properties = ownedType.GetProperties().ToList();
        var values = new object?[properties.Max(p => p.GetIndex()) + 2];
        foreach (var property in properties)
        {
            var fk = IndexOf(ownership.Properties, property);
            if (fk >= 0)
            {
                values[property.GetIndex()] = ownerKey[fk];
            }
            else if (property.IsPrimaryKey())
            {
                // The position in the owner's array (1-based).
                values[property.GetIndex()] = ordinal is null ? null : Convert.ChangeType(ordinal.Value, Nullable.GetUnderlyingType(property.ClrType) ?? property.ClrType);
            }
            else
            {
                values[property.GetIndex()] = JsonValues.FromJson(json[property.GetJsonPropertyName()], property);
            }
        }
        values[^1] = json;
        return new ValueBuffer(values);
    }

    private static int IndexOf(IReadOnlyList<IProperty> list, IProperty property)
    {
        for (var i = 0; i < list.Count; i++)
        {
            if (list[i] == property) return i;
        }
        return -1;
    }

    private static void Set(object entity, INavigation navigation, object value)
    {
        switch (navigation.GetMemberInfo(forMaterialization: true, forSet: true))
        {
            case PropertyInfo property:
                property.SetValue(entity, value);
                break;
            case FieldInfo field:
                field.SetValue(entity, value);
                break;
        }
    }

    /// <summary>Wraps each entity materialization of a shaper in a call to Load.</summary>
    internal sealed class Injector : ExpressionVisitor
    {
        private readonly Func<IEntityType, OwnedPlan?> _plan;
        private readonly bool _tracking;

        public Injector(Func<IEntityType, OwnedPlan?> plan, bool tracking)
        {
            _plan = plan;
            _tracking = tracking;
        }

        protected override Expression VisitExtension(Expression node)
        {
            if (node is StructuralTypeShaperExpression { StructuralType: IEntityType entityType, ValueBufferExpression: ProjectionBindingExpression binding } shaper
                && binding.Type == typeof(ValueBuffer) && _plan(entityType) is { } plan)
            {
                return new OwnedLoadExpression(shaper, binding, plan, _tracking);
            }
            return base.VisitExtension(node);
        }
    }

    /// <summary>Turns each OwnedLoadExpression into the call to Load, once EF Core's materializers are in.</summary>
    internal sealed class Lowering : ExpressionVisitor
    {
        protected override Expression VisitExtension(Expression node) =>
            node is OwnedLoadExpression load
                ? Expression.Call(LoadMethod.MakeGenericMethod(load.Type), QueryCompilationContext.QueryContextParameter, Visit(load.Entity), load.Buffer,
                    Expression.Constant(load.Plan), Expression.Constant(load.Tracking))
                : base.VisitExtension(node);
    }

    /// <summary>Removes EF Core's includes of owned navigations: Load reads them from the document.</summary>
    internal sealed class IncludeStripper : ExpressionVisitor
    {
        protected override Expression VisitExtension(Expression node) =>
            node is IncludeExpression { Navigation: INavigation { TargetEntityType: var target } } include && target.IsOwned()
                ? Visit(include.EntityExpression)
                : base.VisitExtension(node);
    }
}

/// <summary>
/// "Materialize this entity, then its owned entities." Only the entity is a
/// child expression: EF Core turns it into its materializer while the plan
/// stays out of its client-constant checks (it's per compiled query, like the
/// query itself). Lowered to a call to OwnedLoader.Load afterwards.
/// </summary>
public sealed class OwnedLoadExpression : Expression
{
    internal OwnedLoadExpression(Expression entity, ProjectionBindingExpression buffer, OwnedPlan plan, bool tracking)
    {
        Entity = entity;
        Buffer = buffer;
        Plan = plan;
        Tracking = tracking;
    }

    public Expression Entity { get; }
    public ProjectionBindingExpression Buffer { get; }
    public OwnedPlan Plan { get; }
    public bool Tracking { get; }

    public override Type Type => Entity.Type;

    public override ExpressionType NodeType => ExpressionType.Extension;

    protected override Expression VisitChildren(ExpressionVisitor visitor)
    {
        var entity = visitor.Visit(Entity);
        return entity == Entity ? this : new OwnedLoadExpression(entity, Buffer, Plan, Tracking);
    }
}
