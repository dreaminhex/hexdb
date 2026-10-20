package com.dreaminhex.hexdb.jdbc;

import java.sql.Types;

/** A result column: its name and SQL type. */
final class Column {
    final String name;
    final int sqlType;
    final String typeName;

    Column(String name, int sqlType, String typeName) {
        this.name = name;
        this.sqlType = sqlType;
        this.typeName = typeName;
    }

    /** A column of HexDB's /sql answer, typed by HexDB's name for it. */
    static Column ofHexType(String name, String hexType) {
        switch (hexType == null ? "" : hexType) {
            case "boolean":
                return new Column(name, Types.BOOLEAN, "BOOLEAN");
            case "integer":
                return new Column(name, Types.BIGINT, "BIGINT");
            case "number":
                return new Column(name, Types.DOUBLE, "DOUBLE");
            case "json":
                return new Column(name, Types.LONGVARCHAR, "JSON");
            default:
                return new Column(name, Types.VARCHAR, "VARCHAR");
        }
    }

    static Column of(String name, int sqlType) {
        switch (sqlType) {
            case Types.INTEGER:
                return new Column(name, sqlType, "INTEGER");
            case Types.SMALLINT:
                return new Column(name, sqlType, "SMALLINT");
            case Types.BIGINT:
                return new Column(name, sqlType, "BIGINT");
            case Types.BOOLEAN:
                return new Column(name, sqlType, "BOOLEAN");
            default:
                return new Column(name, Types.VARCHAR, "VARCHAR");
        }
    }

    String className() {
        switch (sqlType) {
            case Types.BOOLEAN:
                return Boolean.class.getName();
            case Types.BIGINT:
                return Long.class.getName();
            case Types.INTEGER:
                return Integer.class.getName();
            case Types.SMALLINT:
                return Short.class.getName();
            case Types.DOUBLE:
                return Double.class.getName();
            default:
                return String.class.getName();
        }
    }

    int displaySize() {
        switch (sqlType) {
            case Types.BOOLEAN:
                return 5;
            case Types.BIGINT:
                return 20;
            case Types.INTEGER:
                return 11;
            case Types.SMALLINT:
                return 6;
            case Types.DOUBLE:
                return 24;
            case Types.LONGVARCHAR:
                return Integer.MAX_VALUE;
            default:
                return 65535;
        }
    }

    int precision() {
        switch (sqlType) {
            case Types.BOOLEAN:
                return 1;
            case Types.BIGINT:
                return 19;
            case Types.INTEGER:
                return 10;
            case Types.SMALLINT:
                return 5;
            case Types.DOUBLE:
                return 15;
            case Types.LONGVARCHAR:
                return Integer.MAX_VALUE;
            default:
                return 65535;
        }
    }
}
