using HexDB.Client;
using HexDB.EntityFrameworkCore.Infrastructure;
using Microsoft.EntityFrameworkCore.Infrastructure;

// In the EF Core namespace, like every provider's UseXxx, so it's found without a using.
namespace Microsoft.EntityFrameworkCore;

/// <summary>Configures a DbContext to use HexDB.</summary>
public static class HexDBDbContextOptionsExtensions
{
    // The provider reads at most this many documents per request (see HexDBDbContextOptionsBuilder.PageSize).
    private const int MaxPageSize = 1000;

    /// <summary>
    /// Use HexDB. <paramref name="url"/> is a connection URI (<c>hexdbs://host/path?...</c>, see the manual's
    /// Connection URIs) or the plain URL of any hex of the lattice; <paramref name="apiKey"/>, when given, is an
    /// API key (hxk_...) or session token used instead of the URI's <c>apiKey</c> or login. The user's roles
    /// decide what the context may read and write.
    /// </summary>
    /// <remarks>
    /// From a connection URI the provider takes the login (signing in on the first request), <c>apiKey</c>,
    /// <c>timeout</c> (for the HttpClient it creates) and <c>pageSize</c>, capped at the provider's 1000.
    /// Options set in <paramref name="options"/> win over the URI's.
    /// </remarks>
    /// <exception cref="FormatException">The connection URI isn't valid; the message names the problem.</exception>
    public static DbContextOptionsBuilder UseHexDB(this DbContextOptionsBuilder optionsBuilder, string url, string? apiKey, Action<HexDBDbContextOptionsBuilder>? options = null)
    {
        ArgumentNullException.ThrowIfNull(optionsBuilder);
        var uri = HexDBConnectionUri.Parse(url);
        var extension = (optionsBuilder.Options.FindExtension<HexDBOptionsExtension>() ?? new HexDBOptionsExtension())
            .WithConnection(uri, apiKey);
        if (uri.PageSize is { } pageSize) extension = extension.WithPageSize(Math.Min(pageSize, MaxPageSize));
        ((IDbContextOptionsBuilderInfrastructure)optionsBuilder).AddOrUpdateExtension(extension);
        options?.Invoke(new HexDBDbContextOptionsBuilder(optionsBuilder));
        return optionsBuilder;
    }

    /// <summary>Use HexDB (see the untyped overload).</summary>
    public static DbContextOptionsBuilder<TContext> UseHexDB<TContext>(this DbContextOptionsBuilder<TContext> optionsBuilder, string url, string? apiKey, Action<HexDBDbContextOptionsBuilder>? options = null)
        where TContext : DbContext =>
        (DbContextOptionsBuilder<TContext>)UseHexDB((DbContextOptionsBuilder)optionsBuilder, url, apiKey, options);

    /// <summary>
    /// Use HexDB with a connection URI such as <c>hexdbs://db.example.com?apiKey=hxk_...</c>, or a plain URL
    /// (see the overload that takes an API key). To set provider options as well, call
    /// <c>UseHexDB(connectionString, null, o => ...)</c>.
    /// </summary>
    /// <exception cref="FormatException">The connection URI isn't valid; the message names the problem.</exception>
    public static DbContextOptionsBuilder UseHexDB(this DbContextOptionsBuilder optionsBuilder, string connectionString) =>
        UseHexDB(optionsBuilder, connectionString, null);

    /// <summary>Use HexDB with a connection URI (see the untyped overload).</summary>
    public static DbContextOptionsBuilder<TContext> UseHexDB<TContext>(this DbContextOptionsBuilder<TContext> optionsBuilder, string connectionString)
        where TContext : DbContext =>
        (DbContextOptionsBuilder<TContext>)UseHexDB((DbContextOptionsBuilder)optionsBuilder, connectionString, null);
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
