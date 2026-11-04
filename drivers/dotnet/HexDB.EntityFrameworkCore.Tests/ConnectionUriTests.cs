// UseHexDB with a connection URI: what the options record. No server needed.

using HexDB.EntityFrameworkCore.Infrastructure;
using Microsoft.EntityFrameworkCore;
using Xunit;

public class EntityFrameworkConnectionUriTests
{
    [Fact]
    public void UseHexDBTakesAConnectionUri()
    {
        var options = new DbContextOptionsBuilder<ShopContext>().UseHexDB("hexdbs://db.example.com/hexdb?apiKey=hxk_1&pageSize=50&timeout=10").Options;
        var extension = options.FindExtension<HexDBOptionsExtension>()!;
        Assert.Equal(new Uri("https://db.example.com/hexdb"), extension.Url);
        Assert.Equal("hxk_1", extension.ApiKey);
        Assert.Equal(50, extension.PageSize);
        Assert.Equal(TimeSpan.FromSeconds(10), extension.ConnectionUri!.Timeout);
    }

    [Fact]
    public void ThePageSizeIsCappedAndOptionsWin()
    {
        var capped = new DbContextOptionsBuilder().UseHexDB("hexdb://localhost?pageSize=5000").Options.FindExtension<HexDBOptionsExtension>()!;
        Assert.Equal(1000, capped.PageSize);
        var chosen = new DbContextOptionsBuilder().UseHexDB("hexdb://localhost?pageSize=5000", null, o => o.PageSize(20)).Options.FindExtension<HexDBOptionsExtension>()!;
        Assert.Equal(20, chosen.PageSize);
    }

    [Fact]
    public void AnApiKeyArgumentWinsAndPlainUrlsStillWork()
    {
        var extension = new DbContextOptionsBuilder().UseHexDB("http://127.0.0.1:7700", "hxk_2").Options.FindExtension<HexDBOptionsExtension>()!;
        Assert.Equal(new Uri("http://127.0.0.1:7700"), extension.Url);
        Assert.Equal("hxk_2", extension.ApiKey);
        Assert.Equal(500, extension.PageSize);
    }

    [Fact]
    public void AnInvalidUriIsRefused()
    {
        var error = Assert.ThrowsAny<FormatException>(() => new DbContextOptionsBuilder().UseHexDB("hexdb://localhost?pageSize=0"));
        Assert.Contains("pageSize", error.Message, StringComparison.Ordinal);
    }
}
