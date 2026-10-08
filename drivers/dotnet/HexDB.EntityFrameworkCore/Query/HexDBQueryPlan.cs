// A translated query: what POST /{tessellation}/_query will be asked, built at
// compile time and filled in with parameter values at each execution.

using System.Linq.Expressions;
using System.Runtime.CompilerServices;
using System.Text.Json.Nodes;
using HexDB.EntityFrameworkCore.Storage;
using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.Metadata;
using Microsoft.EntityFrameworkCore.Query;
using Microsoft.EntityFrameworkCore.Storage;

namespace HexDB.EntityFrameworkCore.Query;

/// <summary>A value known at execution time: a constant, a query parameter, or an expression over them.</summary>
public sealed class ValueSource
{
    private readonly Func<QueryContext, object?> _evaluate;

    private ValueSource(Func<QueryContext, object?> evaluate, string description)
    {
        _evaluate = evaluate;
        Description = description;
    }

    public string Description { get; }

    public object? Evaluate(QueryContext context) => _evaluate(context);

    /// <summary>
    /// Compile an expression that doesn't depend on the entity: query parameters
    /// (captured variables) are read from the query context at each execution.
    /// </summary>
    public static ValueSource From(Expression expression)
    {
        if (expression is ConstantExpression constant) return new ValueSource(_ => constant.Value, constant.Value?.ToString() ?? "null");
        var body = new ParameterReplacer().Visit(expression);
        var lambda = Expression.Lambda<Func<QueryContext, object?>>(Expression.Convert(body, typeof(object)), QueryCompilationContext.QueryContextParameter);
        return new ValueSource(lambda.Compile(), expression is ParameterExpression p ? "@" + p.Name : expression.ToString());
    }

    private sealed class ParameterReplacer : ExpressionVisitor
    {
        private static readonly System.Reflection.PropertyInfo ParameterValues = typeof(QueryContext).GetProperty(nameof(QueryContext.ParameterValues))!;

        protected override Expression VisitParameter(ParameterExpression node) =>
            node == QueryCompilationContext.QueryContextParameter
                ? node
                : Expression.Convert(
                    Expression.Property(Expression.Property(QueryCompilationContext.QueryContextParameter, ParameterValues), "Item", Expression.Constant(node.Name)),
                    node.Type);
    }
}

/// <summary>One condition of a filter, rendered to HexDB's filter JSON at execution.</summary>
public abstract class FilterNode
{
    public abstract JsonObject Render(QueryContext context);

    public abstract string Describe();
}

/// <summary><c>{"field": {"$op": value}}</c> (or <c>{"field": value}</c> for equality).</summary>
public sealed class FieldCondition : FilterNode
{
    public FieldCondition(string field, string op, ValueSource value, IReadOnlyProperty? property)
    {
        Field = field;
        Op = op;
        Value = value;
        Property = property;
    }

    public string Field { get; }
    public string Op { get; }
    public ValueSource Value { get; }
    public IReadOnlyProperty? Property { get; }

    public override JsonObject Render(QueryContext context)
    {
        var value = Value.Evaluate(context);
        JsonNode? json;
        if (Op is "$in" or "$nin")
        {
            var items = new JsonArray();
            if (value is System.Collections.IEnumerable list and not string)
            {
                foreach (var item in list) items.Add(Convert(item));
            }
            json = items;
        }
        else if (Op is "$contains" && Property is not null && Property.ClrType != typeof(string))
        {
            // An element of a collection property.
            json = Element(value);
        }
        else
        {
            json = Convert(value);
        }
        return Op == "$eq" ? new JsonObject { [Field] = json } : new JsonObject { [Field] = new JsonObject { [Op] = json } };

        static JsonNode? Element(object? v) => v is null ? null : System.Text.Json.JsonSerializer.SerializeToNode(v, v.GetType(), JsonValues.Options);
    }

    private JsonNode? Convert(object? value)
    {
        if (value is null) return null;
        if (Property is null || Op is "$contains" or "$startsWith" or "$endsWith") return System.Text.Json.JsonSerializer.SerializeToNode(value, value.GetType(), JsonValues.Options);
        // Enums compared through their underlying number: back to the enum, then the property's converter.
        var clr = Nullable.GetUnderlyingType(Property.ClrType) ?? Property.ClrType;
        if (clr.IsEnum && !value.GetType().IsEnum) value = Enum.ToObject(clr, value);
        return JsonValues.ToJson(value, Property);
    }

    public override string Describe() => Op == "$eq" ? $"{Field} = {Value.Description}" : $"{Field} {Op} {Value.Description}";
}

/// <summary><c>$and</c> / <c>$or</c> of conditions.</summary>
public sealed class FilterGroup : FilterNode
{
    public FilterGroup(string op, IReadOnlyList<FilterNode> nodes)
    {
        Op = op;
        Nodes = nodes;
    }

    public string Op { get; }
    public IReadOnlyList<FilterNode> Nodes { get; }

    public override JsonObject Render(QueryContext context) => new() { [Op] = new JsonArray(Nodes.Select(n => (JsonNode)n.Render(context)).ToArray()) };

    public override string Describe() => "(" + string.Join(Op == "$and" ? " AND " : " OR ", Nodes.Select(n => n.Describe())) + ")";
}

/// <summary><c>$not</c> of a condition.</summary>
public sealed class FilterNot : FilterNode
{
    public FilterNot(FilterNode inner) => Inner = inner;

    public FilterNode Inner { get; }

    public override JsonObject Render(QueryContext context) => new() { ["$not"] = Inner.Render(context) };

    public override string Describe() => "NOT " + Inner.Describe();
}

/// <summary>What a query returns.</summary>
public enum HexDBQueryKind
{
    Entities,
    Count,
    LongCount,
    Any,
}

/// <summary>
/// The query HexDB answers, as an expression node EF Core carries through its
/// pipeline. Translation fills it in; <see cref="Execute"/> runs it.
/// </summary>
public sealed class HexDBQueryExpression : Expression, IPrintableExpression
{
    public HexDBQueryExpression(IEntityType entityType)
    {
        EntityType = entityType;
        Tessellation = entityType.GetTessellation();
    }

    public IEntityType EntityType { get; }
    public string Tessellation { get; }
    public List<FilterNode> Filters { get; } = new();
    public List<(string Field, bool Descending)> Orderings { get; } = new();
    public ValueSource? Skip { get; set; }
    public ValueSource? Take { get; set; }
    /// <summary>Set by Take(constant) / First / Single, to combine with an earlier Take.</summary>
    public int? TakeConstant { get; set; }
    public HexDBQueryKind Kind { get; set; } = HexDBQueryKind.Entities;

    public override Type Type => typeof(object);

    public override ExpressionType NodeType => ExpressionType.Extension;

    protected override Expression VisitChildren(ExpressionVisitor visitor) => this;

    public void Print(ExpressionPrinter expressionPrinter) => expressionPrinter.Append(Describe());

    /// <summary>A readable form, for logs and ToQueryString.</summary>
    public string Describe()
    {
        var text = $"HexDB {Kind} from '{Tessellation}'";
        if (Filters.Count > 0) text += " where " + string.Join(" AND ", Filters.Select(f => f.Describe()));
        if (Orderings.Count > 0) text += " order by " + string.Join(", ", Orderings.Select(o => o.Descending ? o.Field + " desc" : o.Field));
        if (Skip is not null) text += " skip " + Skip.Description;
        if (Take is not null) text += " take " + Take.Description;
        return text;
    }

    private JsonObject Filter(QueryContext context) => Filters.Count switch
    {
        0 => new JsonObject(),
        1 => Filters[0].Render(context),
        _ => new FilterGroup("$and", Filters).Render(context),
    };

    private string? Sort => Orderings.Count == 0 ? null : string.Join(",", Orderings.Select(o => (o.Descending ? "-" : "") + o.Field));

    private static int? Count(ValueSource? source, QueryContext context) => source?.Evaluate(context) switch
    {
        null => null,
        int i => i,
        long l => (int)l,
        var other => System.Convert.ToInt32(other),
    };

    /// <summary>The rows: documents as value buffers (by property index), or one scalar.</summary>
    public IEnumerable<ValueBuffer> Execute(QueryContext context) => ExecuteAsync(context, CancellationToken.None).ToBlockingEnumerable();

    public async IAsyncEnumerable<ValueBuffer> ExecuteAsync(QueryContext context, [EnumeratorCancellation] CancellationToken cancel)
    {
        var client = ((HexDBQueryContext)context).Connection.Client;
        var pageSize = ((HexDBQueryContext)context).Connection.Options.PageSize;
        var path = $"/{Uri.EscapeDataString(Tessellation)}/_query";
        var filter = Filter(context);
        var skip = Math.Max(0, Count(Skip, context) ?? 0);
        var take = Count(Take, context);
        if (take is < 0) take = 0;

        switch (Kind)
        {
            case HexDBQueryKind.Count or HexDBQueryKind.LongCount:
            {
                var body = new JsonObject { ["filter"] = filter, ["limit"] = 0 };
                var answer = await client.RequestAsync(HttpMethod.Post, path, body, cancel: cancel).ConfigureAwait(false);
                var total = answer?["total"]?.GetValue<long>() ?? 0;
                var count = Math.Max(0, total - skip);
                if (take is not null) count = Math.Min(count, take.Value);
                yield return new ValueBuffer(new object[] { Kind == HexDBQueryKind.Count ? (object)(int)count : count });
                yield break;
            }
            case HexDBQueryKind.Any:
            {
                var any = false;
                if (take is not 0)
                {
                    var body = new JsonObject { ["filter"] = filter, ["limit"] = 1, ["offset"] = skip, ["total"] = false };
                    var answer = await client.RequestAsync(HttpMethod.Post, path, body, cancel: cancel).ConfigureAwait(false);
                    any = answer?["documents"] is JsonArray { Count: > 0 };
                }
                yield return new ValueBuffer(new object[] { any });
                yield break;
            }
        }

        // Documents, a page at a time: by offset when sorted, by cursor (ID order) otherwise.
        var sort = Sort;
        var remaining = take ?? int.MaxValue;
        var offset = skip;
        string? after = null;
        while (remaining > 0)
        {
            var limit = Math.Min(pageSize, remaining);
            var body = new JsonObject { ["filter"] = filter.DeepClone(), ["limit"] = limit, ["total"] = false };
            if (sort is not null) body["sort"] = sort;
            if (after is not null) body["after"] = after;
            else if (offset > 0) body["offset"] = offset;
            var answer = await client.RequestAsync(HttpMethod.Post, path, body, cancel: cancel).ConfigureAwait(false);
            var documents = answer?["documents"] as JsonArray ?? new JsonArray();
            foreach (var document in documents)
            {
                if (document is JsonObject json) yield return ToValueBuffer(json);
            }
            remaining -= documents.Count;
            if (documents.Count < limit) yield break;
            if (sort is null)
            {
                after = answer?["next"]?.GetValue<string>();
                if (after is null) yield break;
            }
            else
            {
                offset += documents.Count;
            }
        }
    }

    private int? _bufferLength;

    /// <summary>A document's fields, by property index, as EF Core's materializer reads them.</summary>
    public ValueBuffer ToValueBuffer(JsonObject json)
    {
        var properties = EntityType.GetProperties();
        var values = new object?[_bufferLength ??= properties.Max(p => p.GetIndex()) + 1];
        foreach (var property in properties)
        {
            values[property.GetIndex()] = JsonValues.FromJson(json[property.GetJsonPropertyName()], property);
        }
        return new ValueBuffer(values);
    }
}
