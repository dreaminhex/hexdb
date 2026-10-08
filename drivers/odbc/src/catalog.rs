// Catalog functions: SQLTables, SQLColumns, SQLGetTypeInfo, SQLPrimaryKeys,
// SQLSpecialColumns, and the empty results for what HexDB doesn't have
// (foreign keys, statistics, procedures). Tessellations are tables in no
// catalog or schema; `id` is each table's primary key.

use crate::client::Client;
use crate::ffi::*;
use crate::stmt::{Column, ResultSet};
use crate::text::pattern_matches;
use crate::OdbcError;
use serde_json::{json, Value};

/// The SQL types results use, as SQLGetTypeInfo describes them.
const TYPES: &[(&str, SqlSmallInt, i64, Option<&str>, i16)] = &[
    // (name, type, column size, literal prefix/suffix, searchable)
    ("WLONGVARCHAR", SQL_WLONGVARCHAR, 1 << 30, Some("'"), 0),
    ("WVARCHAR", SQL_WVARCHAR, 1 << 30, Some("'"), 3),
    ("BIT", SQL_BIT, 1, None, 2),
    ("BIGINT", SQL_BIGINT, 19, None, 2),
    ("DOUBLE", SQL_DOUBLE, 15, None, 2),
];

pub fn type_name(sql_type: SqlSmallInt) -> &'static str {
    match sql_type {
        SQL_WLONGVARCHAR => "WLONGVARCHAR",
        SQL_WVARCHAR => "WVARCHAR",
        SQL_BIT => "BIT",
        SQL_BIGINT => "BIGINT",
        SQL_DOUBLE => "DOUBLE",
        SQL_SMALLINT => "SMALLINT",
        SQL_INTEGER => "INTEGER",
        _ => "WVARCHAR",
    }
}

/// An argument that may be a search pattern: `None` and "%" match everything.
fn matches(pattern: &Option<String>, text: &str, literal: bool) -> bool {
    match pattern.as_deref() {
        None => true,
        Some(p) if literal => p == text,
        Some(p) => pattern_matches(p, text),
    }
}

fn tables_columns() -> Vec<Column> {
    ["TABLE_CAT", "TABLE_SCHEM", "TABLE_NAME", "TABLE_TYPE", "REMARKS"].iter().map(|n| Column::text(n)).collect()
}

/// SQLTables, including its special forms that list catalogs, schemas or table types.
pub fn tables(
    client: &Client,
    catalog: Option<String>,
    schema: Option<String>,
    table: Option<String>,
    types: Option<String>,
    literal: bool,
) -> Result<ResultSet, OdbcError> {
    let empty = |s: &Option<String>| s.as_deref().is_none_or(str::is_empty);
    // There are no catalogs or schemas to list.
    if catalog.as_deref() == Some("%") && empty(&schema) && empty(&table) {
        return Ok(ResultSet::local(tables_columns(), Vec::new()));
    }
    if schema.as_deref() == Some("%") && empty(&catalog) && empty(&table) {
        return Ok(ResultSet::local(tables_columns(), Vec::new()));
    }
    if types.as_deref() == Some("%") && empty(&catalog) && empty(&schema) && empty(&table) {
        return Ok(ResultSet::local(tables_columns(), vec![vec![Value::Null, Value::Null, Value::Null, json!("TABLE"), Value::Null]]));
    }
    // Tables live in no catalog or schema: a non-empty, non-wildcard one matches nothing.
    let anything = |s: &Option<String>| s.as_deref().is_none_or(|v| v.is_empty() || v == "%");
    if !anything(&catalog) || !anything(&schema) {
        return Ok(ResultSet::local(tables_columns(), Vec::new()));
    }
    if let Some(types) = &types {
        let wanted: Vec<String> = types.split(',').map(|t| t.trim().trim_matches('\'').to_ascii_uppercase()).filter(|t| !t.is_empty()).collect();
        if !wanted.is_empty() && !wanted.iter().any(|t| t == "TABLE" || t == "%") {
            return Ok(ResultSet::local(tables_columns(), Vec::new()));
        }
    }
    let rows = client
        .tables()?
        .into_iter()
        .filter(|name| matches(&table, name, literal))
        .map(|name| vec![Value::Null, Value::Null, json!(name), json!("TABLE"), json!("")])
        .collect();
    Ok(ResultSet::local(tables_columns(), rows))
}

/// SQLColumns: each matching table's columns, from GET /sql/columns.
pub fn columns(
    client: &Client,
    catalog: Option<String>,
    schema: Option<String>,
    table: Option<String>,
    column: Option<String>,
    literal: bool,
) -> Result<ResultSet, OdbcError> {
    let header: Vec<Column> = vec![
        Column::text("TABLE_CAT"),
        Column::text("TABLE_SCHEM"),
        Column::text("TABLE_NAME"),
        Column::text("COLUMN_NAME"),
        Column::small("DATA_TYPE"),
        Column::text("TYPE_NAME"),
        Column::int("COLUMN_SIZE"),
        Column::int("BUFFER_LENGTH"),
        Column::small("DECIMAL_DIGITS"),
        Column::small("NUM_PREC_RADIX"),
        Column::small("NULLABLE"),
        Column::text("REMARKS"),
        Column::text("COLUMN_DEF"),
        Column::small("SQL_DATA_TYPE"),
        Column::small("SQL_DATETIME_SUB"),
        Column::int("CHAR_OCTET_LENGTH"),
        Column::int("ORDINAL_POSITION"),
        Column::text("IS_NULLABLE"),
    ];
    let anything = |s: &Option<String>| s.as_deref().is_none_or(|v| v.is_empty() || v == "%");
    if !anything(&catalog) || !anything(&schema) {
        return Ok(ResultSet::local(header, Vec::new()));
    }
    let max_string = client.settings.max_string_length;
    let mut rows = Vec::new();
    let mut tables: Vec<String> = client.tables()?.into_iter().filter(|t| matches(&table, t, literal)).collect();
    tables.sort();
    for t in tables {
        let list = match client.columns(&t) {
            Ok(list) => list,
            Err(e) if e.state == "42000" || e.state == "42S02" => continue, // not readable after all, or dropped
            Err(e) => return Err(e),
        };
        for (i, c) in list.iter().enumerate() {
            let name = c["name"].as_str().unwrap_or_default();
            if !matches(&column, name, literal) {
                continue;
            }
            let col = Column::from_hexdb(name, c["type"].as_str().unwrap_or("string"), &t, max_string);
            let nullable = c["nullable"].as_bool().unwrap_or(true) && name != "id";
            let radix = match col.sql_type {
                SQL_BIGINT => json!(10),
                SQL_DOUBLE => json!(2),
                _ => Value::Null,
            };
            let char_octets = if matches!(col.sql_type, SQL_WVARCHAR | SQL_WLONGVARCHAR) { json!(col.octet_length().min(i32::MAX as usize)) } else { Value::Null };
            rows.push(vec![
                Value::Null,
                Value::Null,
                json!(t),
                json!(name),
                json!(col.sql_type),
                json!(col.type_name()),
                json!(col.size.min(i32::MAX as usize)),
                json!(col.octet_length().min(i32::MAX as usize)),
                if col.sql_type == SQL_BIGINT { json!(0) } else { Value::Null },
                radix,
                json!(if nullable { SQL_NULLABLE } else { SQL_NO_NULLS }),
                json!(c["source"].as_str().map(|s| if s == "sample" { "inferred from documents" } else { "from the schema" }).unwrap_or("")),
                Value::Null,
                json!(col.sql_type),
                Value::Null,
                char_octets,
                json!(i + 1),
                json!(if nullable { "YES" } else { "NO" }),
            ]);
        }
    }
    Ok(ResultSet::local(header, rows))
}

/// SQLGetTypeInfo: the types results use (all, or one).
pub fn type_info(data_type: SqlSmallInt) -> ResultSet {
    let header = vec![
        Column::text("TYPE_NAME"),
        Column::small("DATA_TYPE"),
        Column::int("COLUMN_SIZE"),
        Column::text("LITERAL_PREFIX"),
        Column::text("LITERAL_SUFFIX"),
        Column::text("CREATE_PARAMS"),
        Column::small("NULLABLE"),
        Column::small("CASE_SENSITIVE"),
        Column::small("SEARCHABLE"),
        Column::small("UNSIGNED_ATTRIBUTE"),
        Column::small("FIXED_PREC_SCALE"),
        Column::small("AUTO_UNIQUE_VALUE"),
        Column::text("LOCAL_TYPE_NAME"),
        Column::small("MINIMUM_SCALE"),
        Column::small("MAXIMUM_SCALE"),
        Column::small("SQL_DATA_TYPE"),
        Column::small("SQL_DATETIME_SUB"),
        Column::int("NUM_PREC_RADIX"),
        Column::small("INTERVAL_PRECISION"),
    ];
    let rows = TYPES
        .iter()
        .filter(|t| data_type == SQL_ALL_TYPES || t.1 == data_type)
        .map(|(name, ty, size, literal, searchable)| {
            let numeric = matches!(*ty, SQL_BIGINT | SQL_DOUBLE);
            vec![
                json!(name),
                json!(ty),
                json!(size),
                json!(literal),
                json!(literal),
                Value::Null,
                json!(SQL_NULLABLE),
                json!(if numeric || *ty == SQL_BIT { 0 } else { 1 }),
                json!(searchable),
                if numeric { json!(0) } else { Value::Null },
                json!(0),
                if numeric { json!(0) } else { Value::Null },
                json!(name),
                if *ty == SQL_BIGINT { json!(0) } else { Value::Null },
                if *ty == SQL_BIGINT { json!(0) } else { Value::Null },
                json!(ty),
                Value::Null,
                match *ty {
                    SQL_BIGINT => json!(10),
                    SQL_DOUBLE => json!(2),
                    _ => Value::Null,
                },
                Value::Null,
            ]
        })
        .collect();
    ResultSet::local(header, rows)
}

/// SQLPrimaryKeys: `id` for any existing table.
pub fn primary_keys(client: &Client, table: Option<String>) -> Result<ResultSet, OdbcError> {
    let header: Vec<Column> = vec![
        Column::text("TABLE_CAT"),
        Column::text("TABLE_SCHEM"),
        Column::text("TABLE_NAME"),
        Column::text("COLUMN_NAME"),
        Column::small("KEY_SEQ"),
        Column::text("PK_NAME"),
    ];
    let Some(table) = table else { return Ok(ResultSet::local(header, Vec::new())) };
    let rows = if client.tables()?.contains(&table) {
        vec![vec![Value::Null, Value::Null, json!(table), json!("id"), json!(1), json!(format!("{}_id", table))]]
    } else {
        Vec::new()
    };
    Ok(ResultSet::local(header, rows))
}

/// SQLSpecialColumns: `id` identifies a row (SQL_BEST_ROWID); nothing changes
/// automatically (SQL_ROWVER).
pub fn special_columns(client: &Client, identifier_type: SqlUSmallInt, table: Option<String>) -> Result<ResultSet, OdbcError> {
    let header: Vec<Column> = vec![
        Column::small("SCOPE"),
        Column::text("COLUMN_NAME"),
        Column::small("DATA_TYPE"),
        Column::text("TYPE_NAME"),
        Column::int("COLUMN_SIZE"),
        Column::int("BUFFER_LENGTH"),
        Column::small("DECIMAL_DIGITS"),
        Column::small("PSEUDO_COLUMN"),
    ];
    const SQL_BEST_ROWID: SqlUSmallInt = 1;
    let rows = match table {
        Some(t) if identifier_type == SQL_BEST_ROWID && client.tables()?.contains(&t) => {
            // SQL_SCOPE_SESSION (2), 26-character ULID, SQL_PC_NOT_PSEUDO (1)
            vec![vec![json!(2), json!("id"), json!(SQL_WVARCHAR), json!("WVARCHAR"), json!(26), json!(52), Value::Null, json!(1)]]
        }
        _ => Vec::new(),
    };
    Ok(ResultSet::local(header, rows))
}

/// SQLForeignKeys: HexDB has none.
pub fn foreign_keys() -> ResultSet {
    let names = [
        "PKTABLE_CAT", "PKTABLE_SCHEM", "PKTABLE_NAME", "PKCOLUMN_NAME", "FKTABLE_CAT", "FKTABLE_SCHEM", "FKTABLE_NAME", "FKCOLUMN_NAME",
        "KEY_SEQ", "UPDATE_RULE", "DELETE_RULE", "FK_NAME", "PK_NAME", "DEFERRABILITY",
    ];
    let header = names
        .iter()
        .map(|n| if ["KEY_SEQ", "UPDATE_RULE", "DELETE_RULE", "DEFERRABILITY"].contains(n) { Column::small(n) } else { Column::text(n) })
        .collect();
    ResultSet::local(header, Vec::new())
}

/// SQLStatistics: no index details are reported.
pub fn statistics() -> ResultSet {
    let header = vec![
        Column::text("TABLE_CAT"),
        Column::text("TABLE_SCHEM"),
        Column::text("TABLE_NAME"),
        Column::small("NON_UNIQUE"),
        Column::text("INDEX_QUALIFIER"),
        Column::text("INDEX_NAME"),
        Column::small("TYPE"),
        Column::small("ORDINAL_POSITION"),
        Column::text("COLUMN_NAME"),
        Column::text("ASC_OR_DESC"),
        Column::int("CARDINALITY"),
        Column::int("PAGES"),
        Column::text("FILTER_CONDITION"),
    ];
    ResultSet::local(header, Vec::new())
}

/// SQLProcedures: functions aren't exposed as procedures.
pub fn procedures() -> ResultSet {
    let header = vec![
        Column::text("PROCEDURE_CAT"),
        Column::text("PROCEDURE_SCHEM"),
        Column::text("PROCEDURE_NAME"),
        Column::int("NUM_INPUT_PARAMS"),
        Column::int("NUM_OUTPUT_PARAMS"),
        Column::int("NUM_RESULT_SETS"),
        Column::text("REMARKS"),
        Column::small("PROCEDURE_TYPE"),
    ];
    ResultSet::local(header, Vec::new())
}
