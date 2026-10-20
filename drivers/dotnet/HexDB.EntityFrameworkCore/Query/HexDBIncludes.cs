// Relationships: Include and ThenInclude across tessellations.
//
// A relational provider joins; HexDB loads related entities the way EF Core's
// Load() does. Before EF Core expands navigations (which would turn includes
// into joins), the preprocessor takes the Include calls out of the query and
// remembers their paths. As results arrive, a page at a time, each included
// navigation becomes one more query on the related tessellation: its key (or
// foreign key) $in the page's values. The related documents are materialized
// by EF Core's materializer, so tracking queries fix up navigations through
// the state manager; no-tracking queries are connected here, by key.
//
// Filtered includes (Include(x => x.Lines.Where(...))) aren't supported, and
// navigations can't be used in filters or projections that run on the server.

using System.Collections;
using System.Linq.Expressions;
using System.Reflection;
using HexDB.EntityFrameworkCore.Storage;
using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.Metadata;
using Microsoft.EntityFrameworkCore.Query;
using Microsoft.EntityFrameworkCore.Storage;

namespace HexDB.EntityFrameworkCore.Query;

/// <summary>A query compilation context that carries the include paths taken out of the query.</summary>
public class HexDBQueryCompilationContext : QueryCompilationContext
{
    public HexDBQueryCompilationContext(QueryCompilationContextDependencies dependencies, bool async) : base(dependencies, async) { }

    /// <summary>Navigation names, from the query's root entity type.</summary>
    public List<string[]> IncludePaths { get; } = new();

    /// <summary>Whether the query called IgnoreAutoIncludes.</summary>
    public bool UserIgnoresAutoIncludes { get; set; }
}

public class HexDBQueryCompilationContextFactory : IQueryCompilationContextFactory
{
    private readonly QueryCompilationContextDependencies _dependencies;

    public HexDBQueryCompilationContextFactory(QueryCompilationContextDependencies dependencies) => _dependencies = dependencies;

    public QueryCompilationContext Create(bool async) => new HexDBQueryCompilationContext(_dependencies, async);
}

public class HexDBQueryTranslationPreprocessorFactory : IQueryTranslationPreprocessorFactory
{
    private readonly QueryTranslationPreprocessorDependencies _dependencies;

    public HexDBQueryTranslationPreprocessorFactory(QueryTranslationPreprocessorDependencies dependencies) => _dependencies = dependencies;

    public QueryTranslationPreprocessor Create(QueryCompilationContext queryCompilationContext) =>
        new HexDBQueryTranslationPreprocessor(_dependencies, queryCompilationContext);
}

/// <summary>Takes Include calls out of a query before EF Core expands navigations.</summary>
public class HexDBQueryTranslationPreprocessor : QueryTranslationPreprocessor
{
    private static readonly MethodInfo IgnoreAutoIncludesMethod =
        typeof(EntityFrameworkQueryableExtensions).GetMethod(nameof(EntityFrameworkQueryableExtensions.IgnoreAutoIncludes))!;

    public HexDBQueryTranslationPreprocessor(QueryTranslationPreprocessorDependencies dependencies, QueryCompilationContext queryCompilationContext)
        : base(dependencies, queryCompilationContext) { }

    public override Expression Process(Expression query)
    {
        if (QueryCompilationContext is HexDBQueryCompilationContext context)
        {
            var extractor = new IncludeExtractor();
            query = extractor.Visit(query);
            context.IncludePaths.AddRange(extractor.Paths);
            context.UserIgnoresAutoIncludes = extractor.IgnoresAutoIncludes;
            // Auto-includes of related (not owned) entities are loaded like Includes, not joined.
            if (!extractor.IgnoresAutoIncludes && QueryCompilationContext.Model.GetEntityTypes().Any(t => t.GetNavigations().Any(n => n.IsEagerLoaded && !n.TargetEntityType.IsOwned())))
            {
                query = new AutoIncludeSuppressor().Visit(query);
            }
        }
        return base.Process(query);
    }

    private sealed class AutoIncludeSuppressor : ExpressionVisitor
    {
        protected override Expression VisitExtension(Expression node) =>
            node is EntityQueryRootExpression root
                ? Expression.Call(IgnoreAutoIncludesMethod.MakeGenericMethod(root.Type.GetGenericArguments()[0]), root)
                : base.VisitExtension(node);
    }

    /// <summary>Removes Include / ThenInclude, collecting their navigation paths.</summary>
    private sealed class IncludeExtractor : ExpressionVisitor
    {
        public List<string[]> Paths { get; } = new();
        public bool IgnoresAutoIncludes { get; private set; }

        protected override Expression VisitMethodCall(MethodCallExpression node)
        {
            var method = node.Method;
            if (method.DeclaringType != typeof(EntityFrameworkQueryableExtensions)) return base.VisitMethodCall(node);
            if (method.Name == nameof(EntityFrameworkQueryableExtensions.IgnoreAutoIncludes))
            {
                IgnoresAutoIncludes = true;
                return Visit(node.Arguments[0]);
            }
            if (method.Name is not (nameof(EntityFrameworkQueryableExtensions.Include) or nameof(EntityFrameworkQueryableExtensions.ThenInclude)))
                return base.VisitMethodCall(node);

            // Include(...).ThenInclude(...)...: collect the chain from its end.
            var chain = new List<MethodCallExpression>();
            Expression current = node;
            while (current is MethodCallExpression call && call.Method.DeclaringType == typeof(EntityFrameworkQueryableExtensions)
                   && call.Method.Name is nameof(EntityFrameworkQueryableExtensions.Include) or nameof(EntityFrameworkQueryableExtensions.ThenInclude))
            {
                chain.Add(call);
                if (call.Method.Name == nameof(EntityFrameworkQueryableExtensions.Include)) break;
                current = call.Arguments[0];
            }
            chain.Reverse();
            var include = chain[0];
            var path = new List<string>();
            if (include.Arguments[1] is ConstantExpression { Value: string text })
            {
                path.AddRange(text.Split('.', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries));
            }
            else
            {
                path.AddRange(Navigations(include.Arguments[1]));
            }
            foreach (var then in chain.Skip(1))
            {
                path.AddRange(Navigations(then.Arguments[1]));
            }
            if (path.Count > 0) Paths.Add(path.ToArray());
            return Visit(include.Arguments[0]);
        }

        /// <summary>The navigation names of an include lambda: x => x.A or x => x.A.B (casts allowed).</summary>
        private static IEnumerable<string> Navigations(Expression argument)
        {
            var lambda = (LambdaExpression)(argument is UnaryExpression { NodeType: ExpressionType.Quote } quote ? quote.Operand : argument);
            var names = new Stack<string>();
            var current = lambda.Body;
            while (true)
            {
                while (current is UnaryExpression { NodeType: ExpressionType.Convert or ExpressionType.TypeAs } convert) current = convert.Operand;
                if (current is MemberExpression { Expression: not null } member)
                {
                    names.Push(member.Member.Name);
                    current = member.Expression;
                }
                else
                {
                    break;
                }
            }
            if (current != lambda.Parameters[0] || names.Count == 0)
                throw new InvalidOperationException($"HexDB can't include '{lambda.Body}': include navigations (x => x.Customer, x => x.Lines); filtered includes aren't supported.");
            return names;
        }
    }
}

/// <summary>One included navigation, and what's included beyond it.</summary>
public sealed class IncludeNode
{
    internal IncludeNode(INavigation navigation, Func<QueryContext, ValueBuffer, object?> materialize, OwnedPlan? owned)
    {
        Navigation = navigation;
        Target = navigation.TargetEntityType;
        var foreignKey = navigation.ForeignKey;
        // On the dependent (Order.Customer): find the principal by its key. On the
        // principal (Customer.Orders): find the dependents by their foreign key.
        ParentProperty = navigation.IsOnDependent ? foreignKey.Properties[0] : foreignKey.PrincipalKey.Properties[0];
        ChildProperty = navigation.IsOnDependent ? foreignKey.PrincipalKey.Properties[0] : foreignKey.Properties[0];
        Materialize = materialize;
        Owned = owned;
    }

    public INavigation Navigation { get; }
    public IEntityType Target { get; }
    /// <summary>The parent's property whose values are looked up.</summary>
    public IProperty ParentProperty { get; }
    /// <summary>The related type's property they're matched against.</summary>
    public IProperty ChildProperty { get; }
    public Func<QueryContext, ValueBuffer, object?> Materialize { get; }
    public OwnedPlan? Owned { get; }
    public List<IncludeNode> Children { get; } = new();

    /// <summary>The include tree for a root type, from navigation-name paths.</summary>
    internal static List<IncludeNode> Build(IEntityType root, IEnumerable<string[]> paths, bool autoIncludes, Func<IEntityType, Func<QueryContext, ValueBuffer, object?>> materializer,
        Func<IEntityType, OwnedPlan?> owned)
    {
        var roots = new List<IncludeNode>();
        foreach (var path in paths)
        {
            var nodes = roots;
            IEntityType type = root;
            foreach (var name in path)
            {
                var navigation = type.GetDerivedTypesInclusive().Select(t => t.FindNavigation(name)).FirstOrDefault(n => n is not null)
                    ?? throw new InvalidOperationException(type.GetDerivedTypesInclusive().Any(t => t.FindSkipNavigation(name) is not null)
                        ? $"'{type.DisplayName()}.{name}' is a many-to-many navigation; the HexDB provider doesn't include those. Map the join entity and include through it."
                        : $"'{type.DisplayName()}' has no navigation '{name}' to include.");
                if (navigation.TargetEntityType.IsOwned())
                {
                    // Owned entities are always loaded with their owner.
                    break;
                }
                var node = nodes.FirstOrDefault(n => n.Navigation == navigation);
                if (node is null)
                {
                    node = new IncludeNode(navigation, materializer(navigation.TargetEntityType), owned(navigation.TargetEntityType));
                    nodes.Add(node);
                }
                nodes = node.Children;
                type = navigation.TargetEntityType;
            }
        }
        if (autoIncludes) AddAutoIncludes(root, roots, materializer, owned, new HashSet<IEntityType>());
        return roots;
    }

    private static void AddAutoIncludes(IEntityType type, List<IncludeNode> nodes, Func<IEntityType, Func<QueryContext, ValueBuffer, object?>> materializer,
        Func<IEntityType, OwnedPlan?> owned, HashSet<IEntityType> seen)
    {
        if (!seen.Add(type)) return;
        foreach (var navigation in type.GetDerivedTypesInclusive().SelectMany(t => t.GetDeclaredNavigations()).Where(n => n.IsEagerLoaded && !n.TargetEntityType.IsOwned()))
        {
            var node = nodes.FirstOrDefault(n => n.Navigation == navigation);
            if (node is null)
            {
                node = new IncludeNode(navigation, materializer(navigation.TargetEntityType), owned(navigation.TargetEntityType));
                nodes.Add(node);
            }
            AddAutoIncludes(navigation.TargetEntityType, node.Children, materializer, owned, seen);
        }
        foreach (var node in nodes)
        {
            AddAutoIncludes(node.Target, node.Children, materializer, owned, seen);
        }
    }
}

/// <summary>Loading included entities for a page of results.</summary>
public static class IncludeLoader
{
    /// <summary>Keys per $in query.</summary>
    private const int Batch = 500;

    public static async Task LoadAsync(QueryContext context, IReadOnlyList<(object Entity, ValueBuffer Buffer)> parents, IReadOnlyList<IncludeNode> nodes, bool tracking,
        CancellationToken cancel)
    {
        foreach (var node in nodes)
        {
            // Only parents of a type that has this navigation (in a hierarchy).
            var relevant = parents.Where(p => node.Navigation.DeclaringEntityType.ClrType.IsInstanceOfType(p.Entity)).ToList();
            if (relevant.Count == 0) continue;
            var keys = relevant.Select(p => p.Buffer[node.ParentProperty.GetIndex()]).Where(k => k is not null).Distinct().ToList();
            var children = new List<(object Entity, ValueBuffer Buffer)>();
            foreach (var batch in keys.Chunk(Batch))
            {
                var query = new HexDBQueryExpression(node.Target);
                query.Filters.Add(new FieldCondition(node.ChildProperty.GetJsonPropertyName(), "$in", ValueSource.From(Expression.Constant(batch.ToList())), node.ChildProperty));
                await foreach (var buffer in query.ExecuteAsync(context, cancel).ConfigureAwait(false))
                {
                    var child = node.Materialize(context, buffer);
                    if (child is null) continue;
                    if (node.Owned is not null) OwnedLoader.Load(context, child, buffer, node.Owned, tracking);
                    children.Add((child, buffer));
                }
            }
            Connect(context, relevant, children, node, tracking);
            if (node.Children.Count > 0 && children.Count > 0) await LoadAsync(context, children, node.Children, tracking, cancel).ConfigureAwait(false);
        }
    }

    /// <summary>
    /// Tracking queries: the state manager has fixed up navigations as the related
    /// entities were tracked; mark them loaded. No-tracking: connect them here.
    /// </summary>
    private static void Connect(QueryContext context, List<(object Entity, ValueBuffer Buffer)> parents, List<(object Entity, ValueBuffer Buffer)> children, IncludeNode node, bool tracking)
    {
        var navigation = node.Navigation;
        var inverse = navigation.Inverse;
        if (tracking)
        {
            foreach (var parent in parents) context.SetNavigationIsLoaded(parent.Entity, navigation);
            return;
        }
        var byKey = new Dictionary<object, List<object>>();
        foreach (var child in children)
        {
            var key = child.Buffer[node.ChildProperty.GetIndex()];
            if (key is null) continue;
            if (!byKey.TryGetValue(key, out var list)) byKey[key] = list = new List<object>();
            list.Add(child.Entity);
        }
        foreach (var parent in parents)
        {
            var key = parent.Buffer[node.ParentProperty.GetIndex()];
            var related = key is not null && byKey.TryGetValue(key, out var found) ? found : new List<object>();
            if (navigation.IsCollection)
            {
                var accessor = navigation.GetCollectionAccessor()!;
                accessor.GetOrCreate(parent.Entity, forMaterialization: true);
                foreach (var child in related)
                {
                    accessor.Add(parent.Entity, child, forMaterialization: true);
                    SetInverse(inverse, child, parent.Entity);
                }
            }
            else if (related.Count > 0)
            {
                Set(navigation, parent.Entity, related[0]);
                SetInverse(inverse, related[0], parent.Entity);
            }
            navigation.SetIsLoadedWhenNoTracking(parent.Entity);
        }
    }

    private static void SetInverse(INavigation? inverse, object child, object parent)
    {
        if (inverse is null) return;
        if (inverse.IsCollection)
        {
            var accessor = inverse.GetCollectionAccessor()!;
            if (!accessor.Contains(child, parent)) accessor.Add(child, parent, forMaterialization: true);
        }
        else
        {
            Set(inverse, child, parent);
            inverse.SetIsLoadedWhenNoTracking(child);
        }
    }

    private static void Set(INavigation navigation, object entity, object value)
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
}
