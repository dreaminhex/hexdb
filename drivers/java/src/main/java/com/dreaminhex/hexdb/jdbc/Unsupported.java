package com.dreaminhex.hexdb.jdbc;

import java.sql.SQLException;
import java.sql.SQLFeatureNotSupportedException;

/** Errors the driver raises itself. */
final class Unsupported {
    private Unsupported() {
    }

    /** A JDBC method HexDB's driver doesn't implement. */
    static SQLFeatureNotSupportedException feature(String method) {
        return new SQLFeatureNotSupportedException(method + " isn't supported by the HexDB driver.", "0A000");
    }

    /** HexDB's SQL is read-only. */
    static SQLException readOnly() {
        return new SQLException("HexDB's SQL is read-only: write through the REST API, transactions or a client library.", "42000");
    }

    static SQLException closed(String what) {
        return new SQLException("The " + what + " is closed.", "HY010");
    }
}
