// LINQ to HexDB. Each supported operator narrows the HexDB query; anything
// else returns null, and EF Core reports that the LINQ expression couldn't be
// translated (rather than loading every document to evaluate it in memory).
//
//   Where           ==, !=, <, <=, >, >=, &&, ||, !, null checks, bool members,
//                   string.Contains/StartsWith/EndsWith, list.Contains(x.P) ($in),
//                   x.Collection.Contains(v), string.IsNullOrEmpty, Equals
//   OrderBy/ThenBy  (Descending) on properties
//   Skip, Take, First(OrDefault), Single(OrDefault), Count, LongCount, Any
//   Select          projections run on the client, after the documents are read

using System.Linq.Expressions;
using System.Reflection;
using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.Metadata;
using Microsoft.EntityFrameworkCore.Query;
using Microsoft.EntityFrameworkCore.Storage;

namespace HexDB.EntityFrameworkCore.Query;

public class HexDBQueryableMethodTranslatingExpressionVisitorFactory : IQueryableMethodTranslatingExpressionVisitorFactory
{
    private readonly QueryableMethodTranslatingExpressionVisitorDependencies _dependencies;

    public HexDBQueryableMethodTranslatingExpressionVisitorFactory(QueryableMethodTranslatingExpressionVisitorDependencies dependencies) => _dependencies = dependencies;

    public QueryableMethodTranslatingExpressionVisitor Create(QueryCompilationContext queryCompilationContext) =>
        new HexDBQueryableMethodTranslatingExpressionVisitor(_dependencies, queryCompilationContext, subquery: false);
}

public class HexDBQueryableMethodTranslatingExpressionVisitor : QueryableMethodTranslatingExpressionVisitor
{
    public HexDBQueryableMethodTranslatingExpressionVisitor(QueryableMethodTranslatingExpressionVisitorDependencies dependencies, QueryCompilationContext queryCompilationContext, bool subquery)
        : base(dependencies, queryCompilationContext, subquery) { }

    protected override QueryableMethodTranslatingExpressionVisitor CreateSubqueryVisitor() =>
        new HexDBQueryableMethodTranslatingExpressionVisitor(Dependencies, QueryCompilationContext, subquery: true);

    protected override ShapedQueryExpression CreateShapedQueryExpression(IEntityType entityType)
    {
        var query = new HexDBQueryExpression(entityType);
        return new ShapedQueryExpression(query, new StructuralTypeShaperExpression(entityType, new ProjectionBindingExpression(query, new ProjectionMember(), typeof(ValueBuffer)), nullable: false));
    }

    private static HexDBQueryExpression Query(ShapedQueryExpression source) => (HexDBQueryExpression)source.QueryExpression;

    /// <summary>Still a plain entity query (no projection, not yet a count): filters and sorts can be added.</summary>
    private static bool IsEntityQuery(ShapedQueryExpression source) =>
        source.ShaperExpression is StructuralTypeShaperExpression && Query(source).Kind == HexDBQueryKind.Entities;

    protected override ShapedQueryExpression? TranslateWhere(ShapedQueryExpression source, LambdaExpression predicate)
    {
        var query = Query(source);
        // HexDB filters before paging: a Where after Skip/Take would mean something else.
        if (!IsEntityQuery(source) || query.Skip is not null || query.Take is not null) return null;
        var filter = new FilterTranslator(query.EntityType, predicate.Parameters[0]).Translate(predicate.Body);
        if (filter is null) return null;
        query.Filters.Add(filter);
        return source;
    }

    protected override ShapedQueryExpression? TranslateOrderBy(ShapedQueryExpression source, LambdaExpression keySelector, bool ascending)
    {
        var query = Query(source);
        if (!IsEntityQuery(source) || query.Skip is not null || query.Take is not null) return null;
        var field = FilterTranslator.Field(query.EntityType, keySelector.Parameters[0], keySelector.Body);
        if (field is null) return null;
        query.Orderings.Clear();
        query.Orderings.Add((field.Value.Name, !ascending));
        return source;
    }

    protected override ShapedQueryExpression? TranslateThenBy(ShapedQueryExpression source, LambdaExpression keySelector, bool ascending)
    {
        var query = Query(source);
        if (!IsEntityQuery(source) || query.Orderings.Count == 0 || query.Skip is not null || query.Take is not null) return null;
        var field = FilterTranslator.Field(query.EntityType, keySelector.Parameters[0], keySelector.Body);
        if (field is null) return null;
        query.Orderings.Add((field.Value.Name, !ascending));
        return source;
    }

    protected override ShapedQueryExpression? TranslateSkip(ShapedQueryExpression source, Expression count)
    {
        var query = Query(source);
        if (query.Kind != HexDBQueryKind.Entities || query.Skip is not null || query.Take is not null) return null;
        query.Skip = ValueSource.From(count);
        return source;
    }

    protected override ShapedQueryExpression? TranslateTake(ShapedQueryExpression source, Expression count)
    {
        var query = Query(source);
        if (query.Kind != HexDBQueryKind.Entities || query.Take is not null) return null;
        query.Take = ValueSource.From(count);
        if (count is ConstantExpression { Value: int n }) query.TakeConstant = n;
        return source;
    }

    protected override ShapedQueryExpression? TranslateFirstOrDefault(ShapedQueryExpression source, LambdaExpression? predicate, Type returnType, bool returnDefault) =>
        TranslateSingleResult(source, predicate, returnDefault, rows: 1);

    protected override ShapedQueryExpression? TranslateSingleOrDefault(ShapedQueryExpression source, LambdaExpression? predicate, Type returnType, bool returnDefault) =>
        // Two rows, so EF Core can tell "more than one" apart.
        TranslateSingleResult(source, predicate, returnDefault, rows: 2);

    private ShapedQueryExpression? TranslateSingleResult(ShapedQueryExpression source, LambdaExpression? predicate, bool returnDefault, int rows)
    {
        if (predicate is not null && (source = TranslateWhere(source, predicate)!) is null) return null;
        var query = Query(source);
        if (query.Kind != HexDBQueryKind.Entities) return null;
        if (query.Take is null)
        {
            query.Take = ValueSource.From(Expression.Constant(rows));
            query.TakeConstant = rows;
        }
        else if (query.TakeConstant is { } n)
        {
            var take = Math.Min(n, rows);
            query.Take = ValueSource.From(Expression.Constant(take));
            query.TakeConstant = take;
        }
        else
        {
            return null;
        }
        return source.UpdateResultCardinality(returnDefault ? ResultCardinality.SingleOrDefault : ResultCardinality.Single);
    }

    protected override ShapedQueryExpression? TranslateCount(ShapedQueryExpression source, LambdaExpression? predicate) =>
        TranslateScalar(source, predicate, HexDBQueryKind.Count, typeof(int));

    protected override ShapedQueryExpression? TranslateLongCount(ShapedQueryExpression source, LambdaExpression? predicate) =>
        TranslateScalar(source, predicate, HexDBQueryKind.LongCount, typeof(long));

    protected override ShapedQueryExpression? TranslateAny(ShapedQueryExpression source, LambdaExpression? predicate) =>
        TranslateScalar(source, predicate, HexDBQueryKind.Any, typeof(bool));

    private ShapedQueryExpression? TranslateScalar(ShapedQueryExpression source, LambdaExpression? predicate, HexDBQueryKind kind, Type type)
    {
        if (predicate is not null && (source = TranslateWhere(source, predicate)!) is null) return null;
        var query = Query(source);
        if (query.Kind != HexDBQueryKind.Entities) return null;
        query.Kind = kind;
        return source.UpdateShaperExpression(new ProjectionBindingExpression(query, new ProjectionMember(), type)).UpdateResultCardinality(ResultCardinality.Single);
    }

    protected override ShapedQueryExpression TranslateSelect(ShapedQueryExpression source, LambdaExpression selector)
    {
        if (selector.Body == selector.Parameters[0]) return source;
        // The projection runs on the client over each materialized result.
        var shaper = ReplacingExpressionVisitor.Replace(selector.Parameters[0], source.ShaperExpression, selector.Body);
        return source.UpdateShaperExpression(shaper);
    }

    // Everything else isn't supported (yet): EF Core reports it as untranslatable.
    protected override ShapedQueryExpression? TranslateAll(ShapedQueryExpression source, LambdaExpression predicate) => null;
    protected override ShapedQueryExpression? TranslateAverage(ShapedQueryExpression source, LambdaExpression? selector, Type resultType) => null;
    protected override ShapedQueryExpression? TranslateCast(ShapedQueryExpression source, Type castType) => castType.IsAssignableFrom(source.ShaperExpression.Type) ? source : null;
    protected override ShapedQueryExpression? TranslateConcat(ShapedQueryExpression source1, ShapedQueryExpression source2) => null;
    protected override ShapedQueryExpression? TranslateContains(ShapedQueryExpression source, Expression item) => null;
    protected override ShapedQueryExpression? TranslateDefaultIfEmpty(ShapedQueryExpression source, Expression? defaultValue) => null;
    protected override ShapedQueryExpression? TranslateDistinct(ShapedQueryExpression source) => null;
    protected override ShapedQueryExpression? TranslateElementAtOrDefault(ShapedQueryExpression source, Expression index, bool returnDefault) => null;
    protected override ShapedQueryExpression? TranslateExcept(ShapedQueryExpression source1, ShapedQueryExpression source2) => null;
    protected override ShapedQueryExpression? TranslateGroupBy(ShapedQueryExpression source, LambdaExpression keySelector, LambdaExpression? elementSelector, LambdaExpression? resultSelector) => null;
    protected override ShapedQueryExpression? TranslateGroupJoin(ShapedQueryExpression outer, ShapedQueryExpression inner, LambdaExpression outerKeySelector, LambdaExpression innerKeySelector, LambdaExpression resultSelector) => null;
    protected override ShapedQueryExpression? TranslateIntersect(ShapedQueryExpression source1, ShapedQueryExpression source2) => null;
    protected override ShapedQueryExpression? TranslateJoin(ShapedQueryExpression outer, ShapedQueryExpression inner, LambdaExpression outerKeySelector, LambdaExpression innerKeySelector, LambdaExpression resultSelector) => null;
    protected override ShapedQueryExpression? TranslateLastOrDefault(ShapedQueryExpression source, LambdaExpression? predicate, Type returnType, bool returnDefault) => null;
    protected override ShapedQueryExpression? TranslateLeftJoin(ShapedQueryExpression outer, ShapedQueryExpression inner, LambdaExpression outerKeySelector, LambdaExpression innerKeySelector, LambdaExpression resultSelector) => null;
    protected override ShapedQueryExpression? TranslateMax(ShapedQueryExpression source, LambdaExpression? selector, Type resultType) => null;
    protected override ShapedQueryExpression? TranslateMin(ShapedQueryExpression source, LambdaExpression? selector, Type resultType) => null;
    protected override ShapedQueryExpression? TranslateOfType(ShapedQueryExpression source, Type resultType) => resultType == source.ShaperExpression.Type ? source : null;
    protected override ShapedQueryExpression? TranslateReverse(ShapedQueryExpression source) => null;
    protected override ShapedQueryExpression? TranslateSelectMany(ShapedQueryExpression source, LambdaExpression collectionSelector, LambdaExpression resultSelector) => null;
    protected override ShapedQueryExpression? TranslateSelectMany(ShapedQueryExpression source, LambdaExpression selector) => null;
    protected override ShapedQueryExpression? TranslateSkipWhile(ShapedQueryExpression source, LambdaExpression predicate) => null;
    protected override ShapedQueryExpression? TranslateSum(ShapedQueryExpression source, LambdaExpression? selector, Type resultType) => null;
    protected override ShapedQueryExpression? TranslateTakeWhile(ShapedQueryExpression source, LambdaExpression predicate) => null;
    protected override ShapedQueryExpression? TranslateUnion(ShapedQueryExpression source1, ShapedQueryExpression source2) => null;
}

/// <summary>A predicate (over one entity parameter) as a HexDB filter, or null if it can't be one.</summary>
internal sealed class FilterTranslator
{
    private readonly IEntityType _entityType;
    private readonly ParameterExpression _entity;

    public FilterTranslator(IEntityType entityType, ParameterExpression entity)
    {
        _entityType = entityType;
        _entity = entity;
    }

    public FilterNode? Translate(Expression expression)
    {
        expression = StripConvert(expression);
        switch (expression)
        {
            case BinaryExpression { NodeType: ExpressionType.AndAlso or ExpressionType.OrElse } logical:
            {
                var left = Translate(logical.Left);
                var right = Translate(logical.Right);
                if (left is null || right is null) return null;
                return new FilterGroup(logical.NodeType == ExpressionType.AndAlso ? "$and" : "$or", new[] { left, right });
            }
            case UnaryExpression { NodeType: ExpressionType.Not } not:
            {
                // !x.Flag is x.Flag == false; everything else is $not.
                if (Field(_entityType, _entity, not.Operand) is { } flag && flag.Property?.ClrType is { } t && (Nullable.GetUnderlyingType(t) ?? t) == typeof(bool))
                    return new FieldCondition(flag.Name, "$eq", ValueSource.From(Expression.Constant(false)), flag.Property);
                var inner = Translate(not.Operand);
                return inner is null ? null : new FilterNot(inner);
            }
            case BinaryExpression binary when Comparison(binary.NodeType) is not null:
                return TranslateComparison(binary.NodeType, binary.Left, binary.Right);
            case MethodCallExpression call:
                return TranslateCall(call);
            default:
                // A bool property on its own: x.IsActive.
                if (expression.Type == typeof(bool) && Field(_entityType, _entity, expression) is { } member)
                    return new FieldCondition(member.Name, "$eq", ValueSource.From(Expression.Constant(true)), member.Property);
                return null;
        }
    }

    private FilterNode? TranslateComparison(ExpressionType type, Expression left, Expression right)
    {
        if (Field(_entityType, _entity, left) is { } field && IsValue(right)) return Condition(field, type, right);
        if (Field(_entityType, _entity, right) is { } reversed && IsValue(left)) return Condition(reversed, Flip(type), left);
        return null;
    }

    private static FilterNode Condition((string Name, IReadOnlyProperty? Property) field, ExpressionType type, Expression value)
    {
        value = StripConvert(value);
        if (value is ConstantExpression { Value: null })
        {
            return type == ExpressionType.NotEqual
                ? new FieldCondition(field.Name, "$ne", ValueSource.From(Expression.Constant(null)), field.Property)
                : new FieldCondition(field.Name, "$eq", ValueSource.From(Expression.Constant(null)), field.Property);
        }
        return new FieldCondition(field.Name, Comparison(type)!, ValueSource.From(value), field.Property);
    }

    private FilterNode? TranslateCall(MethodCallExpression call)
    {
        var method = call.Method;
        // string.Contains / StartsWith / EndsWith(value), ordinal (HexDB compares case-sensitively).
        if (method.DeclaringType == typeof(string) && call.Object is not null && call.Arguments.Count == 1 && call.Arguments[0].Type == typeof(string)
            && Field(_entityType, _entity, call.Object) is { } text && IsValue(call.Arguments[0]))
        {
            var op = method.Name switch { "Contains" => "$contains", "StartsWith" => "$startsWith", "EndsWith" => "$endsWith", "Equals" => "$eq", _ => null };
            return op is null ? null : new FieldCondition(text.Name, op, ValueSource.From(call.Arguments[0]), text.Property);
        }
        // string.IsNullOrEmpty(x.P)
        if (method.DeclaringType == typeof(string) && method.Name == nameof(string.IsNullOrEmpty) && Field(_entityType, _entity, call.Arguments[0]) is { } maybeEmpty)
        {
            return new FilterGroup("$or", new FilterNode[]
            {
                new FieldCondition(maybeEmpty.Name, "$eq", ValueSource.From(Expression.Constant(null)), maybeEmpty.Property),
                new FieldCondition(maybeEmpty.Name, "$eq", ValueSource.From(Expression.Constant("")), maybeEmpty.Property),
            });
        }
        // a.Equals(b) and object.Equals(a, b)
        if (method.Name == nameof(object.Equals) && call.Object is not null && call.Arguments.Count == 1) return TranslateComparison(ExpressionType.Equal, call.Object, call.Arguments[0]);
        if (method.Name == nameof(object.Equals) && call.Object is null && call.Arguments.Count == 2) return TranslateComparison(ExpressionType.Equal, call.Arguments[0], call.Arguments[1]);
        // Contains: list.Contains(x.P) is $in; x.Collection.Contains(v) is $contains.
        if (method.Name == nameof(Enumerable.Contains))
        {
            var (collection, item) = call.Object is null && call.Arguments.Count == 2 ? (call.Arguments[0], call.Arguments[1])
                : call.Object is not null && call.Arguments.Count == 1 ? (call.Object, call.Arguments[0])
                : (null, null);
            if (collection is null || item is null) return null;
            if (Field(_entityType, _entity, item) is { } inField && IsValue(collection)) return new FieldCondition(inField.Name, "$in", ValueSource.From(collection), inField.Property);
            if (Field(_entityType, _entity, collection) is { } arrayField && IsValue(item)) return new FieldCondition(arrayField.Name, "$contains", ValueSource.From(item), arrayField.Property);
        }
        return null;
    }

    /// <summary>The document field an expression reads (x.P or EF.Property(x, "P")), if it's one.</summary>
    public static (string Name, IReadOnlyProperty? Property)? Field(IEntityType entityType, ParameterExpression entity, Expression expression)
    {
        expression = StripConvert(expression);
        string? name = expression switch
        {
            MemberExpression member when StripConvert(member.Expression!) == entity => member.Member.Name,
            MethodCallExpression call when call.Method.IsGenericMethod && call.Method.GetGenericMethodDefinition() == EFPropertyMethod
                && StripConvert(call.Arguments[0]) == entity && call.Arguments[1] is ConstantExpression { Value: string n } => n,
            _ => null,
        };
        if (name is null) return null;
        var property = entityType.FindProperty(name);
        return property is null ? null : (property.GetJsonPropertyName(), property);
    }

    private static readonly MethodInfo EFPropertyMethod = typeof(EF).GetMethod(nameof(EF.Property))!;

    /// <summary>True if an expression doesn't read the entity (a constant, parameter, or expression over them).</summary>
    private bool IsValue(Expression expression) => !new EntityFinder(_entity).Finds(expression);

    private static Expression StripConvert(Expression expression)
    {
        while (expression is UnaryExpression { NodeType: ExpressionType.Convert or ExpressionType.ConvertChecked } convert) expression = convert.Operand;
        return expression;
    }

    private static string? Comparison(ExpressionType type) => type switch
    {
        ExpressionType.Equal => "$eq",
        ExpressionType.NotEqual => "$ne",
        ExpressionType.LessThan => "$lt",
        ExpressionType.LessThanOrEqual => "$lte",
        ExpressionType.GreaterThan => "$gt",
        ExpressionType.GreaterThanOrEqual => "$gte",
        _ => null,
    };

    private static ExpressionType Flip(ExpressionType type) => type switch
    {
        ExpressionType.LessThan => ExpressionType.GreaterThan,
        ExpressionType.LessThanOrEqual => ExpressionType.GreaterThanOrEqual,
        ExpressionType.GreaterThan => ExpressionType.LessThan,
        ExpressionType.GreaterThanOrEqual => ExpressionType.LessThanOrEqual,
        _ => type,
    };

    private sealed class EntityFinder : ExpressionVisitor
    {
        private readonly ParameterExpression _entity;
        private bool _found;

        public EntityFinder(ParameterExpression entity) => _entity = entity;

        public bool Finds(Expression expression)
        {
            _found = false;
            Visit(expression);
            return _found;
        }

        protected override Expression VisitParameter(ParameterExpression node)
        {
            _found |= node == _entity;
            return node;
        }

        protected override Expression VisitExtension(Expression node)
        {
            // EF Core's own nodes (shapers, query roots) only appear when the entity is read.
            _found = true;
            return node;
        }
    }
}
