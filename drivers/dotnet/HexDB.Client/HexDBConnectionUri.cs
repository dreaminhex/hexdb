// HexDB connection URIs (MANUAL.md, Connection URIs):
//
//   hexdb://[login[:password]@]host[:port][/path][?option=value&...]
//   hexdbs://...
//
// Parsed by hand: System.Uri doesn't know the hexdb schemes' default ports,
// and the shared test vectors (drivers/testing/connection-uris.json) pin down
// details such as refusing a fragment or a second host.

using System.Diagnostics.CodeAnalysis;
using System.Globalization;
using System.Net;
using System.Net.Sockets;
using System.Text;

namespace HexDB.Client;

/// <summary>
/// A parsed HexDB connection URI: <c>hexdb://</c> (HTTP, port 7700 by default) or
/// <c>hexdbs://</c> (HTTPS, port 443 by default), with an optional login and password,
/// a base path, and the options <c>apiKey</c>, <c>timeout</c>, <c>pageSize</c> and <c>caFile</c>.
/// </summary>
public sealed class HexDBConnectionUri
{
    private const int HttpDefaultPort = 7700;
    private const int HttpsDefaultPort = 443;
    private const int MaxPageSize = 10_000;

    private readonly bool _secure;
    private readonly bool _plainUrl;
    private readonly string _host = "";
    private readonly int _port;
    private readonly string _path = "";

    private HexDBConnectionUri(Uri url) => (Url, _plainUrl) = (url, true);

    private HexDBConnectionUri(bool secure, string host, int port, string path, string? login, string? password, string? apiKey, int? timeoutSeconds, int? pageSize, string? caFile)
    {
        _secure = secure;
        _host = host;
        _port = port;
        _path = path;
        Login = login;
        Password = password;
        ApiKey = apiKey;
        Timeout = timeoutSeconds is { } seconds ? TimeSpan.FromSeconds(seconds) : null;
        PageSize = pageSize;
        CaFile = caFile;
        var authority = (host.Contains(':') ? $"[{host}]" : host) + (secure && port == HttpsDefaultPort ? "" : $":{port}");
        Url = new Uri($"{(secure ? "https" : "http")}://{authority}{path}");
    }

    /// <summary>The HTTP or HTTPS base URL of the server, including any base path (no trailing slash).</summary>
    public Uri Url { get; }

    /// <summary>The login to sign in with (<c>POST /auth/login</c>), or null.</summary>
    public string? Login { get; }

    /// <summary>The password for <see cref="Login"/>, or null when the URI has none.</summary>
    public string? Password { get; }

    /// <summary>The <c>apiKey</c> option: an API key, session token or OAuth access token, or null.</summary>
    public string? ApiKey { get; }

    /// <summary>The <c>timeout</c> option: how long to wait for each request, or null.</summary>
    public TimeSpan? Timeout { get; }

    /// <summary>The <c>pageSize</c> option (1 to 10,000): rows per request for drivers that page, or null.</summary>
    public int? PageSize { get; }

    /// <summary>The <c>caFile</c> option: a PEM file of certificates to trust, or null. The .NET client doesn't use it.</summary>
    public string? CaFile { get; }

    /// <summary>
    /// Parse a <c>hexdb://</c> or <c>hexdbs://</c> URI. A plain <c>http://</c> or <c>https://</c>
    /// URL is accepted too and kept as it is, with no login or options.
    /// </summary>
    /// <exception cref="ArgumentNullException"><paramref name="uri"/> is null.</exception>
    /// <exception cref="FormatException">The URI isn't valid; the message names the problem.</exception>
    public static HexDBConnectionUri Parse(string uri)
    {
        ArgumentNullException.ThrowIfNull(uri);
        var (result, error) = ParseCore(uri);
        return result ?? throw new FormatException(error);
    }

    /// <summary>Parse a connection URI (see <see cref="Parse"/>); false if it isn't valid.</summary>
    public static bool TryParse([NotNullWhen(true)] string? uri, [NotNullWhen(true)] out HexDBConnectionUri? result)
    {
        result = uri is null ? null : ParseCore(uri).Result;
        return result is not null;
    }

    /// <summary>The URI with its password and apiKey replaced by <c>***</c>, safe to log.</summary>
    public override string ToString()
    {
        if (_plainUrl) return Url.ToString();
        var text = new StringBuilder(_secure ? "hexdbs://" : "hexdb://");
        if (Login is not null)
        {
            text.Append(Uri.EscapeDataString(Login));
            if (Password is not null) text.Append(":***");
            text.Append('@');
        }
        text.Append(_host.Contains(':') ? $"[{_host}]" : _host);
        if (_port != (_secure ? HttpsDefaultPort : HttpDefaultPort)) text.Append(':').Append(_port.ToString(CultureInfo.InvariantCulture));
        text.Append(_path);
        var options = new List<string>();
        if (ApiKey is not null) options.Add("apiKey=***");
        if (Timeout is { } timeout) options.Add($"timeout={(int)timeout.TotalSeconds}");
        if (PageSize is { } pageSize) options.Add($"pageSize={pageSize}");
        if (CaFile is not null) options.Add($"caFile={Uri.EscapeDataString(CaFile)}");
        if (options.Count > 0) text.Append('?').Append(string.Join('&', options));
        return text.ToString();
    }

    // Error messages never repeat the URI itself: it may hold a password or key.
    private static (HexDBConnectionUri? Result, string Error) ParseCore(string uri)
    {
        static (HexDBConnectionUri?, string) Fail(string error) => (null, error);

        var text = uri.Trim();
        var colon = text.IndexOf(':');
        var scheme = colon < 0 ? "" : text[..colon].ToLowerInvariant();
        if (scheme is "http" or "https")
        {
            return Uri.TryCreate(text, UriKind.Absolute, out var url)
                ? (new HexDBConnectionUri(url), "")
                : Fail($"The {scheme} URL isn't valid.");
        }
        if (scheme is not ("hexdb" or "hexdbs"))
        {
            return Fail(colon < 0
                ? "A connection URI starts with hexdb:// or hexdbs://."
                : $"A connection URI starts with hexdb:// or hexdbs://, not {text[..colon]}://.");
        }
        var secure = scheme == "hexdbs";
        if (string.CompareOrdinal(text, colon + 1, "//", 0, 2) != 0) return Fail($"A {scheme} URI needs // after the scheme ({scheme}://host).");
        var rest = text[(colon + 3)..];
        if (rest.Contains('#')) return Fail("The URI has a fragment (#...), which a connection URI can't have.");

        var question = rest.IndexOf('?');
        var query = question < 0 ? "" : rest[(question + 1)..];
        var beforeQuery = question < 0 ? rest : rest[..question];
        var slash = beforeQuery.IndexOf('/');
        var authority = slash < 0 ? beforeQuery : beforeQuery[..slash];
        var path = slash < 0 ? "" : beforeQuery[slash..].TrimEnd('/');

        // Credentials: everything before the last @ (an @ in a password should be %40).
        string? login = null, password = null;
        var at = authority.LastIndexOf('@');
        if (at >= 0)
        {
            var userInfo = authority[..at];
            authority = authority[(at + 1)..];
            var separator = userInfo.IndexOf(':');
            login = Uri.UnescapeDataString(separator < 0 ? userInfo : userInfo[..separator]);
            password = separator < 0 ? null : Uri.UnescapeDataString(userInfo[(separator + 1)..]);
            if (login.Length == 0) return Fail("The URI has an @ but no login before it.");
        }

        if (authority.Contains(',')) return Fail("The URI names more than one host; give one host, such as a name that reaches several hexes.");
        string host;
        string? portText;
        if (authority.StartsWith('['))
        {
            var close = authority.IndexOf(']');
            if (close < 0) return Fail("The URI's IPv6 host has no closing ].");
            host = authority[1..close];
            var after = authority[(close + 1)..];
            if (after.Length > 0 && after[0] != ':') return Fail("The URI's host has text after the IPv6 address's ].");
            portText = after.Length > 0 ? after[1..] : null;
            if (host.Length == 0) return Fail("The URI has no host.");
            if (!IPAddress.TryParse(host, out var address) || address.AddressFamily != AddressFamily.InterNetworkV6)
            {
                return Fail("The URI's host in brackets isn't an IPv6 address.");
            }
        }
        else
        {
            var portColon = authority.IndexOf(':');
            if (portColon >= 0 && authority.IndexOf(':', portColon + 1) >= 0) return Fail("An IPv6 host must be in brackets, such as hexdb://[::1]:7700.");
            host = portColon < 0 ? authority : authority[..portColon];
            portText = portColon < 0 ? null : authority[(portColon + 1)..];
            if (host.Length == 0) return Fail("The URI has no host.");
            if (Uri.CheckHostName(host) is not (UriHostNameType.Dns or UriHostNameType.IPv4)) return Fail("The URI's host isn't a valid host name or IPv4 address.");
        }
        host = host.ToLowerInvariant();

        var port = secure ? HttpsDefaultPort : HttpDefaultPort;
        if (portText is not null)
        {
            if (!int.TryParse(portText, NumberStyles.None, CultureInfo.InvariantCulture, out port) || port is < 1 or > 65535)
            {
                return Fail("The URI's port must be a number from 1 to 65535.");
            }
        }

        string? apiKey = null, caFile = null;
        int? timeout = null, pageSize = null;
        foreach (var pair in query.Split('&', StringSplitOptions.RemoveEmptyEntries))
        {
            var equals = pair.IndexOf('=');
            var name = Uri.UnescapeDataString(equals < 0 ? pair : pair[..equals]).ToLowerInvariant();
            var value = equals < 0 ? "" : Uri.UnescapeDataString(pair[(equals + 1)..]);
            switch (name)
            {
                case "apikey":
                    if (value.Length == 0) return Fail("The apiKey option is empty.");
                    apiKey = value;
                    break;
                case "timeout":
                    if (!int.TryParse(value, NumberStyles.None, CultureInfo.InvariantCulture, out var seconds) || seconds < 1)
                    {
                        return Fail("The timeout option must be a positive whole number of seconds.");
                    }
                    timeout = seconds;
                    break;
                case "pagesize":
                    if (!int.TryParse(value, NumberStyles.None, CultureInfo.InvariantCulture, out var rows) || rows is < 1 or > MaxPageSize)
                    {
                        return Fail("The pageSize option must be a whole number from 1 to 10,000.");
                    }
                    pageSize = rows;
                    break;
                case "cafile":
                    if (value.Length == 0) return Fail("The caFile option is empty.");
                    caFile = value;
                    break;
                default:
                    break; // options for other drivers
            }
        }
        if (login is not null && apiKey is not null) return Fail("The URI has both a login and an apiKey option; use one or the other.");

        try
        {
            return (new HexDBConnectionUri(secure, host, port, path, login, password, apiKey, timeout, pageSize, caFile), "");
        }
        catch (UriFormatException)
        {
            return Fail("The URI's path isn't valid.");
        }
    }
}
