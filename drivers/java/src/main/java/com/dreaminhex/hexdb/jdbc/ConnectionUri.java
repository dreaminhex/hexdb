package com.dreaminhex.hexdb.jdbc;

import java.io.ByteArrayOutputStream;
import java.nio.charset.StandardCharsets;
import java.util.LinkedHashMap;
import java.util.Locale;
import java.util.Map;

/**
 * A HexDB connection URI: {@code hexdb://[login[:password]@]host[:port][/path][?option=value&...]},
 * or {@code hexdbs://...} for HTTPS. See MANUAL.md, Connection URIs.
 */
final class ConnectionUri {
    static final int HTTP_PORT = 7700;
    static final int HTTPS_PORT = 443;
    static final int MAX_PAGE_SIZE = 10_000;

    /** True for hexdbs://. */
    final boolean secure;
    /** The host, lowercased; an IPv6 address keeps its brackets. */
    final String host;
    /** The port given in the URI, or -1 for the scheme's default. */
    final int port;
    /** The base path, without a trailing slash ("" for none). */
    final String path;
    final String login;
    final String password;
    final String apiKey;
    final Integer timeoutSeconds;
    final Integer pageSize;
    final String caFile;
    /** Every option, by lowercased name, percent-decoded (including ones this driver doesn't use). */
    final Map<String, String> options;

    private ConnectionUri(boolean secure, String host, int port, String path, String login, String password,
                          Map<String, String> options) {
        this.secure = secure;
        this.host = host;
        this.port = port;
        this.path = path;
        this.login = login;
        this.password = password;
        this.options = options;
        this.apiKey = options.get("apikey");
        this.timeoutSeconds = options.containsKey("timeout") ? Integer.valueOf(options.get("timeout")) : null;
        this.pageSize = options.containsKey("pagesize") ? Integer.valueOf(options.get("pagesize")) : null;
        this.caFile = options.get("cafile");
    }

    /** True if the text starts with hexdb: or hexdbs: (any case). */
    static boolean hasScheme(String text) {
        return text != null && (text.regionMatches(true, 0, "hexdb:", 0, 6) || text.regionMatches(true, 0, "hexdbs:", 0, 7));
    }

    /** The HTTP base URL: http(s)://host[:port][/path]. */
    String url() {
        return url(secure);
    }

    /** The base URL over HTTPS (true) or HTTP (false); the default port follows the choice. */
    String url(boolean https) {
        int p = port > 0 ? port : (https ? HTTPS_PORT : HTTP_PORT);
        String shown = https && p == HTTPS_PORT ? "" : ":" + p;
        return (https ? "https://" : "http://") + host + shown + path;
    }

    /** Parse a URI; an invalid one throws IllegalArgumentException naming the problem. */
    static ConnectionUri parse(String uri) {
        if (uri == null) {
            throw new IllegalArgumentException("no connection URI given");
        }
        int colon = uri.indexOf(':');
        String scheme = colon < 0 ? "" : uri.substring(0, colon).toLowerCase(Locale.ROOT);
        if (!scheme.equals("hexdb") && !scheme.equals("hexdbs")) {
            throw new IllegalArgumentException("not a hexdb:// or hexdbs:// URI");
        }
        if (!uri.startsWith("//", colon + 1)) {
            throw new IllegalArgumentException("no // after " + scheme + ":");
        }
        boolean secure = scheme.equals("hexdbs");
        String rest = uri.substring(colon + 3);
        if (rest.indexOf('#') >= 0) {
            throw new IllegalArgumentException("a fragment (#...) isn't allowed");
        }
        String query = "";
        int q = rest.indexOf('?');
        if (q >= 0) {
            query = rest.substring(q + 1);
            rest = rest.substring(0, q);
        }
        int slash = rest.indexOf('/');
        String authority = slash < 0 ? rest : rest.substring(0, slash);
        String path = slash < 0 ? "" : rest.substring(slash);
        while (path.endsWith("/")) {
            path = path.substring(0, path.length() - 1);
        }

        String login = null;
        String password = null;
        int at = authority.lastIndexOf('@');
        if (at >= 0) {
            String userinfo = authority.substring(0, at);
            authority = authority.substring(at + 1);
            int c = userinfo.indexOf(':');
            login = decode(c < 0 ? userinfo : userinfo.substring(0, c), "the login");
            password = c < 0 ? null : decode(userinfo.substring(c + 1), "the password");
            if (login.isEmpty()) {
                throw new IllegalArgumentException("an empty login");
            }
        }

        if (authority.indexOf(',') >= 0) {
            throw new IllegalArgumentException("more than one host; give one name that reaches the lattice's hexes");
        }
        String host;
        String portText = null;
        if (authority.startsWith("[")) {
            int close = authority.indexOf(']');
            if (close < 0) {
                throw new IllegalArgumentException("an IPv6 address without a closing ]");
            }
            host = authority.substring(0, close + 1);
            String after = authority.substring(close + 1);
            if (!after.isEmpty()) {
                if (!after.startsWith(":")) {
                    throw new IllegalArgumentException("unexpected text after the IPv6 address");
                }
                portText = after.substring(1);
            }
            if (host.length() == 2) {
                throw new IllegalArgumentException("no host");
            }
        } else {
            int c = authority.indexOf(':');
            if (c >= 0 && authority.indexOf(':', c + 1) >= 0) {
                throw new IllegalArgumentException("an IPv6 address must be in brackets, as [::1]");
            }
            host = c < 0 ? authority : authority.substring(0, c);
            portText = c < 0 ? null : authority.substring(c + 1);
        }
        if (host.isEmpty()) {
            throw new IllegalArgumentException("no host");
        }
        host = host.toLowerCase(Locale.ROOT);
        int port = -1;
        if (portText != null) {
            if (portText.isEmpty() || !portText.chars().allMatch(ch -> ch >= '0' && ch <= '9')) {
                throw new IllegalArgumentException("the port isn't a number");
            }
            port = portText.length() > 5 ? 0 : Integer.parseInt(portText);
            if (port < 1 || port > 65535) {
                throw new IllegalArgumentException("the port is outside 1 to 65535");
            }
        }

        Map<String, String> options = new LinkedHashMap<>();
        for (String pair : query.split("&")) {
            if (pair.isEmpty()) {
                continue;
            }
            int eq = pair.indexOf('=');
            String name = decode(eq < 0 ? pair : pair.substring(0, eq), "an option name").toLowerCase(Locale.ROOT);
            String value = eq < 0 ? "" : decode(pair.substring(eq + 1), "option " + name);
            options.put(name, value);
        }
        if (options.containsKey("apikey") && options.get("apikey").isEmpty()) {
            throw new IllegalArgumentException("apiKey is empty");
        }
        if (options.containsKey("cafile") && options.get("cafile").isEmpty()) {
            throw new IllegalArgumentException("caFile is empty");
        }
        if (options.containsKey("timeout")) {
            Integer t = integer(options.get("timeout"));
            if (t == null || t < 1) {
                throw new IllegalArgumentException("timeout isn't a positive whole number of seconds");
            }
            options.put("timeout", t.toString());
        }
        if (options.containsKey("pagesize")) {
            Integer n = integer(options.get("pagesize"));
            if (n == null || n < 1 || n > MAX_PAGE_SIZE) {
                throw new IllegalArgumentException("pageSize is outside 1 to 10,000");
            }
            options.put("pagesize", n.toString());
        }
        if (login != null && options.containsKey("apikey")) {
            throw new IllegalArgumentException("a login together with apiKey; use one or the other");
        }
        return new ConnectionUri(secure, host, port, path, login, password, options);
    }

    /** A decimal integer of ASCII digits, or null. */
    private static Integer integer(String text) {
        if (text.isEmpty() || text.length() > 9 || !text.chars().allMatch(ch -> ch >= '0' && ch <= '9')) {
            return null;
        }
        return Integer.valueOf(text);
    }

    /** Percent-decode as UTF-8; '+' stays '+'. */
    static String decode(String text, String what) {
        if (text.indexOf('%') < 0) {
            return text;
        }
        ByteArrayOutputStream out = new ByteArrayOutputStream(text.length());
        for (int i = 0; i < text.length(); ) {
            char ch = text.charAt(i);
            if (ch == '%') {
                int hi = i + 2 < text.length() ? Character.digit(text.charAt(i + 1), 16) : -1;
                int lo = i + 2 < text.length() ? Character.digit(text.charAt(i + 2), 16) : -1;
                if (hi < 0 || lo < 0) {
                    throw new IllegalArgumentException("bad percent-encoding in " + what);
                }
                out.write(hi * 16 + lo);
                i += 3;
            } else {
                int end = i;
                while (end < text.length() && text.charAt(end) != '%') {
                    end++;
                }
                byte[] bytes = text.substring(i, end).getBytes(StandardCharsets.UTF_8);
                out.write(bytes, 0, bytes.length);
                i = end;
            }
        }
        return new String(out.toByteArray(), StandardCharsets.UTF_8);
    }
}
