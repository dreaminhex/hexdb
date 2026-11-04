package com.dreaminhex.hexdb.jdbc;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.math.BigDecimal;
import java.math.BigInteger;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.sql.DriverManager;
import java.sql.SQLException;
import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Properties;
import org.junit.jupiter.api.Test;

class UnitTest {
    @Test
    void jsonRoundTrips() {
        Object parsed = Json.parse("{\"a\": [1, -2.5, 1e2, true, null, \"x\\\"\\u00e9\\n\"], \"big\": 123456789012345678901234567890, \"exact\": 0.1}");
        Map<?, ?> map = (Map<?, ?>) parsed;
        List<?> a = (List<?>) map.get("a");
        assertEquals(1L, a.get(0));
        assertEquals(-2.5, a.get(1));
        assertEquals(100.0, a.get(2));
        assertEquals(Boolean.TRUE, a.get(3));
        assertNull(a.get(4));
        assertEquals("x\"é\n", a.get(5));
        assertEquals(new BigInteger("123456789012345678901234567890"), map.get("big"));
        assertEquals(0.1, map.get("exact"));
        Map<String, Object> out = new LinkedHashMap<>();
        out.put("s", "a\"b\\c\u0001");
        out.put("n", Arrays.asList(1L, 2.5, new BigDecimal("1.10"), null, false));
        assertEquals("{\"s\":\"a\\\"b\\\\c\\u0001\",\"n\":[1,2.5,1.10,null,false]}", Json.write(out));
        assertThrows(IllegalArgumentException.class, () -> Json.parse("{\"a\": 1,}"));
        assertThrows(IllegalArgumentException.class, () -> Json.parse("[1] 2"));
    }

    @Test
    void escapesAreRewrittenOutsideLiterals() {
        assertEquals("SELECT * FROM t WHERE d >=  '2026-01-01'", Escapes.rewrite("SELECT * FROM t WHERE d >= {d '2026-01-01'}"));
        assertEquals("SELECT  UCASE(a) FROM t", Escapes.rewrite("SELECT {fn UCASE(a)} FROM t"));
        assertEquals("SELECT * FROM  a LEFT OUTER JOIN b ON a.k = b.k", Escapes.rewrite("SELECT * FROM {oj a LEFT OUTER JOIN b ON a.k = b.k}"));
        assertEquals("SELECT '{d}' FROM t", Escapes.rewrite("SELECT '{d}' FROM t"));
        assertEquals("SELECT a FROM t WHERE a LIKE 'x!_%' ESCAPE '!'", Escapes.rewrite("SELECT a FROM t WHERE a LIKE 'x!_%' {escape '!'}"));
        assertEquals(2, Escapes.countParameters("SELECT * FROM t WHERE a = ? AND b = '?' AND \"c?\" = ? -- ?\n"));
        assertTrue(Escapes.matches("ord%", "orders"));
        assertTrue(Escapes.matches("o_ders", "orders"));
        assertFalse(Escapes.matches("o\\_ders", "orders"));
        assertTrue(Escapes.matches(null, "anything"));
    }

    @Test
    void urlsParse() throws SQLException {
        Map<String, String> s = Client.settings("jdbc:hexdb://h:7700/?apiKey=a%20b&pageSize=5", new Properties());
        assertEquals("http://h:7700", s.get("server"));
        assertEquals("a b", s.get("apikey"));
        assertEquals("5", s.get("pagesize"));
        assertEquals("https://h:7700", Client.settings("jdbc:hexdb://h:7700?ssl=true", null).get("server"));
        assertEquals("https://h", Client.settings("jdbc:hexdb:https://h/", null).get("server"));
        Properties p = new Properties();
        p.setProperty("apiKey", "from-properties");
        assertEquals("from-properties", Client.settings("jdbc:hexdb://h?apiKey=from-url", p).get("apikey"));
        assertThrows(SQLException.class, () -> Client.settings("jdbc:hexdb:h:7700", null));
    }

    /** The shared vectors in drivers/testing/connection-uris.json, each prefixed with jdbc:. */
    @Test
    void connectionUriVectors() throws Exception {
        Path file = Paths.get(System.getProperty("basedir", ".")).resolve("../testing/connection-uris.json");
        Map<?, ?> vectors = (Map<?, ?>) Json.parse(Files.readString(file));
        List<?> valid = (List<?>) vectors.get("valid");
        List<?> invalid = (List<?>) vectors.get("invalid");
        assertFalse(valid.isEmpty());
        assertFalse(invalid.isEmpty());
        for (Object v : valid) {
            Map<?, ?> t = (Map<?, ?>) v;
            String url = "jdbc:" + t.get("uri");
            assertTrue(new HexDBDriver().acceptsURL(url), url);
            Map<String, String> s = Client.settings(url, null);
            assertEquals(t.get("url"), s.get("server"), url);
            assertEquals(t.get("login"), s.get("user"), url);
            assertEquals(t.get("password"), s.get("password"), url);
            assertEquals(t.get("apiKey"), s.get("apikey"), url);
            assertEquals(text(t.get("timeoutSeconds")), s.get("timeout"), url);
            assertEquals(text(t.get("pageSize")), s.get("pagesize"), url);
            assertEquals(t.get("caFile"), s.get("cafile"), url);
        }
        for (Object v : invalid) {
            Map<?, ?> t = (Map<?, ?>) v;
            String url = "jdbc:" + t.get("uri");
            SQLException e = assertThrows(SQLException.class, () -> Client.settings(url, null), url);
            assertEquals("08001", e.getSQLState(), url);
            String reason = (String) t.get("reason");
            String expected = reason.contains("host") ? "host"
                : reason.contains("apiKey") ? "apiKey"
                : reason.contains("timeout") ? "timeout"
                : reason.contains("pageSize") ? "pageSize"
                : reason.contains("port") ? "port"
                : reason.contains("fragment") ? "fragment"
                : reason.contains("//") ? "//"
                : "hexdb";
            assertTrue(e.getMessage().contains(expected), url + ": " + e.getMessage());
        }
    }

    private static String text(Object number) {
        return number == null ? null : number.toString();
    }

    @Test
    void propertiesWinOverTheUri() throws SQLException {
        Properties p = new Properties();
        p.setProperty("timeout", "30");
        p.setProperty("PageSize", "50");
        p.setProperty("caFile", "/etc/ca.pem");
        Map<String, String> s = Client.settings("jdbc:hexdbs://db.example.com?timeout=5&pageSize=500&caFile=x.pem&apiKey=from-uri", p);
        assertEquals("30", s.get("timeout"));
        assertEquals("50", s.get("pagesize"));
        assertEquals("/etc/ca.pem", s.get("cafile"));
        assertEquals("from-uri", s.get("apikey"));

        // A login in the properties replaces the URI's API key, and an API key replaces its login.
        Properties login = new Properties();
        login.setProperty("user", "grace");
        login.setProperty("password", "pw");
        s = Client.settings("jdbc:hexdbs://db.example.com?apiKey=from-uri", login);
        assertNull(s.get("apikey"));
        assertEquals("grace", s.get("user"));
        assertEquals("pw", s.get("password"));
        Properties key = new Properties();
        key.setProperty("apiKey", "hxk_props");
        s = Client.settings("jdbc:hexdbs://ada:secret@db.example.com", key);
        assertEquals("hxk_props", s.get("apikey"));
        assertNull(s.get("user"));
        assertNull(s.get("password"));
        Properties other = new Properties();
        other.setProperty("user", "grace");
        other.setProperty("password", "pw2");
        s = Client.settings("jdbc:hexdb://ada:secret@localhost", other);
        assertEquals("grace", s.get("user"));
        assertEquals("pw2", s.get("password"));

        // The URI's password goes with its login only.
        Properties userOnly = new Properties();
        userOnly.setProperty("user", "grace");
        s = Client.settings("jdbc:hexdb://ada:secret@localhost", userOnly);
        assertEquals("grace", s.get("user"));
        assertNull(s.get("password"));
        Properties passwordOnly = new Properties();
        passwordOnly.setProperty("password", "typed");
        s = Client.settings("jdbc:hexdb://ada@localhost", passwordOnly);
        assertEquals("ada", s.get("user"));
        assertEquals("typed", s.get("password"));
    }

    @Test
    void olderUrlFormsStillParse() throws SQLException {
        assertEquals("http://h:7700", Client.settings("jdbc:hexdb://h:7700", null).get("server"));
        assertEquals("https://h:8443", Client.settings("jdbc:hexdb://h:8443?ssl=true", null).get("server"));
        assertEquals("https://h", Client.settings("jdbc:hexdb://h?ssl=true", null).get("server"));
        assertEquals("https://h:7700", Client.settings("jdbc:hexdb://h:7700?tls=yes", null).get("server"));
        Properties ssl = new Properties();
        ssl.setProperty("ssl", "true");
        assertEquals("https://h:7700", Client.settings("jdbc:hexdb://h:7700", ssl).get("server"));
        assertEquals("http://127.0.0.1:7700", Client.settings("jdbc:hexdb:http://127.0.0.1:7700/", null).get("server"));
        Map<String, String> s = Client.settings("jdbc:hexdb:https://h:8443/base?apiKey=k&connectTimeout=3", null);
        assertEquals("https://h:8443/base", s.get("server"));
        assertEquals("k", s.get("apikey"));
        assertEquals("3", s.get("connecttimeout"));
        s = Client.settings("jdbc:hexdb://h:7700?user=ada&password=p%40ss&connectTimeout=3", null);
        assertEquals("ada", s.get("user"));
        assertEquals("p@ss", s.get("password"));
        assertEquals("3", s.get("connecttimeout"));
        assertTrue(new HexDBDriver().acceptsURL("jdbc:hexdb:https://h"));
        assertTrue(new HexDBDriver().acceptsURL("jdbc:hexdbs://h"));
        assertTrue(new HexDBDriver().acceptsURL("JDBC:HEXDBS://h"));
        assertFalse(new HexDBDriver().acceptsURL("jdbc:hexdbx://h"));
        assertFalse(new HexDBDriver().acceptsURL(null));
    }

    @Test
    void theDriverRegistersItself() throws SQLException {
        assertTrue(DriverManager.getDriver("jdbc:hexdb://localhost:7700") instanceof HexDBDriver);
        assertFalse(new HexDBDriver().acceptsURL("jdbc:postgresql://x"));
        assertNull(new HexDBDriver().connect("jdbc:postgresql://x", new Properties()));
    }
}
