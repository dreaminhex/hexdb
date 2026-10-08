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
        var shaper = InjectEntityMaterializers(shapedQueryExpression.ShaperExpression);
        shaper = new ValueBufferRewriter(valueBuffer).Visit(shaper);
        var lambda = Expression.Lambda(shaper, QueryCompilationContext.QueryContextParameter, valueBuffer);
        return Expression.New(
            typeof(HexDBQueryingEnumerable<>).MakeGenericType(lambda.ReturnType).GetConstructors()[0],
            QueryCompilationContext.QueryContextParameter,
            Expression.Constant(query),
            Expression.Constant(lambda.Compile()),
            Expression.Constant(_contextType),
            Expression.Constant(QueryCompilationContext.QueryTrackingBehavior == QueryTrackingBehavior.NoTrackingWithIdentityResolution),
            Expression.Constant(_threadSafetyChecksEnabled));
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

    public HexDBQueryingEnumerable(QueryContext queryContext, HexDBQueryExpression query, Func<QueryContext, ValueBuffer, T> shaper, Type contextType, bool standAloneStateManager, bool threadSafetyChecksEnabled)
    {
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
        foreach (var row in Rows(() => _query.Execute(_queryContext))) yield return _shaper(_queryContext, row);
    }

    IEnumerator IEnumerable.GetEnumerator() => GetEnumerator();

    public async IAsyncEnumerator<T> GetAsyncEnumerator(CancellationToken cancellationToken = default)
    {
        using var guard = Guard();
        _queryContext.InitializeStateManager(_standAloneStateManager);
        await using var rows = _query.ExecuteAsync(_queryContext, cancellationToken).GetAsyncEnumerator(cancellationToken);
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
            if (!more) yield break;
            yield return _shaper(_queryContext, rows.Current);
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
