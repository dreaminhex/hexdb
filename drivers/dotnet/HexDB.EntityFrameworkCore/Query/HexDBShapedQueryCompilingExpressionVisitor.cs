// Turns a translated query into code: EF Core's generated materializer reads
// each document through a ValueBuffer laid out by property index (so EF Core
// does the materialization, identity resolution and change tracking), and the
// querying enumerable feeds it the documents HexDB returns.

using System.Collections;
using System.Linq.Expressions;
using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.Diagnostics;
using Microsoft.EntityFrameworkCore.Infrastructure;
using Microsoft.EntityFrameworkCore.Metadata;
using Microsoft.EntityFrameworkCore.Query;
using Microsoft.EntityFrameworkCore.Storage;
using HexDB.EntityFrameworkCore.Storage;

namespace HexDB.EntityFrameworkCore.Query;

public class HexDBShapedQueryCompilingExpressionVisitorFactory : IShapedQueryCompilingExpressionVisitorFactory
{
    private readonly ShapedQueryCompilingExpressionVisitorDependencies _dependencies;

    public HexDBShapedQueryCompilingExpressionVisitorFactory(ShapedQueryCompilingExpressionVisitorDependencies dependencies) => _dependencies = dependencies;

    public ShapedQueryCompilingExpressionVisitor Create(QueryCompilationContext queryCompilationContext) =>
        new HexDBShapedQueryCompilingExpressionVisitor(_dependencies, queryCompilationContext);
}

public class HexDBShapedQueryCompilingExpressionVisitor : ShapedQueryCompilingExpressionVisitor
{
    private readonly Type _contextType;
    private readonly bool _threadSafetyChecksEnabled;

    public HexDBShapedQueryCompilingExpressionVisitor(ShapedQueryCompilingExpressionVisitorDependencies dependencies, QueryCompilationContext queryCompilationContext)
        : base(dependencies, queryCompilationContext)
    {
        _contextType = queryCompilationContext.ContextType;
        _threadSafetyChecksEnabled = dependencies.CoreSingletonOptions.AreThreadSafetyChecksEnabled;
    }

    protected override Expression VisitShapedQuery(ShapedQueryExpression shapedQueryExpression)
    {
        var query = (HexDBQueryExpression)shapedQueryExpression.QueryExpression;
        var valueBuffer = Expression.Parameter(typeof(ValueBuffer), "valueBuffer");
        var tracking = QueryCompilationContext.QueryTrackingBehavior == QueryTrackingBehavior.TrackAll;
        // Owned entities come from the document (OwnedLoader), not from EF Core's includes.
        var shaper = new OwnedLoader.IncludeStripper().Visit(shapedQueryExpression.ShaperExpression);
        var plans = new Dictionary<IEntityType, OwnedPlan?>();
        // Includes of related entities (taken out of the query by the preprocessor) apply when the query returns entities.
        List<IncludeNode>? includes = null;
        if (shaper is StructuralTypeShaperExpression { StructuralType: IEntityType rootType } && QueryCompilationContext is HexDBQueryCompilationContext context)
        {
            includes = IncludeNode.Build(rootType, context.IncludePaths, !context.UserIgnoresAutoIncludes, t => Materializer(t, query), t => Plan(t, query, plans));
            if (includes.Count == 0) includes = null;
        }
        shaper = new OwnedLoader.Injector(t => Plan(t, query, plans), tracking).Visit(shaper);
        shaper = InjectEntityMaterializers(shaper);
        shaper = new OwnedLoader.Lowering().Visit(shaper);
        shaper = new ValueBufferRewriter(valueBuffer).Visit(shaper);
        var lambda = Expression.Lambda(shaper, QueryCompilationContext.QueryContextParameter, valueBuffer);
        return Expression.New(
            typeof(HexDBQueryingEnumerable<>).MakeGenericType(lambda.ReturnType).GetConstructors()[0],
            QueryCompilationContext.QueryContextParameter,
            Expression.Constant(query),
            Expression.Constant(lambda.Compile()),
            Expression.Constant(_contextType),
            Expression.Constant(QueryCompilationContext.QueryTrackingBehavior == QueryTrackingBehavior.NoTrackingWithIdentityResolution),
            Expression.Constant(_threadSafetyChecksEnabled),
            Expression.Constant(includes, typeof(List<IncludeNode>)),
            Expression.Constant(tracking));
    }

    private OwnedPlan? Plan(IEntityType entityType, HexDBQueryExpression query, Dictionary<IEntityType, OwnedPlan?> plans)
    {
        if (!plans.TryGetValue(entityType, out var plan))
        {
            plan = OwnedPlan.Build(entityType, owned => Materializer(owned, query));
            plans[entityType] = plan;
        }
        return plan;
    }

    /// <summary>EF Core's materializer for an (owned) entity type, reading a ValueBuffer.</summary>
    private Func<QueryContext, ValueBuffer, object?> Materializer(IEntityType entityType, HexDBQueryExpression query)
    {
        var valueBuffer = Expression.Parameter(typeof(ValueBuffer), "valueBuffer");
        var binding = new ProjectionBindingExpression(query, new ProjectionMember(), typeof(ValueBuffer));
        var shaper = Expression.Convert(new StructuralTypeShaperExpression(entityType, binding, nullable: false), typeof(object));
        // EF Core only builds a tracking materializer for an owned type next to its
        // owners'. They're here, in a branch that never runs: by the time an owned
        // entity is materialized, LoadOwned has its owner (tracked) already.
        var owners = new List<Expression>();
        for (var owner = entityType.FindOwnership()?.PrincipalEntityType; owner is not null; owner = owner.FindOwnership()?.PrincipalEntityType)
        {
            owners.Add(Expression.Convert(new StructuralTypeShaperExpression(owner, binding, nullable: true), typeof(object)));
        }
        Expression body = owners.Count == 0
            ? shaper
            : Expression.Condition(Expression.Constant(false), Expression.Block(typeof(object), owners.Append(Expression.Constant(null, typeof(object)))), shaper);
        body = new ValueBufferRewriter(valueBuffer).Visit(InjectEntityMaterializers(body));
        return Expression.Lambda<Func<QueryContext, ValueBuffer, object?>>(body, QueryCompilationContext.QueryContextParameter, valueBuffer).Compile();
    }

    /// <summary>Points the materializer's reads at our ValueBuffer: entity values by property index, scalars at 0.</summary>
    private sealed class ValueBufferRewriter : ExpressionVisitor
    {
        private readonly ParameterExpression _valueBuffer;

        public ValueBufferRewriter(ParameterExpression valueBuffer) => _valueBuffer = valueBuffer;

        protected override Expression VisitExtension(Expression node) =>
            node is ProjectionBindingExpression binding
                ? binding.Type == typeof(ValueBuffer) ? _valueBuffer : _valueBuffer.CreateValueBufferReadValueExpression(binding.Type, 0, null)
                : base.VisitExtension(node);

        protected override Expression VisitMethodCall(MethodCallExpression node)
        {
            if (node.Method.IsGenericMethod && node.Method.GetGenericMethodDefinition() == ExpressionExtensions.ValueBufferTryReadValueMethod
                && node.Arguments[2] is ConstantExpression { Value: IProperty property })
            {
                return Expression.Call(node.Method, _valueBuffer, Expression.Constant(property.GetIndex()), node.Arguments[2]);
            }
            return base.VisitMethodCall(node);
        }
    }
}

/// <summary>The rows of a HexDB query, shaped into results (entities, projections or scalars).</summary>
public sealed class HexDBQueryingEnumerable<T> : IEnumerable<T>, IAsyncEnumerable<T>, IQueryingEnumerable
{
    private readonly QueryContext _queryContext;
    private readonly HexDBQueryExpression _query;
    private readonly Func<QueryContext, ValueBuffer, T> _shaper;
    private readonly Type _contextType;
    private readonly bool _standAloneStateManager;
    private readonly bool _threadSafetyChecksEnabled;
    private readonly List<IncludeNode>? _includes;
    private readonly bool _tracking;

    public HexDBQueryingEnumerable(QueryContext queryContext, HexDBQueryExpression query, Func<QueryContext, ValueBuffer, T> shaper, Type contextType, bool standAloneStateManager, bool threadSafetyChecksEnabled,
        List<IncludeNode>? includes, bool tracking)
    {
        _includes = includes;
        _tracking = tracking;
        _queryContext = queryContext;
        _query = query;
        _shaper = shaper;
        _contextType = contextType;
        _standAloneStateManager = standAloneStateManager;
        _threadSafetyChecksEnabled = threadSafetyChecksEnabled;
    }

    public string ToQueryString() => _query.Describe();

    public IEnumerator<T> GetEnumerator()
    {
        using var guard = Guard();
        _queryContext.InitializeStateManager(_standAloneStateManager);
        if (_includes is null)
        {
            foreach (var row in Rows(() => _query.Execute(_queryContext))) yield return _shaper(_queryContext, row);
            yield break;
        }
        // With includes: a page of results, then their related entities, then the page.
        var page = new List<(object Entity, ValueBuffer Buffer)>();
        var results = new List<T>();
        foreach (var row in Rows(() => _query.Execute(_queryContext)))
        {
            var result = _shaper(_queryContext, row);
            results.Add(result);
            if (result is not null) page.Add((result, row));
            if (results.Count >= PageSize)
            {
                IncludeLoader.LoadAsync(_queryContext, page, _includes, _tracking, CancellationToken.None).GetAwaiter().GetResult();
                foreach (var r in results) yield return r;
                page.Clear();
                results.Clear();
            }
        }
        IncludeLoader.LoadAsync(_queryContext, page, _includes, _tracking, CancellationToken.None).GetAwaiter().GetResult();
        foreach (var r in results) yield return r;
    }

    private int PageSize => ((HexDBQueryContext)_queryContext).Connection.Options.PageSize;

    IEnumerator IEnumerable.GetEnumerator() => GetEnumerator();

    public async IAsyncEnumerator<T> GetAsyncEnumerator(CancellationToken cancellationToken = default)
    {
        using var guard = Guard();
        _queryContext.InitializeStateManager(_standAloneStateManager);
        await using var rows = _query.ExecuteAsync(_queryContext, cancellationToken).GetAsyncEnumerator(cancellationToken);
        var page = new List<(object Entity, ValueBuffer Buffer)>();
        var results = new List<T>();
        while (true)
        {
            bool more;
            try
            {
                more = await rows.MoveNextAsync().ConfigureAwait(false);
            }
            catch (Exception e)
            {
                Log(e, cancellationToken);
                throw;
            }
            if (!more) break;
            if (_includes is null)
            {
                yield return _shaper(_queryContext, rows.Current);
                continue;
            }
            var result = _shaper(_queryContext, rows.Current);
            results.Add(result);
            if (result is not null) page.Add((result, rows.Current));
            if (results.Count >= PageSize)
            {
                await IncludeLoader.LoadAsync(_queryContext, page, _includes, _tracking, cancellationToken).ConfigureAwait(false);
                foreach (var r in results) yield return r;
                page.Clear();
                results.Clear();
            }
        }
        if (_includes is not null)
        {
            await IncludeLoader.LoadAsync(_queryContext, page, _includes, _tracking, cancellationToken).ConfigureAwait(false);
            foreach (var r in results) yield return r;
        }
    }

    private IEnumerable<ValueBuffer> Rows(Func<IEnumerable<ValueBuffer>> rows)
    {
        using var enumerator = rows().GetEnumerator();
        while (true)
        {
            bool more;
            try
            {
                more = enumerator.MoveNext();
            }
            catch (Exception e)
            {
                Log(e, default);
                throw;
            }
            if (!more) yield break;
            yield return enumerator.Current;
        }
    }

    private void Log(Exception e, CancellationToken cancellationToken)
    {
        if (_queryContext.ExceptionDetector.IsCancellation(e, cancellationToken)) _queryContext.QueryLogger.QueryCanceled(_contextType);
        else _queryContext.QueryLogger.QueryIterationFailed(_contextType, e);
    }

    /// <summary>EF Core's check that a context isn't used by two threads at once.</summary>
    private IDisposable? Guard()
    {
        if (!_threadSafetyChecksEnabled) return null;
        _queryContext.ConcurrencyDetector.EnterCriticalSection();
        return new Exit(_queryContext.ConcurrencyDetector);
    }

    private sealed class Exit : IDisposable
    {
        private readonly IConcurrencyDetector _detector;
        public Exit(IConcurrencyDetector detector) => _detector = detector;
        public void Dispose() => _detector.ExitCriticalSection();
    }
}

/// <summary>A query context with the context's HexDB connection.</summary>
public class HexDBQueryContext : QueryContext
{
    public HexDBQueryContext(QueryContextDependencies dependencies, HexDBConnection connection) : base(dependencies) => Connection = connection;

    public HexDBConnection Connection { get; }
}

public class HexDBQueryContextFactory : IQueryContextFactory
{
    private readonly QueryContextDependencies _dependencies;
    private readonly HexDBConnection _connection;

    public HexDBQueryContextFactory(QueryContextDependencies dependencies, HexDBConnection connection)
    {
        _dependencies = dependencies;
        _connection = connection;
    }

    public QueryContext Create() => new HexDBQueryContext(_dependencies, _connection);
}
