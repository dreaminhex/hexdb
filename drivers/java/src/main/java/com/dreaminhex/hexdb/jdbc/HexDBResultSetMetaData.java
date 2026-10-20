package com.dreaminhex.hexdb.jdbc;

import java.sql.ResultSetMetaData;
import java.sql.SQLException;
import java.sql.Types;
import java.util.List;

/** Column names and types of a result. HexDB reports each column's type from its values. */
public class HexDBResultSetMetaData extends UnsupportedResultSetMetaData {
    private final List<Column> columns;

    HexDBResultSetMetaData(List<Column> columns) {
        this.columns = columns;
    }

    private Column column(int i) throws SQLException {
        if (i < 1 || i > columns.size()) {
            throw new SQLException("Column " + i + " is out of range: the result has " + columns.size() + ".", "07009");
        }
        return columns.get(i - 1);
    }

    @Override
    public int getColumnCount() {
        return columns.size();
    }

    @Override
    public String getColumnName(int i) throws SQLException {
        return column(i).name;
    }

    @Override
    public String getColumnLabel(int i) throws SQLException {
        return column(i).name;
    }

    @Override
    public int getColumnType(int i) throws SQLException {
        return column(i).sqlType;
    }

    @Override
    public String getColumnTypeName(int i) throws SQLException {
        return column(i).typeName;
    }

    @Override
    public String getColumnClassName(int i) throws SQLException {
        return column(i).className();
    }

    @Override
    public int getColumnDisplaySize(int i) throws SQLException {
        return column(i).displaySize();
    }

    @Override
    public int getPrecision(int i) throws SQLException {
        return column(i).precision();
    }

    @Override
    public int getScale(int i) throws SQLException {
        return column(i).sqlType == Types.DOUBLE ? 15 : 0;
    }

    @Override
    public boolean isSigned(int i) throws SQLException {
        int t = column(i).sqlType;
        return t == Types.BIGINT || t == Types.INTEGER || t == Types.SMALLINT || t == Types.DOUBLE;
    }

    @Override
    public boolean isCaseSensitive(int i) throws SQLException {
        int t = column(i).sqlType;
        return t == Types.VARCHAR || t == Types.LONGVARCHAR;
    }

    @Override
    public int isNullable(int i) throws SQLException {
        column(i);
        return ResultSetMetaData.columnNullableUnknown;
    }

    @Override
    public boolean isAutoIncrement(int i) throws SQLException {
        column(i);
        return false;
    }

    @Override
    public boolean isSearchable(int i) throws SQLException {
        column(i);
        return true;
    }

    @Override
    public boolean isCurrency(int i) throws SQLException {
        column(i);
        return false;
    }

    @Override
    public boolean isReadOnly(int i) throws SQLException {
        column(i);
        return true;
    }

    @Override
    public boolean isWritable(int i) throws SQLException {
        column(i);
        return false;
    }

    @Override
    public boolean isDefinitelyWritable(int i) throws SQLException {
        column(i);
        return false;
    }

    @Override
    public String getTableName(int i) throws SQLException {
        column(i);
        return "";
    }

    @Override
    public String getSchemaName(int i) throws SQLException {
        column(i);
        return "";
    }

    @Override
    public String getCatalogName(int i) throws SQLException {
        column(i);
        return "";
    }
}
