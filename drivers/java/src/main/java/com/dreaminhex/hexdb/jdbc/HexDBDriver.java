package com.dreaminhex.hexdb.jdbc;

import java.io.InputStream;
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.DriverPropertyInfo;
import java.sql.SQLException;
import java.sql.SQLFeatureNotSupportedException;
import java.util.Map;
import java.util.Properties;
import java.util.logging.Logger;

/**
 * HexDB's JDBC driver. Connect with
 * {@code jdbc:hexdb://host:7700?apiKey=...} (or {@code jdbc:hexdb:https://host:7700},
 * or the user and password properties). See drivers/java/README.md.
 */
public class HexDBDriver extends UnsupportedDriver {
    /** The driver's version (the HexDB release it ships with). */
    public static final String VERSION = loadVersion();

    static {
        try {
            DriverManager.registerDriver(new HexDBDriver());
        } catch (SQLException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    private static String loadVersion() {
        try (InputStream in = HexDBDriver.class.getResourceAsStream("hexdb-jdbc.properties")) {
            Properties p = new Properties();
            if (in != null) {
                p.load(in);
            }
            return p.getProperty("version", "0.0.0");
        } catch (Exception e) {
            return "0.0.0";
        }
    }

    /** A number from a version string such as 2.1.0. */
    static int part(String version, int index) {
        String[] parts = version == null ? new String[0] : version.split("[.-]");
        try {
            return index < parts.length ? Integer.parseInt(parts[index]) : 0;
        } catch (NumberFormatException e) {
            return 0;
        }
    }

    @Override
    public boolean acceptsURL(String url) {
        return url != null && url.startsWith(Client.PREFIX);
    }

    @Override
    public Connection connect(String url, Properties info) throws SQLException {
        if (!acceptsURL(url)) {
            return null;
        }
        return new HexDBConnection(new Client(url, info));
    }

    @Override
    public DriverPropertyInfo[] getPropertyInfo(String url, Properties info) throws SQLException {
        Map<String, String> given = acceptsURL(url) ? Client.settings(url, info) : Map.of();
        String[][] specs = {
            {"apiKey", "An API key (create one on the admin UI's Account page)."},
            {"user", "A login, with password, instead of an API key."},
            {"password", "The login's password."},
            {"ssl", "true for HTTPS with jdbc:hexdb://host:port."},
            {"caFile", "A PEM file of CA certificates to trust besides the JVM's."},
            {"pageSize", "Rows per request, 1 to 10000 (default 1000)."},
            {"timeout", "Seconds a request may take (default 60)."},
            {"connectTimeout", "Seconds to connect (default 10)."},
        };
        DriverPropertyInfo[] out = new DriverPropertyInfo[specs.length];
        for (int i = 0; i < specs.length; i++) {
            out[i] = new DriverPropertyInfo(specs[i][0], given.get(specs[i][0].toLowerCase(java.util.Locale.ROOT)));
            out[i].description = specs[i][1];
            out[i].required = false;
        }
        return out;
    }

    @Override
    public int getMajorVersion() {
        return part(VERSION, 0);
    }

    @Override
    public int getMinorVersion() {
        return part(VERSION, 1);
    }

    /** Not fully SQL-92 compliant (read-only), so not "JDBC compliant" in the spec's sense. */
    @Override
    public boolean jdbcCompliant() {
        return false;
    }

    @Override
    public Logger getParentLogger() throws SQLFeatureNotSupportedException {
        throw Unsupported.feature("Driver.getParentLogger");
    }
}
