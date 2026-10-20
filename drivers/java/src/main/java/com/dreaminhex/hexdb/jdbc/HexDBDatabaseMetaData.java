package com.dreaminhex.hexdb.jdbc;

import java.sql.Connection;
import java.sql.DatabaseMetaData;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.sql.Types;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/**
 * What HexDB's SQL supports, and its tables and columns. HexDB has no
 * catalogs or schemas: every tessellation the caller can read is a table.
 */
public class HexDBDatabaseMetaData extends UnsupportedDatabaseMetaData {
    private final HexDBConnection connection;

    HexDBDatabaseMetaData(HexDBConnection connection) {
        this.connection = connection;
    }

    // -- Result helpers ------------------------------------------------------

    /** Columns from specs: "NAME" (text), "NAME:i" (int), "NAME:s" (smallint), "NAME:b" (boolean). */
    private static List<Column> columns(String... specs) {
        List<Column> out = new ArrayList<>();
        for (String spec : specs) {
            int colon = spec.indexOf(':');
            String name = colon < 0 ? spec : spec.substring(0, colon);
            char kind = colon < 0 ? 'v' : spec.charAt(colon + 1);
            int type = kind == 'i' ? Types.INTEGER : kind == 's' ? Types.SMALLINT : kind == 'b' ? Types.BOOLEAN : Types.VARCHAR;
            out.add(Column.of(name, type));
        }
        return out;
    }

    private ResultSet result(List<Column> columns, List<List<Object>> rows) {
        return new HexDBResultSet(null, columns, rows);
    }

    private static final Map<String, String[]> EMPTY = new HashMap<>();

    static {
        String[] keys = {"PKTABLE_CAT", "PKTABLE_SCHEM", "PKTABLE_NAME", "PKCOLUMN_NAME", "FKTABLE_CAT", "FKTABLE_SCHEM", "FKTABLE_NAME", "FKCOLUMN_NAME",
            "KEY_SEQ:s", "UPDATE_RULE:s", "DELETE_RULE:s", "FK_NAME", "PK_NAME", "DEFERRABILITY:s"};
        EMPTY.put("getImportedKeys", keys);
        EMPTY.put("getExportedKeys", keys);
        EMPTY.put("getCrossReference", keys);
        EMPTY.put("getProcedures", new String[] {"PROCEDURE_CAT", "PROCEDURE_SCHEM", "PROCEDURE_NAME", "RESERVED1", "RESERVED2", "RESERVED3", "REMARKS",
            "PROCEDURE_TYPE:s", "SPECIFIC_NAME"});
        EMPTY.put("getProcedureColumns", new String[] {"PROCEDURE_CAT", "PROCEDURE_SCHEM", "PROCEDURE_NAME", "COLUMN_NAME", "COLUMN_TYPE:s", "DATA_TYPE:i",
            "TYPE_NAME", "PRECISION:i", "LENGTH:i", "SCALE:s", "RADIX:s", "NULLABLE:s", "REMARKS", "COLUMN_DEF", "SQL_DATA_TYPE:i", "SQL_DATETIME_SUB:i",
            "CHAR_OCTET_LENGTH:i", "ORDINAL_POSITION:i", "IS_NULLABLE", "SPECIFIC_NAME"});
        EMPTY.put("getFunctions", new String[] {"FUNCTION_CAT", "FUNCTION_SCHEM", "FUNCTION_NAME", "REMARKS", "FUNCTION_TYPE:s", "SPECIFIC_NAME"});
        EMPTY.put("getFunctionColumns", new String[] {"FUNCTION_CAT", "FUNCTION_SCHEM", "FUNCTION_NAME", "COLUMN_NAME", "COLUMN_TYPE:s", "DATA_TYPE:i",
            "TYPE_NAME", "PRECISION:i", "LENGTH:i", "SCALE:s", "RADIX:s", "NULLABLE:s", "REMARKS", "CHAR_OCTET_LENGTH:i", "ORDINAL_POSITION:i", "IS_NULLABLE",
            "SPECIFIC_NAME"});
        EMPTY.put("getColumnPrivileges", new String[] {"TABLE_CAT", "TABLE_SCHEM", "TABLE_NAME", "COLUMN_NAME", "GRANTOR", "GRANTEE", "PRIVILEGE", "IS_GRANTABLE"});
        EMPTY.put("getTablePrivileges", new String[] {"TABLE_CAT", "TABLE_SCHEM", "TABLE_NAME", "GRANTOR", "GRANTEE", "PRIVILEGE", "IS_GRANTABLE"});
        EMPTY.put("getVersionColumns", new String[] {"SCOPE:s", "COLUMN_NAME", "DATA_TYPE:i", "TYPE_NAME", "COLUMN_SIZE:i", "BUFFER_LENGTH:i", "DECIMAL_DIGITS:s",
            "PSEUDO_COLUMN:s"});
        EMPTY.put("getIndexInfo", new String[] {"TABLE_CAT", "TABLE_SCHEM", "TABLE_NAME", "NON_UNIQUE:b", "INDEX_QUALIFIER", "INDEX_NAME", "TYPE:s",
            "ORDINAL_POSITION:s", "COLUMN_NAME", "ASC_OR_DESC", "CARDINALITY:i", "PAGES:i", "FILTER_CONDITION"});
        EMPTY.put("getUDTs", new String[] {"TYPE_CAT", "TYPE_SCHEM", "TYPE_NAME", "CLASS_NAME", "DATA_TYPE:i", "REMARKS", "BASE_TYPE:s"});
        EMPTY.put("getSuperTypes", new String[] {"TYPE_CAT", "TYPE_SCHEM", "TYPE_NAME", "SUPERTYPE_CAT", "SUPERTYPE_SCHEM", "SUPERTYPE_NAME"});
        EMPTY.put("getSuperTables", new String[] {"TABLE_CAT", "TABLE_SCHEM", "TABLE_NAME", "SUPERTABLE_NAME"});
        EMPTY.put("getAttributes", new String[] {"TYPE_CAT", "TYPE_SCHEM", "TYPE_NAME", "ATTR_NAME", "DATA_TYPE:i", "ATTR_TYPE_NAME", "ATTR_SIZE:i",
            "DECIMAL_DIGITS:i", "NUM_PREC_RADIX:i", "NULLABLE:i", "REMARKS", "ATTR_DEF", "SQL_DATA_TYPE:i", "SQL_DATETIME_SUB:i", "CHAR_OCTET_LENGTH:i",
            "ORDINAL_POSITION:i", "IS_NULLABLE", "SCOPE_CATALOG", "SCOPE_SCHEMA", "SCOPE_TABLE", "SOURCE_DATA_TYPE:s"});
        EMPTY.put("getClientInfoProperties", new String[] {"NAME", "MAX_LEN:i", "DEFAULT_VALUE", "DESCRIPTION"});
        EMPTY.put("getPseudoColumns", new String[] {"TABLE_CAT", "TABLE_SCHEM", "TABLE_NAME", "COLUMN_NAME", "DATA_TYPE:i", "COLUMN_SIZE:i", "DECIMAL_DIGITS:i",
            "NUM_PREC_RADIX:i", "COLUMN_USAGE", "REMARKS", "CHAR_OCTET_LENGTH:i", "IS_NULLABLE"});
    }

    @Override
    protected ResultSet empty(String method) throws SQLException {
        connection.check();
        String[] specs = EMPTY.get(method);
        if (specs == null) {
            throw Unsupported.feature("DatabaseMetaData." + method);
        }
        return result(columns(specs), new ArrayList<>());
    }

    // -- Tables and columns --------------------------------------------------

    @Override
    public ResultSet getTables(String catalog, String schemaPattern, String tableNamePattern, String[] types) throws SQLException {
        connection.check();
        List<Column> cols = columns("TABLE_CAT", "TABLE_SCHEM", "TABLE_NAME", "TABLE_TYPE", "REMARKS", "TYPE_CAT", "TYPE_SCHEM", "TYPE_NAME",
            "SELF_REFERENCING_COL_NAME", "REF_GENERATION");
        List<List<Object>> rows = new ArrayList<>();
        boolean tables = types == null || Arrays.stream(types).anyMatch(t -> "TABLE".equalsIgnoreCase(t));
        if (tables) {
            List<String> names = connection.client.tables();
            names.sort(String::compareTo);
            for (String name : names) {
                if (Escapes.matches(tableNamePattern, name)) {
                    rows.add(Arrays.asList(null, null, name, "TABLE", "", null, null, null, null, null));
                }
            }
        }
        return result(cols, rows);
    }

    @Override
    public ResultSet getColumns(String catalog, String schemaPattern, String tableNamePattern, String columnNamePattern) throws SQLException {
        connection.check();
        List<Column> cols = columns("TABLE_CAT", "TABLE_SCHEM", "TABLE_NAME", "COLUMN_NAME", "DATA_TYPE:i", "TYPE_NAME", "COLUMN_SIZE:i", "BUFFER_LENGTH:i",
            "DECIMAL_DIGITS:i", "NUM_PREC_RADIX:i", "NULLABLE:i", "REMARKS", "COLUMN_DEF", "SQL_DATA_TYPE:i", "SQL_DATETIME_SUB:i", "CHAR_OCTET_LENGTH:i",
            "ORDINAL_POSITION:i", "IS_NULLABLE", "SCOPE_CATALOG", "SCOPE_SCHEMA", "SCOPE_TABLE", "SOURCE_DATA_TYPE:s", "IS_AUTOINCREMENT",
            "IS_GENERATEDCOLUMN");
        List<List<Object>> rows = new ArrayList<>();
        List<String> names = connection.client.tables();
        names.sort(String::compareTo);
        for (String table : names) {
            if (!Escapes.matches(tableNamePattern, table)) {
                continue;
            }
            int position = 0;
            for (Map<String, Object> c : connection.client.columns(table)) {
                position++;
                String name = String.valueOf(c.get("name"));
                if (!Escapes.matches(columnNamePattern, name)) {
                    continue;
                }
                Column column = Column.ofHexType(name, (String) c.get("type"));
                boolean nullable = !Boolean.FALSE.equals(c.get("nullable"));
                boolean numeric = column.sqlType == Types.BIGINT || column.sqlType == Types.DOUBLE;
                boolean text = column.sqlType == Types.VARCHAR || column.sqlType == Types.LONGVARCHAR;
                // Boxed on purpose: a ternary mixing int and null would unbox the null.
                Integer digits = column.sqlType == Types.DOUBLE ? Integer.valueOf(15) : numeric ? Integer.valueOf(0) : null;
                Integer radix = numeric ? Integer.valueOf(10) : null;
                Integer octets = text ? Integer.valueOf(column.precision()) : null;
                rows.add(Arrays.asList(null, null, table, name, column.sqlType, column.typeName, column.precision(), null, digits, radix,
                    nullable ? DatabaseMetaData.columnNullable : DatabaseMetaData.columnNoNulls, String.valueOf(c.getOrDefault("source", "")), null,
                    column.sqlType, null, octets, position, nullable ? "YES" : "NO", null, null, null, null, "NO", "NO"));
            }
        }
        return result(cols, rows);
    }

    @Override
    public ResultSet getPrimaryKeys(String catalog, String schema, String table) throws SQLException {
        connection.check();
        List<Column> cols = columns("TABLE_CAT", "TABLE_SCHEM", "TABLE_NAME", "COLUMN_NAME", "KEY_SEQ:s", "PK_NAME");
        List<List<Object>> rows = new ArrayList<>();
        if (table != null && connection.client.tables().contains(table)) {
            rows.add(Arrays.asList(null, null, table, "id", (short) 1, "id"));
        }
        return result(cols, rows);
    }

    @Override
    public ResultSet getBestRowIdentifier(String catalog, String schema, String table, int scope, boolean nullable) throws SQLException {
        connection.check();
        List<Column> cols = columns("SCOPE:s", "COLUMN_NAME", "DATA_TYPE:i", "TYPE_NAME", "COLUMN_SIZE:i", "BUFFER_LENGTH:i", "DECIMAL_DIGITS:s", "PSEUDO_COLUMN:s");
        List<List<Object>> rows = new ArrayList<>();
        if (table != null && connection.client.tables().contains(table)) {
            rows.add(Arrays.asList((short) DatabaseMetaData.bestRowSession, "id", Types.VARCHAR, "VARCHAR", 26, null, null,
                (short) DatabaseMetaData.bestRowNotPseudo));
        }
        return result(cols, rows);
    }

    @Override
    public ResultSet getTableTypes() throws SQLException {
        connection.check();
        List<List<Object>> rows = new ArrayList<>();
        rows.add(Arrays.asList("TABLE"));
        return result(columns("TABLE_TYPE"), rows);
    }

    @Override
    public ResultSet getSchemas() throws SQLException {
        connection.check();
        return result(columns("TABLE_SCHEM", "TABLE_CATALOG"), new ArrayList<>());
    }

    @Override
    public ResultSet getSchemas(String catalog, String schemaPattern) throws SQLException {
        return getSchemas();
    }

    @Override
    public ResultSet getCatalogs() throws SQLException {
        connection.check();
        return result(columns("TABLE_CAT"), new ArrayList<>());
    }

    @Override
    public ResultSet getTypeInfo() throws SQLException {
        connection.check();
        List<Column> cols = columns("TYPE_NAME", "DATA_TYPE:i", "PRECISION:i", "LITERAL_PREFIX", "LITERAL_SUFFIX", "CREATE_PARAMS", "NULLABLE:s",
            "CASE_SENSITIVE:b", "SEARCHABLE:s", "UNSIGNED_ATTRIBUTE:b", "FIXED_PREC_SCALE:b", "AUTO_INCREMENT:b", "LOCAL_TYPE_NAME", "MINIMUM_SCALE:s",
            "MAXIMUM_SCALE:s", "SQL_DATA_TYPE:i", "SQL_DATETIME_SUB:i", "NUM_PREC_RADIX:i");
        List<List<Object>> rows = new ArrayList<>();
        short nullable = (short) DatabaseMetaData.typeNullable;
        short searchable = (short) DatabaseMetaData.typeSearchable;
        rows.add(Arrays.asList("BIGINT", Types.BIGINT, 19, null, null, null, nullable, false, searchable, false, false, false, "BIGINT", (short) 0, (short) 0,
            Types.BIGINT, null, 10));
        rows.add(Arrays.asList("JSON", Types.LONGVARCHAR, Integer.MAX_VALUE, "'", "'", null, nullable, true, searchable, true, false, false, "JSON", null, null,
            Types.LONGVARCHAR, null, null));
        rows.add(Arrays.asList("DOUBLE", Types.DOUBLE, 15, null, null, null, nullable, false, searchable, false, false, false, "DOUBLE", (short) 0, (short) 15,
            Types.DOUBLE, null, 10));
        rows.add(Arrays.asList("VARCHAR", Types.VARCHAR, 65535, "'", "'", null, nullable, true, searchable, true, false, false, "VARCHAR", null, null,
            Types.VARCHAR, null, null));
        rows.add(Arrays.asList("BOOLEAN", Types.BOOLEAN, 1, null, null, null, nullable, false, searchable, true, false, false, "BOOLEAN", null, null,
            Types.BOOLEAN, null, null));
        return result(cols, rows);
    }

    // -- About the server and driver -----------------------------------------

    @Override
    public Connection getConnection() throws SQLException {
        connection.check();
        return connection;
    }

    @Override
    public String getURL() {
        return connection.client.url;
    }

    @Override
    public String getUserName() {
        return connection.client.login;
    }

    @Override
    public String getDatabaseProductName() {
        return "HexDB";
    }

    @Override
    public String getDatabaseProductVersion() {
        return connection.client.serverVersion;
    }

    @Override
    public int getDatabaseMajorVersion() {
        return HexDBDriver.part(connection.client.serverVersion, 0);
    }

    @Override
    public int getDatabaseMinorVersion() {
        return HexDBDriver.part(connection.client.serverVersion, 1);
    }

    @Override
    public String getDriverName() {
        return "HexDB JDBC";
    }

    @Override
    public String getDriverVersion() {
        return HexDBDriver.VERSION;
    }

    @Override
    public int getDriverMajorVersion() {
        return HexDBDriver.part(HexDBDriver.VERSION, 0);
    }

    @Override
    public int getDriverMinorVersion() {
        return HexDBDriver.part(HexDBDriver.VERSION, 1);
    }

    @Override
    public int getJDBCMajorVersion() {
        return 4;
    }

    @Override
    public int getJDBCMinorVersion() {
        return 2;
    }

    @Override
    public boolean isReadOnly() {
        return true;
    }

    @Override
    public boolean allTablesAreSelectable() {
        return true;
    }

    @Override
    public boolean allProceduresAreCallable() {
        return true;
    }

    @Override
    public String getIdentifierQuoteString() {
        return "\"";
    }

    @Override
    public String getSearchStringEscape() {
        return "\\";
    }

    @Override
    public String getCatalogTerm() {
        return "catalog";
    }

    @Override
    public String getSchemaTerm() {
        return "schema";
    }

    @Override
    public String getProcedureTerm() {
        return "procedure";
    }

    @Override
    public String getCatalogSeparator() {
        return ".";
    }

    @Override
    public String getNumericFunctions() {
        return "ABS,CEILING,EXP,FLOOR,LOG,LOG10,MOD,PI,POWER,ROUND,SIGN,SQRT,TRUNCATE";
    }

    @Override
    public String getStringFunctions() {
        return "CHAR_LENGTH,CHARACTER_LENGTH,CONCAT,LCASE,LEFT,LENGTH,LOCATE,LTRIM,POSITION,REPLACE,RIGHT,RTRIM,SUBSTRING,UCASE";
    }

    @Override
    public String getSystemFunctions() {
        return "IFNULL";
    }

    @Override
    public String getTimeDateFunctions() {
        return "CURDATE,CURRENT_DATE,CURRENT_TIME,CURRENT_TIMESTAMP,CURTIME,DAYNAME,DAYOFMONTH,DAYOFWEEK,DAYOFYEAR,EXTRACT,HOUR,MINUTE,MONTH,MONTHNAME,NOW,"
            + "QUARTER,SECOND,WEEK,YEAR";
    }

    // Identifiers are case-sensitive, like HexDB's field names.
    @Override
    public boolean supportsMixedCaseIdentifiers() {
        return true;
    }

    @Override
    public boolean storesMixedCaseIdentifiers() {
        return true;
    }

    @Override
    public boolean supportsMixedCaseQuotedIdentifiers() {
        return true;
    }

    @Override
    public boolean storesMixedCaseQuotedIdentifiers() {
        return true;
    }

    // NULLs sort first ascending, last descending; NULL || x is NULL.
    @Override
    public boolean nullsAreSortedLow() {
        return true;
    }

    @Override
    public boolean nullPlusNonNullIsNull() {
        return true;
    }

    // What the SQL supports.
    @Override
    public boolean supportsColumnAliasing() {
        return true;
    }

    @Override
    public boolean supportsTableCorrelationNames() {
        return true;
    }

    @Override
    public boolean supportsDifferentTableCorrelationNames() {
        return true;
    }

    @Override
    public boolean supportsExpressionsInOrderBy() {
        return true;
    }

    @Override
    public boolean supportsOrderByUnrelated() {
        return true;
    }

    @Override
    public boolean supportsGroupBy() {
        return true;
    }

    @Override
    public boolean supportsGroupByUnrelated() {
        return true;
    }

    @Override
    public boolean supportsGroupByBeyondSelect() {
        return true;
    }

    @Override
    public boolean supportsLikeEscapeClause() {
        return true;
    }

    @Override
    public boolean supportsNonNullableColumns() {
        return true;
    }

    @Override
    public boolean supportsMinimumSQLGrammar() {
        return true;
    }

    @Override
    public boolean supportsANSI92EntryLevelSQL() {
        return true;
    }

    @Override
    public boolean supportsOuterJoins() {
        return true;
    }

    @Override
    public boolean supportsFullOuterJoins() {
        return true;
    }

    @Override
    public boolean supportsLimitedOuterJoins() {
        return true;
    }

    @Override
    public boolean supportsSubqueriesInComparisons() {
        return true;
    }

    @Override
    public boolean supportsSubqueriesInExists() {
        return true;
    }

    @Override
    public boolean supportsSubqueriesInIns() {
        return true;
    }

    @Override
    public boolean supportsSubqueriesInQuantifieds() {
        return true;
    }

    @Override
    public boolean supportsCorrelatedSubqueries() {
        return true;
    }

    @Override
    public boolean supportsUnion() {
        return true;
    }

    @Override
    public boolean supportsUnionAll() {
        return true;
    }

    @Override
    public int getDefaultTransactionIsolation() {
        return Connection.TRANSACTION_NONE;
    }

    @Override
    public boolean supportsTransactionIsolationLevel(int level) {
        return level == Connection.TRANSACTION_NONE;
    }

    @Override
    public boolean supportsResultSetType(int type) {
        return type == ResultSet.TYPE_FORWARD_ONLY;
    }

    @Override
    public boolean supportsResultSetConcurrency(int type, int concurrency) {
        return type == ResultSet.TYPE_FORWARD_ONLY && concurrency == ResultSet.CONCUR_READ_ONLY;
    }

    @Override
    public boolean supportsResultSetHoldability(int holdability) {
        return holdability == ResultSet.CLOSE_CURSORS_AT_COMMIT;
    }

    @Override
    public int getResultSetHoldability() {
        return ResultSet.CLOSE_CURSORS_AT_COMMIT;
    }

    @Override
    public int getSQLStateType() {
        return DatabaseMetaData.sqlStateSQL;
    }

    @Override
    public boolean supportsStoredFunctionsUsingCallSyntax() {
        return false;
    }

    @Override
    public boolean dataDefinitionIgnoredInTransactions() {
        return true;
    }
}
