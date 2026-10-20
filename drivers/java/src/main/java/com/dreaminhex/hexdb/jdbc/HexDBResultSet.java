package com.dreaminhex.hexdb.jdbc;

import java.io.Reader;
import java.io.StringReader;
import java.math.BigDecimal;
import java.math.BigInteger;
import java.nio.charset.StandardCharsets;
import java.sql.Date;
import java.sql.ResultSet;
import java.sql.ResultSetMetaData;
import java.sql.SQLDataException;
import java.sql.SQLException;
import java.sql.SQLWarning;
import java.sql.Statement;
import java.sql.Time;
import java.sql.Timestamp;
import java.time.Instant;
import java.time.LocalDate;
import java.time.LocalDateTime;
import java.time.LocalTime;
import java.time.OffsetDateTime;
import java.time.ZoneOffset;
import java.time.format.DateTimeParseException;
import java.util.ArrayList;
import java.util.Calendar;
import java.util.List;
import java.util.Locale;
import java.util.Map;

/**
 * Rows of a statement, fetched a page at a time as the application reads
 * them, or a fixed set of rows (metadata). Forward-only and read-only.
 */
public class HexDBResultSet extends UnsupportedResultSet {
    /** Fetches the page after `cursor`. */
    interface Pager {
        Map<String, Object> page(String cursor) throws SQLException;
    }

    private final HexDBStatement statement;
    private final List<Column> columns;
    private List<List<Object>> rows;
    private int index = -1;
    private String next;
    private final Pager pager;
    private final long maxRows;
    private long position;
    private boolean closed;
    private boolean wasNull;
    private int fetchSize;

    /** A fixed result (metadata). */
    HexDBResultSet(HexDBStatement statement, List<Column> columns, List<List<Object>> rows) {
        this.statement = statement;
        this.columns = columns;
        this.rows = rows;
        this.pager = null;
        this.maxRows = 0;
    }

    /** A statement's result, starting with its first page. */
    HexDBResultSet(HexDBStatement statement, Map<String, Object> first, Pager pager, long maxRows) throws SQLException {
        this.statement = statement;
        this.pager = pager;
        this.maxRows = maxRows;
        this.columns = new ArrayList<>();
        Object cols = first.get("columns");
        if (cols instanceof List) {
            for (Object c : (List<?>) cols) {
                Map<?, ?> m = (Map<?, ?>) c;
                columns.add(Column.ofHexType(String.valueOf(m.get("name")), (String) m.get("type")));
            }
        }
        take(first);
    }

    @SuppressWarnings("unchecked")
    private void take(Map<String, Object> page) {
        Object r = page.get("rows");
        rows = r instanceof List ? (List<List<Object>>) r : new ArrayList<>();
        Object n = page.get("next");
        next = n instanceof String ? (String) n : null;
        index = -1;
    }

    private void check() throws SQLException {
        if (closed) {
            throw Unsupported.closed("result set");
        }
    }

    List<Column> columns() {
        return columns;
    }

    // -- Moving --------------------------------------------------------------

    @Override
    public boolean next() throws SQLException {
        check();
        if (maxRows > 0 && position >= maxRows) {
            index = rows.size();
            next = null;
            position = maxRows + 1;
            return false;
        }
        while (index + 1 >= rows.size()) {
            if (next == null || pager == null) {
                index = rows.size();
                position++;
                return false;
            }
            take(pager.page(next));
        }
        index++;
        position++;
        return true;
    }

    private List<Object> row() throws SQLException {
        check();
        if (index < 0 || index >= rows.size()) {
            throw new SQLException("No current row: call next() first.", "24000");
        }
        return rows.get(index);
    }

    @Override
    public boolean isBeforeFirst() throws SQLException {
        check();
        return position == 0 && !(rows.isEmpty() && next == null);
    }

    @Override
    public boolean isAfterLast() throws SQLException {
        check();
        return position > 0 && (index >= rows.size()) && next == null;
    }

    @Override
    public boolean isFirst() throws SQLException {
        check();
        return position == 1 && index >= 0 && index < rows.size();
    }

    @Override
    public boolean isLast() throws SQLException {
        check();
        if (index < 0 || index >= rows.size()) {
            return false;
        }
        if (maxRows > 0 && position >= maxRows) {
            return true;
        }
        if (index + 1 < rows.size()) {
            return false;
        }
        // Look ahead: fetch the next page, keeping the current row.
        while (next != null && pager != null) {
            List<Object> current = rows.get(index);
            Map<String, Object> page = pager.page(next);
            take(page);
            if (!rows.isEmpty()) {
                List<List<Object>> withCurrent = new ArrayList<>(rows.size() + 1);
                withCurrent.add(current);
                withCurrent.addAll(rows);
                rows = withCurrent;
                index = 0;
                return false;
            }
            List<List<Object>> only = new ArrayList<>();
            only.add(current);
            rows = only;
            index = 0;
        }
        return true;
    }

    @Override
    public int getRow() throws SQLException {
        check();
        return index >= 0 && index < rows.size() ? (int) Math.min(position, Integer.MAX_VALUE) : 0;
    }

    @Override
    public void close() throws SQLException {
        if (closed) {
            return;
        }
        closed = true;
        rows = new ArrayList<>();
        if (statement != null) {
            statement.resultClosed();
        }
    }

    /** Close without closing the statement (it's running another query). */
    void discard() {
        closed = true;
        rows = new ArrayList<>();
    }

    @Override
    public boolean isClosed() {
        return closed;
    }

    @Override
    public int getType() {
        return ResultSet.TYPE_FORWARD_ONLY;
    }

    @Override
    public int getConcurrency() {
        return ResultSet.CONCUR_READ_ONLY;
    }

    @Override
    public int getHoldability() {
        return ResultSet.CLOSE_CURSORS_AT_COMMIT;
    }

    @Override
    public int getFetchDirection() {
        return ResultSet.FETCH_FORWARD;
    }

    @Override
    public void setFetchDirection(int direction) throws SQLException {
        if (direction != ResultSet.FETCH_FORWARD) {
            throw Unsupported.feature("ResultSet.setFetchDirection (only FETCH_FORWARD)");
        }
    }

    @Override
    public int getFetchSize() {
        return fetchSize;
    }

    @Override
    public void setFetchSize(int rows) throws SQLException {
        if (rows < 0) {
            throw new SQLException("The fetch size can't be negative.", "HY024");
        }
        fetchSize = rows;
    }

    @Override
    public Statement getStatement() {
        return statement;
    }

    @Override
    public SQLWarning getWarnings() {
        return null;
    }

    @Override
    public void clearWarnings() {
    }

    @Override
    public ResultSetMetaData getMetaData() throws SQLException {
        check();
        return new HexDBResultSetMetaData(columns);
    }

    @Override
    public int findColumn(String label) throws SQLException {
        check();
        for (int i = 0; i < columns.size(); i++) {
            if (columns.get(i).name.equals(label)) {
                return i + 1;
            }
        }
        for (int i = 0; i < columns.size(); i++) {
            if (columns.get(i).name.equalsIgnoreCase(label)) {
                return i + 1;
            }
        }
        throw new SQLException("No column named " + label + ".", "42S22");
    }

    @Override
    public boolean wasNull() {
        return wasNull;
    }

    // -- Values --------------------------------------------------------------

    private Object value(int column) throws SQLException {
        List<Object> row = row();
        if (column < 1 || column > columns.size()) {
            throw new SQLException("Column " + column + " is out of range: the result has " + columns.size() + ".", "07009");
        }
        Object v = column <= row.size() ? row.get(column - 1) : null;
        wasNull = v == null;
        return v;
    }

    private static SQLException cant(Object v, String type) {
        return new SQLDataException("Can't read " + Json.write(v) + " as " + type + ".", "22018");
    }

    private Number numberOf(int column, String type) throws SQLException {
        Object v = value(column);
        if (v == null) {
            return null;
        }
        if (v instanceof Number) {
            return (Number) v;
        }
        if (v instanceof Boolean) {
            return ((Boolean) v) ? 1L : 0L;
        }
        if (v instanceof String) {
            try {
                return new BigDecimal(((String) v).trim());
            } catch (NumberFormatException e) {
                throw cant(v, type);
            }
        }
        throw cant(v, type);
    }

    private long integral(int column, String type, long min, long max) throws SQLException {
        Number n = numberOf(column, type);
        if (n == null) {
            return 0;
        }
        BigDecimal d = n instanceof BigDecimal ? (BigDecimal) n : new BigDecimal(n.toString());
        BigDecimal whole = d.setScale(0, java.math.RoundingMode.DOWN);
        if (whole.compareTo(BigDecimal.valueOf(min)) < 0 || whole.compareTo(BigDecimal.valueOf(max)) > 0) {
            throw new SQLDataException(n + " doesn't fit in " + type + ".", "22003");
        }
        return whole.longValue();
    }

    @Override
    public String getString(int column) throws SQLException {
        Object v = value(column);
        if (v == null || v instanceof String) {
            return (String) v;
        }
        if (v instanceof Map || v instanceof List) {
            return Json.write(v);
        }
        if (v instanceof Double) {
            double d = (Double) v;
            if (d == Math.rint(d) && Math.abs(d) < 1e15) {
                return BigDecimal.valueOf(d).setScale(1).toPlainString();
            }
        }
        return v.toString();
    }

    @Override
    public boolean getBoolean(int column) throws SQLException {
        Object v = value(column);
        if (v == null) {
            return false;
        }
        if (v instanceof Boolean) {
            return (Boolean) v;
        }
        if (v instanceof Number) {
            return ((Number) v).doubleValue() != 0;
        }
        if (v instanceof String) {
            switch (((String) v).trim().toLowerCase(Locale.ROOT)) {
                case "true":
                case "t":
                case "yes":
                case "y":
                case "1":
                    return true;
                case "false":
                case "f":
                case "no":
                case "n":
                case "0":
                    return false;
                default:
                    break;
            }
        }
        throw cant(v, "a boolean");
    }

    @Override
    public byte getByte(int column) throws SQLException {
        return (byte) integral(column, "a byte", Byte.MIN_VALUE, Byte.MAX_VALUE);
    }

    @Override
    public short getShort(int column) throws SQLException {
        return (short) integral(column, "a short", Short.MIN_VALUE, Short.MAX_VALUE);
    }

    @Override
    public int getInt(int column) throws SQLException {
        return (int) integral(column, "an int", Integer.MIN_VALUE, Integer.MAX_VALUE);
    }

    @Override
    public long getLong(int column) throws SQLException {
        return integral(column, "a long", Long.MIN_VALUE, Long.MAX_VALUE);
    }

    @Override
    public float getFloat(int column) throws SQLException {
        Number n = numberOf(column, "a float");
        return n == null ? 0 : n.floatValue();
    }

    @Override
    public double getDouble(int column) throws SQLException {
        Number n = numberOf(column, "a double");
        return n == null ? 0 : n.doubleValue();
    }

    @Override
    public BigDecimal getBigDecimal(int column) throws SQLException {
        Number n = numberOf(column, "a decimal");
        if (n == null) {
            return null;
        }
        if (n instanceof BigDecimal) {
            return (BigDecimal) n;
        }
        if (n instanceof Double || n instanceof Float) {
            return BigDecimal.valueOf(n.doubleValue());
        }
        return new BigDecimal(n.toString());
    }

    @Override
    @SuppressWarnings("deprecation")
    public BigDecimal getBigDecimal(int column, int scale) throws SQLException {
        BigDecimal d = getBigDecimal(column);
        return d == null ? null : d.setScale(scale, java.math.RoundingMode.HALF_UP);
    }

    @Override
    public byte[] getBytes(int column) throws SQLException {
        String s = getString(column);
        return s == null ? null : s.getBytes(StandardCharsets.UTF_8);
    }

    @Override
    public Reader getCharacterStream(int column) throws SQLException {
        String s = getString(column);
        return s == null ? null : new StringReader(s);
    }

    @Override
    public String getNString(int column) throws SQLException {
        return getString(column);
    }

    @Override
    public Reader getNCharacterStream(int column) throws SQLException {
        return getCharacterStream(column);
    }

    // -- Dates and times (ISO 8601 strings in HexDB) ---------------------------

    /** A date/time string as an instant (with an offset) or a local date-time. */
    private Object temporal(int column) throws SQLException {
        Object v = value(column);
        if (v == null) {
            return null;
        }
        if (!(v instanceof String)) {
            throw cant(v, "a date or time");
        }
        String s = ((String) v).trim();
        try {
            return OffsetDateTime.parse(s);
        } catch (DateTimeParseException ignored) {
            // try the next form
        }
        try {
            return LocalDateTime.parse(s.replace(' ', 'T'));
        } catch (DateTimeParseException ignored) {
            // try the next form
        }
        try {
            return LocalDate.parse(s);
        } catch (DateTimeParseException ignored) {
            // try the next form
        }
        try {
            return LocalTime.parse(s);
        } catch (DateTimeParseException e) {
            throw cant(v, "a date or time (ISO 8601)");
        }
    }

    private LocalDateTime localDateTime(int column) throws SQLException {
        Object t = temporal(column);
        if (t == null) {
            return null;
        }
        if (t instanceof OffsetDateTime) {
            return ((OffsetDateTime) t).atZoneSameInstant(java.time.ZoneId.systemDefault()).toLocalDateTime();
        }
        if (t instanceof LocalDate) {
            return ((LocalDate) t).atStartOfDay();
        }
        if (t instanceof LocalTime) {
            return ((LocalTime) t).atDate(LocalDate.of(1970, 1, 1));
        }
        return (LocalDateTime) t;
    }

    @Override
    public Timestamp getTimestamp(int column) throws SQLException {
        Object t = temporal(column);
        if (t instanceof OffsetDateTime) {
            return Timestamp.from(((OffsetDateTime) t).toInstant());
        }
        LocalDateTime local = localDateTime(column);
        return local == null ? null : Timestamp.valueOf(local);
    }

    @Override
    public Date getDate(int column) throws SQLException {
        Object t = temporal(column);
        if (t == null) {
            return null;
        }
        if (t instanceof OffsetDateTime) {
            return Date.valueOf(((OffsetDateTime) t).toLocalDate());
        }
        if (t instanceof LocalTime) {
            throw cant(value(column), "a date");
        }
        return Date.valueOf(localDateTime(column).toLocalDate());
    }

    @Override
    public Time getTime(int column) throws SQLException {
        Object t = temporal(column);
        if (t == null) {
            return null;
        }
        if (t instanceof OffsetDateTime) {
            return Time.valueOf(((OffsetDateTime) t).toLocalTime());
        }
        return Time.valueOf(localDateTime(column).toLocalTime());
    }

    @Override
    public Timestamp getTimestamp(int column, Calendar calendar) throws SQLException {
        Object t = temporal(column);
        if (t instanceof OffsetDateTime || t == null || calendar == null) {
            return getTimestamp(column);
        }
        LocalDateTime local = localDateTime(column);
        return Timestamp.from(local.atZone(calendar.getTimeZone().toZoneId()).toInstant());
    }

    @Override
    public Date getDate(int column, Calendar calendar) throws SQLException {
        return getDate(column);
    }

    @Override
    public Time getTime(int column, Calendar calendar) throws SQLException {
        return getTime(column);
    }

    // -- Objects -------------------------------------------------------------

    @Override
    public Object getObject(int column) throws SQLException {
        Object v = value(column);
        if (v == null) {
            return null;
        }
        Column c = columns.get(column - 1);
        switch (c.sqlType) {
            case java.sql.Types.BIGINT:
                return v instanceof Number && !(v instanceof Double) ? (Object) ((Number) v).longValue() : v;
            case java.sql.Types.INTEGER:
                return v instanceof Number ? (Object) ((Number) v).intValue() : v;
            case java.sql.Types.SMALLINT:
                return v instanceof Number ? (Object) ((Number) v).shortValue() : v;
            case java.sql.Types.DOUBLE:
                return v instanceof Number ? (Object) ((Number) v).doubleValue() : v;
            case java.sql.Types.LONGVARCHAR:
            case java.sql.Types.VARCHAR:
                return v instanceof Map || v instanceof List ? Json.write(v) : v;
            default:
                return v;
        }
    }

    @Override
    public Object getObject(int column, Map<String, Class<?>> map) throws SQLException {
        return getObject(column);
    }

    @Override
    public <T> T getObject(int column, Class<T> type) throws SQLException {
        if (type == null) {
            throw new SQLException("Give a type.", "HY009");
        }
        Object v = value(column);
        if (v == null) {
            return null;
        }
        Object out;
        if (type == String.class) {
            out = getString(column);
        } else if (type == Long.class) {
            out = getLong(column);
        } else if (type == Integer.class) {
            out = getInt(column);
        } else if (type == Short.class) {
            out = getShort(column);
        } else if (type == Byte.class) {
            out = getByte(column);
        } else if (type == Double.class) {
            out = getDouble(column);
        } else if (type == Float.class) {
            out = getFloat(column);
        } else if (type == Boolean.class) {
            out = getBoolean(column);
        } else if (type == BigDecimal.class) {
            out = getBigDecimal(column);
        } else if (type == BigInteger.class) {
            BigDecimal d = getBigDecimal(column);
            out = d == null ? null : d.toBigInteger();
        } else if (type == Timestamp.class) {
            out = getTimestamp(column);
        } else if (type == Date.class) {
            out = getDate(column);
        } else if (type == Time.class) {
            out = getTime(column);
        } else if (type == LocalDateTime.class) {
            Object t = temporal(column);
            out = t instanceof OffsetDateTime ? ((OffsetDateTime) t).toLocalDateTime() : localDateTime(column);
        } else if (type == LocalDate.class) {
            Object t = temporal(column);
            out = t instanceof OffsetDateTime ? ((OffsetDateTime) t).toLocalDate() : localDateTime(column).toLocalDate();
        } else if (type == LocalTime.class) {
            Object t = temporal(column);
            out = t instanceof OffsetDateTime ? ((OffsetDateTime) t).toLocalTime() : localDateTime(column).toLocalTime();
        } else if (type == OffsetDateTime.class) {
            Object t = temporal(column);
            out = t instanceof OffsetDateTime ? t : localDateTime(column).atOffset(ZoneOffset.UTC);
        } else if (type == Instant.class) {
            Object t = temporal(column);
            out = t instanceof OffsetDateTime ? ((OffsetDateTime) t).toInstant() : localDateTime(column).toInstant(ZoneOffset.UTC);
        } else if (type == Object.class) {
            out = getObject(column);
        } else if (type.isInstance(v)) {
            out = v;
        } else {
            throw new SQLDataException("Can't read column " + column + " as " + type.getName() + ".", "22018");
        }
        return type.cast(out);
    }

    // -- By label ------------------------------------------------------------

    @Override
    public String getString(String label) throws SQLException {
        return getString(findColumn(label));
    }

    @Override
    public boolean getBoolean(String label) throws SQLException {
        return getBoolean(findColumn(label));
    }

    @Override
    public byte getByte(String label) throws SQLException {
        return getByte(findColumn(label));
    }

    @Override
    public short getShort(String label) throws SQLException {
        return getShort(findColumn(label));
    }

    @Override
    public int getInt(String label) throws SQLException {
        return getInt(findColumn(label));
    }

    @Override
    public long getLong(String label) throws SQLException {
        return getLong(findColumn(label));
    }

    @Override
    public float getFloat(String label) throws SQLException {
        return getFloat(findColumn(label));
    }

    @Override
    public double getDouble(String label) throws SQLException {
        return getDouble(findColumn(label));
    }

    @Override
    public BigDecimal getBigDecimal(String label) throws SQLException {
        return getBigDecimal(findColumn(label));
    }

    @Override
    @SuppressWarnings("deprecation")
    public BigDecimal getBigDecimal(String label, int scale) throws SQLException {
        return getBigDecimal(findColumn(label), scale);
    }

    @Override
    public byte[] getBytes(String label) throws SQLException {
        return getBytes(findColumn(label));
    }

    @Override
    public Date getDate(String label) throws SQLException {
        return getDate(findColumn(label));
    }

    @Override
    public Time getTime(String label) throws SQLException {
        return getTime(findColumn(label));
    }

    @Override
    public Timestamp getTimestamp(String label) throws SQLException {
        return getTimestamp(findColumn(label));
    }

    @Override
    public Date getDate(String label, Calendar calendar) throws SQLException {
        return getDate(findColumn(label), calendar);
    }

    @Override
    public Time getTime(String label, Calendar calendar) throws SQLException {
        return getTime(findColumn(label), calendar);
    }

    @Override
    public Timestamp getTimestamp(String label, Calendar calendar) throws SQLException {
        return getTimestamp(findColumn(label), calendar);
    }

    @Override
    public Object getObject(String label) throws SQLException {
        return getObject(findColumn(label));
    }

    @Override
    public Object getObject(String label, Map<String, Class<?>> map) throws SQLException {
        return getObject(findColumn(label), map);
    }

    @Override
    public <T> T getObject(String label, Class<T> type) throws SQLException {
        return getObject(findColumn(label), type);
    }

    @Override
    public Reader getCharacterStream(String label) throws SQLException {
        return getCharacterStream(findColumn(label));
    }

    @Override
    public String getNString(String label) throws SQLException {
        return getNString(findColumn(label));
    }

    @Override
    public Reader getNCharacterStream(String label) throws SQLException {
        return getNCharacterStream(findColumn(label));
    }
}
