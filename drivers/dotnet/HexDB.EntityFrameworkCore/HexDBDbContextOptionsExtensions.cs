using HexDB.EntityFrameworkCore.Infrastructure;
using Microsoft.EntityFrameworkCore.Infrastructure;

// In the EF Core namespace, like every provider's UseXxx, so it's found without a using.
namespace Microsoft.EntityFrameworkCore;

/// <summary>Configures a DbContext to use HexDB.</summary>
public static class HexDBDbContextOptionsExtensions
{
    /// <summary>
    /// Use HexDB. <paramref name="url"/> is any hex of the lattice; <paramref name="apiKey"/> an API key
    /// (hxk_...) or session token whose user's roles decide what the context may read and write.
    /// </summary>
    public static DbContextOptionsBuilder UseHexDB(this DbContextOptionsBuilder optionsBuilder, string url, string? apiKey, Action<HexDBDbContextOptionsBuilder>? options = null)
    {
        var extension = (optionsBuilder.Options.FindExtension<HexDBOptionsExtension>() ?? new HexDBOptionsExtension())
            .WithConnection(new Uri(url), apiKey);
        ((IDbContextOptionsBuilderInfrastructure)optionsBuilder).AddOrUpdateExtension(extension);
        options?.Invoke(new HexDBDbContextOptionsBuilder(optionsBuilder));
        return optionsBuilder;
    }

    /// <summary>Use HexDB (see the untyped overload).</summary>
    public static DbContextOptionsBuilder<TContext> UseHexDB<TContext>(this DbContextOptionsBuilder<TContext> optionsBuilder, string url, string? apiKey, Action<HexDBDbContextOptionsBuilder>? options = null)
        where TContext : DbContext =>
        (DbContextOptionsBuilder<TContext>)UseHexDB((DbContextOptionsBuilder)optionsBuilder, url, apiKey, options);
}

/// <summary>HexDB-specific options.</summary>
public class HexDBDbContextOptionsBuilder
{
    private readonly DbContextOptionsBuilder _builder;

    public HexDBDbContextOptionsBuilder(DbContextOptionsBuilder builder) => _builder = builder;

    /// <summary>An HttpClient to use, e.g. one that trusts a private certificate.</summary>
    public virtual HexDBDbContextOptionsBuilder HttpClient(HttpClient httpClient) => With(e => e.WithHttpClient(httpClient));

    /// <summary>Documents fetched per request when a query reads many (1-1000, default 500).</summary>
    public virtual HexDBDbContextOptionsBuilder PageSize(int pageSize) => With(e => e.WithPageSize(pageSize));

    private HexDBDbContextOptionsBuilder With(Func<HexDBOptionsExtension, HexDBOptionsExtension> change)
    {
        var extension = change(_builder.Options.FindExtension<HexDBOptionsExtension>() ?? new HexDBOptionsExtension());
        ((IDbContextOptionsBuilderInfrastructure)_builder).AddOrUpdateExtension(extension);
        return this;
    }
}
