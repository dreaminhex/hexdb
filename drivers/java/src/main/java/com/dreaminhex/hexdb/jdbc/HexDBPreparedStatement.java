package com.dreaminhex.hexdb.jdbc;

import java.io.IOException;
import java.io.Reader;
import java.math.BigDecimal;
import java.math.BigInteger;
import java.sql.Date;
import java.sql.ResultSet;
import java.sql.ResultSetMetaData;
import java.sql.SQLException;
import java.sql.Time;
import java.sql.Timestamp;
import java.sql.Types;
import java.time.Instant;
import java.time.LocalDate;
import java.time.LocalDateTime;
import java.time.LocalTime;
import java.time.OffsetDateTime;
import java.time.ZonedDateTime;
import java.time.format.DateTimeFormatter;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Calendar;
import java.util.List;

/** A statement with ? parameters, sent as values with the statement (never spliced into its text). */
public class HexDBPreparedStatement extends UnsupportedPreparedStatement {
    private final String sql;
    private final Object[] params;
    private final boolean[] set;

    HexDBPreparedStatement(HexDBConnection connection, String sql) {
        super(connection);
        this.sql = Escapes.rewrite(sql);
        int n = Escapes.countParameters(this.sql);
        this.params = new Object[n];
        this.set = new boolean[n];
    }

    private List<Object> values() throws SQLException {
        for (int i = 0; i < set.length; i++) {
            if (!set[i]) {
                throw new SQLException("Parameter " + (i + 1) + " has no value.", "07002");
            }
        }
        return new ArrayList<>(Arrays.asList(params));
    }

    private void put(int index, Object value) throws SQLException {
        check();
        if (index < 1 || index > params.length) {
            throw new SQLException("Parameter " + index + " is out of range: the statement has " + params.length + ".", "07009");
        }
        params[index - 1] = value;
        set[index - 1] = true;
    }

    @Override
    public ResultSet executeQuery() throws SQLException {
        return run(sql, values());
    }

    @Override
    public boolean execute() throws SQLException {
        run(sql, values());
        return true;
    }

    @Override
    public int executeUpdate() throws SQLException {
        throw Unsupported.readOnly();
    }

    @Override
    public long executeLargeUpdate() throws SQLException {
        throw Unsupported.readOnly();
    }

    @Override
    public ResultSet executeQuery(String sql) throws SQLException {
        throw new SQLException("A prepared statement runs its own SQL: call executeQuery() without arguments.", "HY000");
    }

    @Override
    public boolean execute(String sql) throws SQLException {
        throw new SQLException("A prepared statement runs its own SQL: call execute() without arguments.", "HY000");
    }

    /** Not known before the statement runs; run it and use the result's metadata. */
    @Override
    public ResultSetMetaData getMetaData() throws SQLException {
        check();
        return null;
    }

    @Override
    public void clearParameters() throws SQLException {
        check();
        Arrays.fill(params, null);
        Arrays.fill(set, false);
    }

    @Override
    public void setNull(int index, int sqlType) throws SQLException {
        put(index, null);
    }

    @Override
    public void setNull(int index, int sqlType, String typeName) throws SQLException {
        put(index, null);
    }

    @Override
    public void setBoolean(int index, boolean x) throws SQLException {
        put(index, x);
    }

    @Override
    public void setByte(int index, byte x) throws SQLException {
        put(index, (long) x);
    }

    @Override
    public void setShort(int index, short x) throws SQLException {
        put(index, (long) x);
    }

    @Override
    public void setInt(int index, int x) throws SQLException {
        put(index, (long) x);
    }

    @Override
    public void setLong(int index, long x) throws SQLException {
        put(index, x);
    }

    @Override
    public void setFloat(int index, float x) throws SQLException {
        put(index, (double) x);
    }

    @Override
    public void setDouble(int index, double x) throws SQLException {
        put(index, x);
    }

    @Override
    public void setBigDecimal(int index, BigDecimal x) throws SQLException {
        put(index, x);
    }

    @Override
    public void setString(int index, String x) throws SQLException {
        put(index, x);
    }

    @Override
    public void setNString(int index, String x) throws SQLException {
        put(index, x);
    }

    @Override
    public void setDate(int index, Date x) throws SQLException {
        put(index, x == null ? null : x.toLocalDate().toString());
    }

    @Override
    public void setTime(int index, Time x) throws SQLException {
        put(index, x == null ? null : x.toLocalTime().toString());
    }

    @Override
    public void setTimestamp(int index, Timestamp x) throws SQLException {
        put(index, x == null ? null : DateTimeFormatter.ISO_INSTANT.format(x.toInstant()));
    }

    @Override
    public void setDate(int index, Date x, Calendar calendar) throws SQLException {
        setDate(index, x);
    }

    @Override
    public void setTime(int index, Time x, Calendar calendar) throws SQLException {
        setTime(index, x);
    }

    @Override
    public void setTimestamp(int index, Timestamp x, Calendar calendar) throws SQLException {
        setTimestamp(index, x);
    }

    @Override
    public void setCharacterStream(int index, Reader reader, int length) throws SQLException {
        setCharacterStream(index, reader);
    }

    @Override
    public void setCharacterStream(int index, Reader reader, long length) throws SQLException {
        setCharacterStream(index, reader);
    }

    @Override
    public void setCharacterStream(int index, Reader reader) throws SQLException {
        if (reader == null) {
            put(index, null);
            return;
        }
        StringBuilder out = new StringBuilder();
        char[] buffer = new char[8192];
        try {
            int n;
            while ((n = reader.read(buffer)) >= 0) {
                out.append(buffer, 0, n);
            }
        } catch (IOException e) {
            throw new SQLException("Couldn't read the parameter's text: " + e.getMessage(), "HY000", e);
        }
        put(index, out.toString());
    }

    @Override
    public void setObject(int index, Object x) throws SQLException {
        if (x == null || x instanceof String || x instanceof Boolean || x instanceof Long || x instanceof Double || x instanceof BigDecimal) {
            put(index, x);
        } else if (x instanceof Integer || x instanceof Short || x instanceof Byte) {
            put(index, ((Number) x).longValue());
        } else if (x instanceof Float) {
            put(index, ((Float) x).doubleValue());
        } else if (x instanceof BigInteger) {
            put(index, new BigDecimal((BigInteger) x));
        } else if (x instanceof Timestamp) {
            setTimestamp(index, (Timestamp) x);
        } else if (x instanceof Date) {
            setDate(index, (Date) x);
        } else if (x instanceof Time) {
            setTime(index, (Time) x);
        } else if (x instanceof LocalDate || x instanceof LocalTime || x instanceof LocalDateTime || x instanceof OffsetDateTime) {
            put(index, x.toString());
        } else if (x instanceof ZonedDateTime) {
            put(index, ((ZonedDateTime) x).toOffsetDateTime().toString());
        } else if (x instanceof Instant) {
            put(index, DateTimeFormatter.ISO_INSTANT.format((Instant) x));
        } else if (x instanceof Character) {
            put(index, x.toString());
        } else {
            throw new SQLException("Can't send a " + x.getClass().getName() + " as a parameter.", "HY105");
        }
    }

    @Override
    public void setObject(int index, Object x, int targetSqlType) throws SQLException {
        if (x != null && (targetSqlType == Types.VARCHAR || targetSqlType == Types.NVARCHAR || targetSqlType == Types.LONGVARCHAR || targetSqlType == Types.CHAR)) {
            put(index, x.toString());
            return;
        }
        setObject(index, x);
    }

    @Override
    public void setObject(int index, Object x, int targetSqlType, int scaleOrLength) throws SQLException {
        setObject(index, x, targetSqlType);
    }
}
