// The options UseHexDB records on a DbContext, and the services the provider
// adds to EF Core's internal service provider.

using HexDB.EntityFrameworkCore.Metadata;
using HexDB.EntityFrameworkCore.Query;
using HexDB.EntityFrameworkCore.Storage;
using Microsoft.EntityFrameworkCore;
using Microsoft.EntityFrameworkCore.Diagnostics;
using Microsoft.EntityFrameworkCore.Infrastructure;
using Microsoft.EntityFrameworkCore.Metadata.Conventions.Infrastructure;
using Microsoft.EntityFrameworkCore.Query;
using Microsoft.EntityFrameworkCore.Storage;
using Microsoft.EntityFrameworkCore.ValueGeneration;
using Microsoft.Extensions.DependencyInjection;

namespace HexDB.EntityFrameworkCore.Infrastructure;

/// <summary>Options for the HexDB provider (see <c>UseHexDB</c>).</summary>
public class HexDBOptionsExtension : IDbContextOptionsExtension
{
    private DbContextOptionsExtensionInfo? _info;

    public HexDBOptionsExtension() { }

    protected HexDBOptionsExtension(HexDBOptionsExtension copyFrom)
    {
        Url = copyFrom.Url;
        ApiKey = copyFrom.ApiKey;
        HttpClient = copyFrom.HttpClient;
        PageSize = copyFrom.PageSize;
    }

    /// <summary>Base URL of any hex (replicas forward writes to the Overseer).</summary>
    public virtual Uri? Url { get; private set; }

    /// <summary>An API key (hxk_...) or session token.</summary>
    public virtual string? ApiKey { get; private set; }

    /// <summary>An HttpClient to use (custom certificates or handlers).</summary>
    public virtual HttpClient? HttpClient { get; private set; }

    /// <summary>Documents fetched per request when a query reads many (at most 1000).</summary>
    public virtual int PageSize { get; private set; } = 500;

    public virtual DbContextOptionsExtensionInfo Info => _info ??= new ExtensionInfo(this);

    protected virtual HexDBOptionsExtension Clone() => new(this);

    public virtual HexDBOptionsExtension WithConnection(Uri url, string? apiKey)
    {
        var clone = Clone();
        clone.Url = url;
        clone.ApiKey = apiKey;
        return clone;
    }

    public virtual HexDBOptionsExtension WithHttpClient(HttpClient? httpClient)
    {
        var clone = Clone();
        clone.HttpClient = httpClient;
        return clone;
    }

    public virtual HexDBOptionsExtension WithPageSize(int pageSize)
    {
        if (pageSize is < 1 or > 1000) throw new ArgumentOutOfRangeException(nameof(pageSize), "The page size must be 1-1000.");
        var clone = Clone();
        clone.PageSize = pageSize;
        return clone;
    }

    public virtual void ApplyServices(IServiceCollection services) => services.AddEntityFrameworkHexDB();

    public virtual void Validate(IDbContextOptions options)
    {
        if (Url is null) throw new InvalidOperationException("UseHexDB needs the URL of a HexDB server.");
    }

    private sealed class ExtensionInfo : DbContextOptionsExtensionInfo
    {
        public ExtensionInfo(IDbContextOptionsExtension extension) : base(extension) { }

        private new HexDBOptionsExtension Extension => (HexDBOptionsExtension)base.Extension;

        public override bool IsDatabaseProvider => true;

        public override string LogFragment => $"Url={Extension.Url} ";

        // Connection details are per-context state (the client is scoped), so
        // every HexDB context can share one internal service provider.
        public override int GetServiceProviderHashCode() => 0;

        public override bool ShouldUseSameServiceProvider(DbContextOptionsExtensionInfo other) => other is ExtensionInfo;

        public override void PopulateDebugInfo(IDictionary<string, string> debugInfo) => debugInfo["HexDB:Url"] = Extension.Url?.ToString() ?? "";
    }
}

/// <summary>Registers the HexDB provider's services.</summary>
public static class HexDBServiceCollectionExtensions
{
    /// <summary>
    /// Adds the services the HexDB provider needs. <c>UseHexDB</c> does this; call it
    /// yourself only when building EF Core's internal service provider.
    /// </summary>
    public static IServiceCollection AddEntityFrameworkHexDB(this IServiceCollection services)
    {
        new EntityFrameworkServicesBuilder(services)
            .TryAdd<LoggingDefinitions, HexDBLoggingDefinitions>()
            .TryAdd<IDatabaseProvider, DatabaseProvider<HexDBOptionsExtension>>()
            .TryAdd<IDatabase, HexDBDatabase>()
            .TryAdd<IDbContextTransactionManager, HexDBTransactionManager>()
            .TryAdd<IDatabaseCreator, HexDBDatabaseCreator>()
            .TryAdd<IQueryContextFactory, HexDBQueryContextFactory>()
            .TryAdd<IProviderConventionSetBuilder, HexDBConventionSetBuilder>()
            .TryAdd<IModelValidator, HexDBModelValidator>()
            .TryAdd<ITypeMappingSource, HexDBTypeMappingSource>()
            .TryAdd<IValueGeneratorSelector, HexDBValueGeneratorSelector>()
            .TryAdd<IShapedQueryCompilingExpressionVisitorFactory, HexDBShapedQueryCompilingExpressionVisitorFactory>()
            .TryAdd<IQueryableMethodTranslatingExpressionVisitorFactory, HexDBQueryableMethodTranslatingExpressionVisitorFactory>()
            .TryAddProviderSpecificServices(b => b.TryAddScoped<HexDBConnection, HexDBConnection>())
            .TryAddCoreServices();
        return services;
    }
}
