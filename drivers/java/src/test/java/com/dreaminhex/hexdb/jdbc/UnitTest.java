package com.dreaminhex.hexdb.jdbc;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.math.BigDecimal;
import java.math.BigInteger;
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

    @Test
    void theDriverRegistersItself() throws SQLException {
        assertTrue(DriverManager.getDriver("jdbc:hexdb://localhost:7700") instanceof HexDBDriver);
        assertFalse(new HexDBDriver().acceptsURL("jdbc:postgresql://x"));
        assertNull(new HexDBDriver().connect("jdbc:postgresql://x", new Properties()));
    }
}
