package com.dreaminhex.hexdb.jdbc;

import java.io.IOException;
import java.io.InputStream;
import java.net.URI;
import java.net.URLDecoder;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Paths;
import java.security.KeyStore;
import java.security.cert.Certificate;
import java.security.cert.CertificateException;
import java.security.cert.CertificateFactory;
import java.security.cert.X509Certificate;
import java.sql.SQLException;
import java.sql.SQLInvalidAuthorizationSpecException;
import java.sql.SQLNonTransientConnectionException;
import java.sql.SQLSyntaxErrorException;
import java.sql.SQLTimeoutException;
import java.time.Duration;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Properties;
import javax.net.ssl.SSLContext;
import javax.net.ssl.TrustManager;
import javax.net.ssl.TrustManagerFactory;
import javax.net.ssl.X509TrustManager;

/** HexDB's HTTP API, as the driver uses it: sign-in, /sql and the SQL catalog. */
final class Client {
    static final String PREFIX = "jdbc:hexdb:";
    static final int DEFAULT_PAGE_SIZE = 1000;
    static final int MAX_PAGE_SIZE = 10_000;

    final String base;
    final String url;
    final int pageSize;
    private final Duration timeout;
    private final HttpClient http;
    private String token = "";
    String login = "";
    String serverVersion = "";

    /** Settings from the URL and the connection properties (properties win). */
    static Map<String, String> settings(String url, Properties info) throws SQLException {
        Map<String, String> values = new LinkedHashMap<>();
        String rest = url.substring(PREFIX.length());
        int query = rest.indexOf('?');
        if (query >= 0) {
            for (String pair : rest.substring(query + 1).split("&")) {
                if (pair.isEmpty()) {
                    continue;
                }
                int eq = pair.indexOf('=');
                String key = eq < 0 ? pair : pair.substring(0, eq);
                String value = eq < 0 ? "" : pair.substring(eq + 1);
                values.put(key.toLowerCase(Locale.ROOT), URLDecoder.decode(value, StandardCharsets.UTF_8));
            }
            rest = rest.substring(0, query);
        }
        if (info != null) {
            for (String name : info.stringPropertyNames()) {
                values.put(name.toLowerCase(Locale.ROOT), info.getProperty(name));
            }
        }
        // jdbc:hexdb://host:port, jdbc:hexdb:http://host:port or jdbc:hexdb:https://host:port
        String server;
        if (rest.startsWith("//")) {
            boolean tls = isTrue(values.get("ssl")) || isTrue(values.get("tls"));
            server = (tls ? "https:" : "http:") + rest;
        } else if (rest.startsWith("http://") || rest.startsWith("https://")) {
            server = rest;
        } else {
            throw new SQLNonTransientConnectionException("Use jdbc:hexdb://host:port (or jdbc:hexdb:https://host:port), not " + url + ".", "08001");
        }
        while (server.endsWith("/")) {
            server = server.substring(0, server.length() - 1);
        }
        values.put("server", server);
        return values;
    }

    private static boolean isTrue(String v) {
        return v != null && (v.equalsIgnoreCase("true") || v.equals("1") || v.equalsIgnoreCase("yes"));
    }

    private static int number(Map<String, String> values, String key, int fallback, int min, int max) throws SQLException {
        String v = values.get(key.toLowerCase(Locale.ROOT));
        if (v == null || v.isBlank()) {
            return fallback;
        }
        try {
            int n = Integer.parseInt(v.trim());
            if (n >= min && n <= max) {
                return n;
            }
        } catch (NumberFormatException ignored) {
            // reported below
        }
        throw new SQLException(key + " must be a number from " + min + " to " + max + ".", "HY024");
    }

    Client(String url, Properties info) throws SQLException {
        Map<String, String> values = settings(url, info);
        this.url = url;
        this.base = values.get("server");
        this.pageSize = number(values, "pageSize", DEFAULT_PAGE_SIZE, 1, MAX_PAGE_SIZE);
        this.timeout = Duration.ofSeconds(number(values, "timeout", 60, 1, 3600));
        HttpClient.Builder builder = HttpClient.newBuilder().connectTimeout(Duration.ofSeconds(number(values, "connectTimeout", 10, 1, 600)));
        String caFile = values.get("cafile");
        if (caFile != null && !caFile.isBlank()) {
            builder.sslContext(withCa(caFile.trim()));
        }
        this.http = builder.build();

        String apiKey = values.get("apikey");
        String user = values.get("user");
        String password = values.get("password");
        if (apiKey != null && !apiKey.isBlank()) {
            token = apiKey.trim();
        } else if (user != null && !user.isBlank() && password != null) {
            Map<String, Object> body = new LinkedHashMap<>();
            body.put("login", user);
            body.put("password", password);
            body.put("return_token", true);
            Object answer = request("POST", "/auth/login", body);
            Object t = answer instanceof Map ? ((Map<?, ?>) answer).get("token") : null;
            if (!(t instanceof String)) {
                throw new SQLInvalidAuthorizationSpecException("The server didn't return a session token. If the account uses MFA, connect with an API key (apiKey=...).", "28000");
            }
            token = (String) t;
        } else {
            throw new SQLInvalidAuthorizationSpecException("No credentials: give apiKey=... (create one on the admin UI's Account page), or user and password.", "28000");
        }
        Object me = request("GET", "/auth/me", null);
        if (me instanceof Map && ((Map<?, ?>) me).get("login") instanceof String) {
            login = (String) ((Map<?, ?>) me).get("login");
        }
        try {
            Object spec = request("GET", "/openapi.json", null);
            Object about = spec instanceof Map ? ((Map<?, ?>) spec).get("info") : null;
            Object version = about instanceof Map ? ((Map<?, ?>) about).get("version") : null;
            serverVersion = version instanceof String ? (String) version : "";
        } catch (SQLException ignored) {
            // The version is only for DatabaseMetaData.
        }
    }

    /** Trust the CA file's certificates as well as the JVM's default ones. */
    private static SSLContext withCa(String path) throws SQLException {
        try (InputStream in = Files.newInputStream(Paths.get(path))) {
            KeyStore store = KeyStore.getInstance(KeyStore.getDefaultType());
            store.load(null, null);
            int n = 0;
            for (Certificate cert : CertificateFactory.getInstance("X.509").generateCertificates(in)) {
                store.setCertificateEntry("ca" + n++, cert);
            }
            if (n == 0) {
                throw new SQLNonTransientConnectionException("caFile " + path + " has no certificates.", "08001");
            }
            TrustManagerFactory custom = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
            custom.init(store);
            TrustManagerFactory system = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
            system.init((KeyStore) null);
            X509TrustManager mine = firstX509(custom.getTrustManagers());
            X509TrustManager defaults = firstX509(system.getTrustManagers());
            X509TrustManager both = new X509TrustManager() {
                @Override
                public void checkClientTrusted(X509Certificate[] chain, String auth) throws CertificateException {
                    defaults.checkClientTrusted(chain, auth);
                }

                @Override
                public void checkServerTrusted(X509Certificate[] chain, String auth) throws CertificateException {
                    try {
                        mine.checkServerTrusted(chain, auth);
                    } catch (CertificateException e) {
                        defaults.checkServerTrusted(chain, auth);
                    }
                }

                @Override
                public X509Certificate[] getAcceptedIssuers() {
                    List<X509Certificate> all = new ArrayList<>(List.of(mine.getAcceptedIssuers()));
                    all.addAll(List.of(defaults.getAcceptedIssuers()));
                    return all.toArray(new X509Certificate[0]);
                }
            };
            SSLContext context = SSLContext.getInstance("TLS");
            context.init(null, new TrustManager[] {both}, null);
            return context;
        } catch (SQLException e) {
            throw e;
        } catch (Exception e) {
            throw new SQLNonTransientConnectionException("Couldn't read caFile " + path + ": " + e.getMessage(), "08001", e);
        }
    }

    private static X509TrustManager firstX509(TrustManager[] managers) {
        for (TrustManager m : managers) {
            if (m instanceof X509TrustManager) {
                return (X509TrustManager) m;
            }
        }
        throw new IllegalStateException("no X.509 trust manager");
    }

    /** One request; a non-2xx answer becomes an SQLException with HexDB's message. */
    Object request(String method, String path, Object body) throws SQLException {
        HttpRequest.Builder request = HttpRequest.newBuilder(URI.create(base + path)).timeout(timeout).header("Accept", "application/json");
        if (!token.isEmpty()) {
            request.header("Authorization", "Bearer " + token);
        }
        if (body != null) {
            request.header("Content-Type", "application/json").method(method, HttpRequest.BodyPublishers.ofString(Json.write(body)));
        } else {
            request.method(method, HttpRequest.BodyPublishers.noBody());
        }
        HttpResponse<String> response;
        try {
            response = http.send(request.build(), HttpResponse.BodyHandlers.ofString(StandardCharsets.UTF_8));
        } catch (java.net.http.HttpTimeoutException e) {
            throw new SQLTimeoutException("HexDB at " + base + " didn't answer in time.", "HYT00", e);
        } catch (IOException e) {
            throw new SQLNonTransientConnectionException("Couldn't reach HexDB at " + base + ": " + e.getMessage(), "08001", e);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            throw new SQLException("Interrupted while waiting for HexDB.", "HY008", e);
        }
        int status = response.statusCode();
        Object json;
        try {
            json = response.body().isEmpty() ? null : Json.parse(response.body());
        } catch (IllegalArgumentException e) {
            json = null;
        }
        if (status >= 200 && status < 300) {
            return json;
        }
        String message = null;
        if (json instanceof Map && ((Map<?, ?>) json).get("error") instanceof Map) {
            Object m = ((Map<?, ?>) ((Map<?, ?>) json).get("error")).get("message");
            message = m instanceof String ? (String) m : null;
        }
        if (message == null) {
            String text = response.body();
            message = "HTTP " + status + ": " + (text.length() > 200 ? text.substring(0, 200) : text);
        }
        switch (status) {
            case 401:
                throw new SQLInvalidAuthorizationSpecException(message, "28000", status);
            case 403:
            case 400:
            case 422:
                throw new SQLSyntaxErrorException(message, "42000", status);
            case 404:
                throw new SQLSyntaxErrorException(message, "42S02", status);
            case 408:
            case 504:
                throw new SQLTimeoutException(message, "HYT00", status);
            default:
                throw new SQLException(message, "HY000", status);
        }
    }

    /** One page of a statement's rows: {columns, rows, next}. */
    @SuppressWarnings("unchecked")
    Map<String, Object> sql(String sql, List<Object> params, String cursor, int size) throws SQLException {
        Map<String, Object> body = new LinkedHashMap<>();
        body.put("sql", sql);
        body.put("params", params);
        body.put("page_size", size);
        if (cursor != null) {
            body.put("cursor", cursor);
        }
        Object answer = request("POST", "/sql", body);
        if (!(answer instanceof Map)) {
            throw new SQLException("HexDB answered /sql with something other than an object.", "HY000");
        }
        return (Map<String, Object>) answer;
    }

    /** Tessellations the caller can read. */
    List<String> tables() throws SQLException {
        List<String> names = new ArrayList<>();
        Object answer = request("GET", "/sql/tables", null);
        if (answer instanceof Map && ((Map<?, ?>) answer).get("tables") instanceof List) {
            for (Object t : (List<?>) ((Map<?, ?>) answer).get("tables")) {
                if (t instanceof Map && ((Map<?, ?>) t).get("name") instanceof String) {
                    names.add((String) ((Map<?, ?>) t).get("name"));
                }
            }
        }
        return names;
    }

    /** One table's columns: name, type, nullable, source. */
    @SuppressWarnings("unchecked")
    List<Map<String, Object>> columns(String table) throws SQLException {
        Object answer = request("GET", "/sql/columns?table=" + java.net.URLEncoder.encode(table, StandardCharsets.UTF_8), null);
        List<Map<String, Object>> out = new ArrayList<>();
        if (answer instanceof Map && ((Map<?, ?>) answer).get("columns") instanceof List) {
            for (Object c : (List<?>) ((Map<?, ?>) answer).get("columns")) {
                if (c instanceof Map) {
                    out.add((Map<String, Object>) c);
                }
            }
        }
        return out;
    }

    boolean healthy() {
        try {
            request("GET", "/health", null);
            return true;
        } catch (SQLException e) {
            return false;
        }
    }
}
