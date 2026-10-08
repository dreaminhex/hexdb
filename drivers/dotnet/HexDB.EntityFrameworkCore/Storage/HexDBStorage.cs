// Storage plumbing: the connection, ULIDs, type mappings, JSON conversion,
// value generation, and the (unsupported) explicit transactions.

using System.Collections;
using System.Security.Cryptography;
using System.Text.Json;
using System.Text.Json.Nodes;
using HexDB.Client;
using HexDB.EntityFrameworkCore.Infrastructure;
using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.ChangeTracking;
using Microsoft.EntityFrameworkCore.Diagnostics;
using Microsoft.EntityFrameworkCore.Infrastructure;
using Microsoft.EntityFrameworkCore.Metadata;
using Microsoft.EntityFrameworkCore.Storage;
using Microsoft.EntityFrameworkCore.Storage.Json;
using Microsoft.EntityFrameworkCore.Storage.ValueConversion;
using Microsoft.EntityFrameworkCore.ValueGeneration;

namespace HexDB.EntityFrameworkCore.Storage;

/// <summary>The HexDB client a context uses (one per context, from its options).</summary>
public sealed class HexDBConnection : IDisposable
{
    public HexDBConnection(IDbContextOptions options)
    {
        var extension = options.FindExtension<HexDBOptionsExtension>() ?? throw new InvalidOperationException("The context isn't configured with UseHexDB.");
        Options = extension;
        Client = new HexDBClient(extension.Url!, extension.ApiKey, extension.HttpClient);
    }

    public HexDBClient Client { get; }

    public HexDBOptionsExtension Options { get; }

    public void Dispose() => Client.Dispose();
}

/// <summary>ULIDs: HexDB's document ids (48-bit time in ms, 80 random bits, Crockford base32).</summary>
public static class Ulid
{
    private const string Alphabet = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

    /// <summary>A new ULID string.</summary>
    public static string New()
    {
        Span<byte> bytes = stackalloc byte[16];
        var ms = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        for (var i = 5; i >= 0; i--)
        {
            bytes[i] = (byte)(ms & 0xFF);
            ms >>= 8;
        }
        RandomNumberGenerator.Fill(bytes[6..]);
        return Encode(bytes);
    }

    /// <summary>The ULID text of 16 bytes (big-endian).</summary>
    public static string Encode(ReadOnlySpan<byte> bytes)
    {
        var value = new System.Numerics.BigInteger(bytes, isUnsigned: true, isBigEndian: true);
        Span<char> chars = stackalloc char[26];
        for (var i = 25; i >= 0; i--)
        {
            chars[i] = Alphabet[(int)(value & 31)];
            value >>= 5;
        }
        return new string(chars);
    }

    /// <summary>The 16 bytes (big-endian) of a ULID's text.</summary>
    public static byte[] Decode(string text)
    {
        if (text.Length != 26) throw new FormatException($"'{text}' isn't a ULID.");
        System.Numerics.BigInteger value = 0;
        foreach (var c in text.ToUpperInvariant())
        {
            var digit = Alphabet.IndexOf(c);
            if (digit < 0) throw new FormatException($"'{text}' isn't a ULID.");
            value = (value << 5) | digit;
        }
        var bytes = value.ToByteArray(isUnsigned: true, isBigEndian: true);
        if (bytes.Length > 16) throw new FormatException($"'{text}' is out of range for a ULID.");
        var result = new byte[16];
        bytes.CopyTo(result, 16 - bytes.Length);
        return result;
    }
}

/// <summary>Guid keys are stored as the ULID with the same 128 bits.</summary>
public sealed class GuidUlidConverter : ValueConverter<Guid, string>
{
    public static readonly GuidUlidConverter Instance = new();

    public GuidUlidConverter() : base(g => Ulid.Encode(g.ToByteArray(true)), s => new Guid(Ulid.Decode(s), true)) { }
}

/// <summary>Generates ULIDs for string keys.</summary>
public sealed class UlidValueGenerator : ValueGenerator<string>
{
    public override bool GeneratesTemporaryValues => false;

    public override string Next(EntityEntry entry) => Ulid.New();
}

/// <summary>ULIDs for string keys; EF Core's generators for everything else.</summary>
public class HexDBValueGeneratorSelector : ValueGeneratorSelector
{
    private static readonly UlidValueGenerator Ulids = new();

    public HexDBValueGeneratorSelector(ValueGeneratorSelectorDependencies dependencies) : base(dependencies) { }

    public override ValueGenerator Create(IProperty property, ITypeBase typeBase) =>
        property.ClrType == typeof(string) && property.IsPrimaryKey() ? Ulids : base.Create(property, typeBase);
}

/// <summary>A type HexDB stores as JSON.</summary>
public class HexDBTypeMapping : CoreTypeMapping
{
    public HexDBTypeMapping(Type clrType, ValueComparer? comparer = null, JsonValueReaderWriter? jsonValueReaderWriter = null)
        : base(new CoreTypeMappingParameters(clrType, null, comparer, comparer, null, null, null, jsonValueReaderWriter)) { }

    private HexDBTypeMapping(CoreTypeMappingParameters parameters) : base(parameters) { }

    public override CoreTypeMapping WithComposedConverter(ValueConverter? converter, ValueComparer? comparer = null, ValueComparer? keyComparer = null, CoreTypeMapping? elementMapping = null, JsonValueReaderWriter? jsonValueReaderWriter = null) =>
        new HexDBTypeMapping(Parameters.WithComposedConverter(converter, comparer, keyComparer, elementMapping, jsonValueReaderWriter));

    protected override CoreTypeMapping Clone(CoreTypeMappingParameters parameters) => new HexDBTypeMapping(parameters);
}

/// <summary>
/// The CLR types HexDB stores: scalars (numbers, strings, booleans, dates and
/// times, Guids, enums, byte arrays), collections and dictionaries of them, and
/// JSON values (JsonNode, JsonElement). Other classes are left unmapped (EF Core
/// would take them for related entities, which the provider doesn't support yet).
/// </summary>
public class HexDBTypeMappingSource : TypeMappingSource
{
    public HexDBTypeMappingSource(TypeMappingSourceDependencies dependencies) : base(dependencies) { }

    protected override CoreTypeMapping? FindMapping(in TypeMappingInfo mappingInfo)
    {
        var clrType = mappingInfo.ClrType;
        if (clrType is null) return null;
        if (IsScalar(clrType)) return new HexDBTypeMapping(clrType, null, Dependencies.JsonValueReaderWriterSource.FindReaderWriter(clrType));
        if (IsJson(clrType) || IsCollectionOfScalars(clrType))
        {
            var comparer = (ValueComparer)Activator.CreateInstance(typeof(JsonValueComparer<>).MakeGenericType(clrType))!;
            return new HexDBTypeMapping(clrType, comparer);
        }
        return null;
    }

    internal static bool IsScalar(Type type)
    {
        type = Nullable.GetUnderlyingType(type) ?? type;
        return type.IsPrimitive || type.IsEnum || type == typeof(string) || type == typeof(decimal) || type == typeof(DateTime) || type == typeof(DateTimeOffset)
            || type == typeof(DateOnly) || type == typeof(TimeOnly) || type == typeof(TimeSpan) || type == typeof(Guid) || type == typeof(byte[]);
    }

    private static bool IsJson(Type type) => typeof(JsonNode).IsAssignableFrom(type) || type == typeof(JsonElement) || type == typeof(JsonElement?);

    private static bool IsCollectionOfScalars(Type type)
    {
        if (type == typeof(string) || !typeof(IEnumerable).IsAssignableFrom(type)) return false;
        if (type.IsArray) return IsScalar(type.GetElementType()!);
        var dictionary = type.GetInterfaces().Append(type).FirstOrDefault(i => i.IsGenericType && i.GetGenericTypeDefinition() == typeof(IDictionary<,>));
        if (dictionary is not null) return dictionary.GetGenericArguments()[0] == typeof(string) && (IsScalar(dictionary.GetGenericArguments()[1]) || dictionary.GetGenericArguments()[1] == typeof(object));
        var enumerable = type.GetInterfaces().Append(type).FirstOrDefault(i => i.IsGenericType && i.GetGenericTypeDefinition() == typeof(IEnumerable<>));
        return enumerable is not null && IsScalar(enumerable.GetGenericArguments()[0]);
    }
}

/// <summary>Compares (and snapshots) values by their JSON, for collections and JSON values.</summary>
public sealed class JsonValueComparer<T> : ValueComparer<T>
{
    public JsonValueComparer() : base((a, b) => JsonValues.Text(a) == JsonValues.Text(b), v => JsonValues.Text(v).GetHashCode(), v => JsonValues.Clone(v)) { }
}

/// <summary>Converting between CLR values and HexDB's JSON.</summary>
public static class JsonValues
{
    /// <summary>The serializer settings used for every value.</summary>
    public static readonly JsonSerializerOptions Options = new(JsonSerializerDefaults.General);

    internal static string Text<T>(T value) => JsonSerializer.Serialize(value, Options);

    internal static T Clone<T>(T value) => value is null ? value : JsonSerializer.Deserialize<T>(JsonSerializer.Serialize(value, Options), Options)!;

    /// <summary>A property's value as stored (through its value converter, if any).</summary>
    public static JsonNode? ToJson(object? value, IReadOnlyProperty property)
    {
        var converter = property.GetTypeMapping().Converter;
        if (converter is not null && value is not null) value = converter.ConvertToProvider(value);
        return value is null ? null : JsonSerializer.SerializeToNode(value, value.GetType(), Options);
    }

    /// <summary>A stored value as the property's CLR value (through its value converter, if any).</summary>
    public static object? FromJson(JsonNode? node, IReadOnlyProperty property)
    {
        if (node is null) return null;
        var converter = property.GetTypeMapping().Converter;
        var type = converter?.ProviderClrType ?? property.ClrType;
        try
        {
            var value = node.Deserialize(type, Options);
            return converter is null || value is null ? value : converter.ConvertFromProvider(value);
        }
        catch (JsonException e)
        {
            throw new InvalidOperationException($"The stored value of '{property.DeclaringType.DisplayName()}.{property.Name}' ({node.ToJsonString()}) can't be read as {type.Name}: {e.Message}", e);
        }
    }
}

/// <summary>HexDB has no client-controlled transactions: SaveChanges is already atomic.</summary>
public class HexDBTransactionManager : IDbContextTransactionManager
{
    private static readonly IDbContextTransaction Stub = new StubTransaction();

    public IDbContextTransaction? CurrentTransaction => null;

    public IDbContextTransaction BeginTransaction() => throw NotSupported();

    public Task<IDbContextTransaction> BeginTransactionAsync(CancellationToken cancellationToken = default) => throw NotSupported();

    public void CommitTransaction() { }

    public Task CommitTransactionAsync(CancellationToken cancellationToken = default) => Task.CompletedTask;

    public void RollbackTransaction() { }

    public Task RollbackTransactionAsync(CancellationToken cancellationToken = default) => Task.CompletedTask;

    public void ResetState() { }

    public Task ResetStateAsync(CancellationToken cancellationToken = default) => Task.CompletedTask;

    private static NotSupportedException NotSupported() =>
        new("HexDB doesn't support explicit transactions. Each SaveChanges is one atomic HexDB transaction; group the changes that belong together into one SaveChanges.");

    private sealed class StubTransaction : IDbContextTransaction
    {
        public Guid TransactionId { get; } = Guid.NewGuid();
        public void Commit() { }
        public Task CommitAsync(CancellationToken cancellationToken = default) => Task.CompletedTask;
        public void Rollback() { }
        public Task RollbackAsync(CancellationToken cancellationToken = default) => Task.CompletedTask;
        public void Dispose() { }
        public ValueTask DisposeAsync() => ValueTask.CompletedTask;
    }

    internal static IDbContextTransaction Transaction => Stub;
}

/// <summary>Logging definitions (the provider logs through EF Core's core events).</summary>
public class HexDBLoggingDefinitions : LoggingDefinitions { }
