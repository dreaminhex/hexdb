package com.dreaminhex.hexdb.jdbc;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.math.BigDecimal;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.sql.Connection;
import java.sql.DatabaseMetaData;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.ResultSetMetaData;
import java.sql.SQLException;
import java.sql.Statement;
import java.sql.Timestamp;
import java.sql.Types;
import java.time.LocalDate;
import java.util.ArrayList;
import java.util.List;
import java.util.Properties;
import org.junit.jupiter.api.Assumptions;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;

/** Against a running server: node drivers/testing/server.mjs mvn -f drivers/java test */
class ServerTest {
    static final String URL = System.getenv("HEXDB_URL");
    static final String KEY = System.getenv("HEXDB_API_KEY");

    @BeforeAll
    static void seed() throws Exception {
        Assumptions.assumeTrue(URL != null && KEY != null, "set HEXDB_URL and HEXDB_API_KEY");
        post("/jcustomers/_bulk", "[{\"code\":\"c1\",\"name\":\"Ada\",\"country\":\"NL\"},{\"code\":\"c2\",\"name\":\"Bo\",\"country\":\"BE\"}]");
        StringBuilder orders = new StringBuilder("[");
        for (int i = 0; i < 9; i++) {
            orders.append(i == 0 ? "" : ",").append("{\"n\":").append(i).append(",\"customer\":\"c").append(i % 2 + 1)
                .append("\",\"total\":").append(i * 2.5).append(",\"paid\":").append(i % 3 != 0)
                .append(",\"placed\":\"2026-03-0").append(i + 1).append("T12:00:00Z\",\"meta\":{\"k\":").append(i).append("}}");
        }
        post("/jorders/_bulk", orders.append("]").toString());
    }

    static void post(String path, String body) throws Exception {
        HttpResponse<String> r = HttpClient.newHttpClient().send(HttpRequest.newBuilder(URI.create(URL + path))
            .header("Authorization", "Bearer " + KEY).header("Content-Type", "application/json")
            .POST(HttpRequest.BodyPublishers.ofString(body)).build(), HttpResponse.BodyHandlers.ofString());
        assertTrue(r.statusCode() < 300, r.body());
    }

    static Connection connect(String extra) throws SQLException {
        Properties p = new Properties();
        p.setProperty("apiKey", KEY);
        return DriverManager.getConnection("jdbc:hexdb:" + URL + extra, p);
    }

    @Test
    void queriesPageAndConvert() throws SQLException {
        try (Connection c = connect("?pageSize=2"); PreparedStatement s = c.prepareStatement("SELECT n, total, paid, placed, meta, customer FROM jorders WHERE total >= ? ORDER BY n")) {
            s.setBigDecimal(1, new BigDecimal("5"));
            List<Long> ns = new ArrayList<>();
            try (ResultSet r = s.executeQuery()) {
                ResultSetMetaData md = r.getMetaData();
                assertEquals(6, md.getColumnCount());
                assertEquals("n", md.getColumnName(1));
                assertEquals(Types.BIGINT, md.getColumnType(1));
                assertEquals(Types.DOUBLE, md.getColumnType(2));
                assertEquals(Types.BOOLEAN, md.getColumnType(3));
                assertTrue(r.isBeforeFirst());
                while (r.next()) {
                    ns.add(r.getLong("n"));
                    if (r.getInt(1) == 2) {
                        assertEquals(5.0, r.getDouble("total"));
                        assertEquals(new BigDecimal("5.0"), r.getBigDecimal(2));
                        assertTrue(r.getBoolean("paid"));
                        assertEquals(Timestamp.from(java.time.Instant.parse("2026-03-03T12:00:00Z")), r.getTimestamp("placed"));
                        assertEquals(LocalDate.of(2026, 3, 3), r.getObject("placed", java.time.OffsetDateTime.class).toLocalDate());
                        assertEquals("{\"k\":2}", r.getString("meta"));
                        assertEquals(2L, r.getObject(1));
                        assertEquals("c1", r.getString(6));
                    }
                }
                assertTrue(r.isAfterLast());
            }
            // Seven rows over four pages of two.
            assertEquals(List.of(2L, 3L, 4L, 5L, 6L, 7L, 8L), ns);
            s.setMaxRows(3);
            int count = 0;
            try (ResultSet r = s.executeQuery()) {
                while (r.next()) {
                    count++;
                }
            }
            assertEquals(3, count);
        }
    }

    @Test
    void joinsSubqueriesAndEscapes() throws SQLException {
        try (Connection c = connect(""); Statement s = c.createStatement()) {
            try (ResultSet r = s.executeQuery("SELECT {fn UCASE(cu.name)} AS name, COUNT(*) AS orders, SUM(o.total) AS spent "
                + "FROM {oj jorders o LEFT OUTER JOIN jcustomers cu ON o.customer = cu.code} GROUP BY cu.name ORDER BY name")) {
                assertTrue(r.next());
                assertEquals("ADA", r.getString("name"));
                assertEquals(5, r.getInt("orders"));
                assertEquals(50.0, r.getDouble("spent"));
                assertTrue(r.next());
                assertEquals("BO", r.getString(1));
                assertFalse(r.next());
            }
            try (ResultSet r = s.executeQuery("SELECT name FROM jcustomers WHERE code IN (SELECT customer FROM jorders WHERE total > 18)")) {
                assertTrue(r.next());
                assertEquals("Ada", r.getString(1));
                assertTrue(r.isLast());
                assertFalse(r.next());
            }
            try (ResultSet r = s.executeQuery("SELECT NULL AS nothing, 1 AS one")) {
                assertTrue(r.next());
                assertNull(r.getString(1));
                assertTrue(r.wasNull());
                assertEquals(0, r.getInt(1));
                assertEquals(1, r.getInt(2));
                assertFalse(r.wasNull());
            }
        }
    }

    @Test
    void metadataDescribesTheLattice() throws SQLException {
        try (Connection c = connect("")) {
            DatabaseMetaData md = c.getMetaData();
            assertEquals("HexDB", md.getDatabaseProductName());
            assertFalse(md.getDatabaseProductVersion().isEmpty());
            assertTrue(md.isReadOnly() && md.supportsOuterJoins() && md.supportsCorrelatedSubqueries() && md.supportsUnionAll());
            List<String> tables = new ArrayList<>();
            try (ResultSet r = md.getTables(null, null, "j%", new String[] {"TABLE"})) {
                while (r.next()) {
                    tables.add(r.getString("TABLE_NAME"));
                }
            }
            assertEquals(List.of("jcustomers", "jorders"), tables);
            List<String> columns = new ArrayList<>();
            try (ResultSet r = md.getColumns(null, null, "jorders", "%")) {
                while (r.next()) {
                    columns.add(r.getString("COLUMN_NAME") + ":" + r.getInt("DATA_TYPE"));
                }
            }
            assertEquals("id:" + Types.VARCHAR, columns.get(0));
            assertTrue(columns.contains("total:" + Types.DOUBLE), columns.toString());
            try (ResultSet r = md.getPrimaryKeys(null, null, "jorders")) {
                assertTrue(r.next());
                assertEquals("id", r.getString("COLUMN_NAME"));
            }
            // Metadata HexDB doesn't have comes back empty, with the standard columns.
            try (ResultSet r = md.getImportedKeys(null, null, "jorders")) {
                assertEquals("FKCOLUMN_NAME", r.getMetaData().getColumnName(8));
                assertFalse(r.next());
            }
            try (ResultSet r = md.getTypeInfo()) {
                int n = 0;
                while (r.next()) {
                    n++;
                }
                assertEquals(5, n);
            }
        }
    }

    @Test
    void errorsCarrySqlStates() throws SQLException {
        try (Connection c = connect(""); Statement s = c.createStatement()) {
            assertEquals("42S02", assertThrows(SQLException.class, () -> s.executeQuery("SELECT * FROM nothing_here")).getSQLState());
            assertEquals("42000", assertThrows(SQLException.class, () -> s.executeQuery("SELECT n, ROW_NUMBER() OVER () FROM jorders")).getSQLState());
            assertEquals("42000", assertThrows(SQLException.class, () -> s.executeUpdate("DELETE FROM jorders")).getSQLState());
            assertEquals("0A000", assertThrows(SQLException.class, () -> c.prepareCall("{call x()}")).getSQLState());
            PreparedStatement p = c.prepareStatement("SELECT n FROM jorders WHERE n = ?");
            assertEquals("07002", assertThrows(SQLException.class, p::executeQuery).getSQLState());
        }
        Properties bad = new Properties();
        bad.setProperty("apiKey", "wrong");
        assertEquals("28000", assertThrows(SQLException.class, () -> DriverManager.getConnection("jdbc:hexdb:" + URL, bad)).getSQLState());
        assertEquals("08001", assertThrows(SQLException.class, () -> DriverManager.getConnection("jdbc:hexdb://127.0.0.1:1?apiKey=x")).getSQLState());
    }

    @Test
    void signsInWithALoginAndPassword() throws SQLException {
        String login = System.getenv("HEXDB_ADMIN_LOGIN");
        String password = System.getenv("HEXDB_ADMIN_PASSWORD");
        Assumptions.assumeTrue(login != null && password != null, "set HEXDB_ADMIN_LOGIN and HEXDB_ADMIN_PASSWORD");
        try (Connection c = DriverManager.getConnection("jdbc:hexdb:" + URL, login, password)) {
            assertEquals(login, c.getMetaData().getUserName());
            assertTrue(c.isValid(5));
            try (ResultSet r = c.createStatement().executeQuery("SELECT COUNT(*) FROM jorders")) {
                assertTrue(r.next());
                assertEquals(9, r.getInt(1));
            }
        }
    }
}
