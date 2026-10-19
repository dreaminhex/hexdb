// SQLGetInfo: what the driver and HexDB's SQL support. Read-only,
// forward-only, one table per query, the five standard aggregates.

use crate::client::Client;

pub enum Info {
    Str(String),
    U16(u16),
    U32(u32),
}

pub const DRIVER_NAME: &str = "hexdb_odbc";

pub fn get(info_type: u16, client: Option<&Client>) -> Option<Info> {
    use Info::*;
    let s = |v: &str| Some(Str(v.to_string()));
    Some(match info_type {
        // Driver
        0 => U16(0),                                       // SQL_MAX_DRIVER_CONNECTIONS: no limit
        1 => U16(0),                                       // SQL_MAX_CONCURRENT_ACTIVITIES: no limit
        2 => return s(client.and_then(|c| c.settings.dsn.as_deref()).unwrap_or("")), // SQL_DATA_SOURCE_NAME
        6 => return s(DRIVER_NAME),                        // SQL_DRIVER_NAME
        7 => return s(&driver_version()),                  // SQL_DRIVER_VER
        77 => return s("03.80"),                           // SQL_DRIVER_ODBC_VER
        10 => return s("03.80"),                           // SQL_ODBC_VER
        13 => return s(client.map(|c| c.server_name.as_str()).filter(|n| !n.is_empty()).unwrap_or("HexDB")), // SQL_SERVER_NAME
        16 => return s(""),                                // SQL_DATABASE_NAME
        17 => return s("HexDB"),                           // SQL_DBMS_NAME
        18 => return s(&dbms_version(client)),             // SQL_DBMS_VER
        47 => return s(client.map(|c| c.login.as_str()).unwrap_or("")), // SQL_USER_NAME
        25 => return s("Y"),                               // SQL_DATA_SOURCE_READ_ONLY
        19 => return s("Y"),                               // SQL_ACCESSIBLE_TABLES
        20 => return s("N"),                               // SQL_ACCESSIBLE_PROCEDURES
        21 => return s("N"),                               // SQL_PROCEDURES
        // Naming
        29 => return s("\""),                              // SQL_IDENTIFIER_QUOTE_CHAR
        28 => U16(3),                                      // SQL_IDENTIFIER_CASE: SQL_IC_SENSITIVE
        93 => U16(3),                                      // SQL_QUOTED_IDENTIFIER_CASE: SQL_IC_SENSITIVE
        41 => return s("."),                               // SQL_CATALOG_NAME_SEPARATOR
        42 => return s(""),                                // SQL_CATALOG_TERM: no catalogs
        39 => return s(""),                                // SQL_SCHEMA_TERM: no schemas
        45 => return s("table"),                           // SQL_TABLE_TERM
        40 => return s(""),                                // SQL_PROCEDURE_TERM
        10003 => return s("N"),                            // SQL_CATALOG_NAME
        114 => U16(0),                                     // SQL_CATALOG_LOCATION
        92 => U32(0),                                      // SQL_CATALOG_USAGE
        91 => U32(0),                                      // SQL_SCHEMA_USAGE
        14 => return s("\\"),                              // SQL_SEARCH_PATTERN_ESCAPE
        94 => return s(""),                                // SQL_SPECIAL_CHARACTERS
        89 => return s(""),                                // SQL_KEYWORDS
        // Limits (0: none or unknown)
        30 | 31 | 32 | 33 | 34 | 35 | 97 | 98 | 99 | 100 | 101 | 106 | 107 => U16(0),
        102 | 104 | 105 | 108 | 112 => U32(0),             // index size, row size, statement and literal lengths
        10005 => U16(64),                                  // SQL_MAX_IDENTIFIER_LEN
        // SQL support
        118 => U32(1),                                     // SQL_SQL_CONFORMANCE: SQL_SC_SQL92_ENTRY
        152 => U32(1),                                     // SQL_ODBC_INTERFACE_CONFORMANCE: SQL_OIC_CORE
        15 => U16(1),                                      // SQL_ODBC_SQL_CONFORMANCE (ODBC 2): core
        9 => U16(1),                                       // SQL_ODBC_API_CONFORMANCE (ODBC 2): level 1
        169 => U32(0x7F),                                  // SQL_AGGREGATE_FUNCTIONS: AVG COUNT MAX MIN SUM DISTINCT ALL
        88 => U16(3),                                      // SQL_GROUP_BY: SQL_GB_NO_RELATION
        90 => return s("N"),                               // SQL_ORDER_BY_COLUMNS_IN_SELECT
        27 => return s("Y"),                               // SQL_EXPRESSIONS_IN_ORDERBY
        87 => return s("Y"),                               // SQL_COLUMN_ALIAS
        74 => U16(2),                                      // SQL_CORRELATION_NAME: SQL_CN_ANY
        113 => return s("Y"),                              // SQL_LIKE_ESCAPE_CLAUSE
        73 => return s("N"),                               // SQL_INTEGRITY
        38 => return s("Y"),                               // SQL_OUTER_JOINS
        115 => U32(0x7F),                                  // SQL_OJ_CAPABILITIES: LEFT RIGHT FULL NESTED NOT_ORDERED INNER ALL_COMPARISON_OPS
        161 => U32(0x17E),                                 // SQL_SQL92_RELATIONAL_JOIN_OPERATORS: CROSS_JOIN EXCEPT_JOIN FULL_OUTER_JOIN INNER_JOIN INTERSECT_JOIN LEFT_OUTER_JOIN RIGHT_OUTER_JOIN
        // SQL_SQL92_PREDICATES: EXISTS ISNOTNULL ISNULL LIKE IN BETWEEN COMPARISON QUANTIFIED_COMPARISON
        160 => U32(0x3E07),
        164 => U32(0xEE),                                  // SQL_SQL92_STRING_FUNCTIONS: LOWER UPPER SUBSTRING TRIM_BOTH TRIM_LEADING TRIM_TRAILING
        163 => U32(0),                                     // SQL_SQL92_ROW_VALUE_CONSTRUCTOR
        165 => U32(0xF),                                   // SQL_SQL92_VALUE_EXPRESSIONS: CASE CAST COALESCE NULLIF
        159 => U32(0x2E),                                  // SQL_SQL92_NUMERIC_VALUE_FUNCTIONS: CHAR_LENGTH CHARACTER_LENGTH EXTRACT POSITION
        156 | 157 | 158 | 162 | 166 | 167 | 168 | 171 | 172 | 173 | 174 => U32(0),
        49 => U32(0xD95F21),                               // SQL_NUMERIC_FUNCTIONS: ABS CEILING EXP FLOOR LOG MOD SIGN SQRT PI LOG10 POWER ROUND TRUNCATE
        50 => U32(0xB11F7D),                               // SQL_STRING_FUNCTIONS: CONCAT LEFT LTRIM LENGTH LOCATE LCASE REPLACE RIGHT RTRIM SUBSTRING UCASE LOCATE_2 CHAR_LENGTH CHARACTER_LENGTH POSITION
        51 => U32(0x4),                                    // SQL_SYSTEM_FUNCTIONS: IFNULL
        52 => U32(0x1F9FFF),                               // SQL_TIMEDATE_FUNCTIONS: NOW CURDATE DAYOFMONTH DAYOFWEEK DAYOFYEAR MONTH QUARTER WEEK YEAR CURTIME HOUR MINUTE SECOND DAYNAME MONTHNAME CURRENT_DATE CURRENT_TIME CURRENT_TIMESTAMP EXTRACT
        48 => U32(0x2),                                    // SQL_CONVERT_FUNCTIONS: CAST
        53..=70 | 71 | 122 | 123 | 124 | 125 | 126 => U32(0), // SQL_CONVERT_* : no conversions
        82 => U32(0),                                      // SQL_BOOKMARK_PERSISTENCE
        85 => U16(1),                                      // SQL_NULL_COLLATION: SQL_NC_LOW
        22 => U16(0),                                      // SQL_CONCAT_NULL_BEHAVIOR: SQL_CB_NULL (a || NULL is NULL)
        75 => U16(0),                                      // SQL_NON_NULLABLE_COLUMNS: SQL_NNC_NULL
        36 => return s("N"),                               // SQL_MULT_RESULT_SETS
        37 => return s("N"),                               // SQL_MULTIPLE_ACTIVE_TXN
        111 => return s("N"),                              // SQL_NEED_LONG_DATA_LEN
        10002 => return s("N"),                            // SQL_DESCRIBE_PARAMETER
        11 => return s("N"),                               // SQL_ROW_UPDATES
        103 => return s("N"),                              // SQL_MAX_ROW_SIZE_INCLUDES_LONG
        // Cursors and statements
        44 => U32(1),                                      // SQL_SCROLL_OPTIONS: SQL_SO_FORWARD_ONLY
        43 => U32(1),                                      // SQL_SCROLL_CONCURRENCY: SQL_SCCO_READ_ONLY
        146 => U32(1),                                     // SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES1: SQL_CA1_NEXT
        147 => U32(0),                                     // ..._ATTRIBUTES2
        144 | 145 | 150 | 151 => U32(0),                   // dynamic / keyset cursor attributes
        81 => U32(1 | 2 | 4),                              // SQL_GETDATA_EXTENSIONS: ANY_COLUMN, ANY_ORDER, BLOCK
        23 | 24 => U16(1),                                 // SQL_CURSOR_COMMIT/ROLLBACK_BEHAVIOR: SQL_CB_CLOSE
        10001 => U16(0),                                   // SQL_CURSOR_SENSITIVITY: unspecified
        78 => U32(0),                                      // SQL_LOCK_TYPES
        79 => U32(0),                                      // SQL_POS_OPERATIONS
        80 => U32(0),                                      // SQL_POSITIONED_STATEMENTS
        83 => U32(0),                                      // SQL_STATIC_SENSITIVITY
        // Transactions: none (each statement reads a consistent page)
        46 => U16(0),                                      // SQL_TXN_CAPABLE: SQL_TC_NONE
        26 => U32(0),                                      // SQL_DEFAULT_TXN_ISOLATION
        72 => U32(0),                                      // SQL_TXN_ISOLATION_OPTION
        // Misc
        117 => U32(0),                                     // SQL_ALTER_DOMAIN
        86 => U32(0),                                      // SQL_ALTER_TABLE
        127..=143 => U32(0),                               // create/drop statements
        148 | 149 => U32(0),                               // index keywords, info schema views
        153 => U32(1),                                     // SQL_PARAM_ARRAY_ROW_COUNTS: SQL_PARC_BATCH
        154 => U32(2),                                     // SQL_PARAM_ARRAY_SELECTS: SQL_PAS_NO_BATCH
        120 => U32(0),                                     // SQL_BATCH_ROW_COUNT
        121 => U32(0),                                     // SQL_BATCH_SUPPORT
        116 => U32(0),                                     // SQL_ACTIVE_ENVIRONMENTS
        10021 => U32(0),                                   // SQL_ASYNC_MODE: SQL_AM_NONE
        10022 => U32(0),                                   // SQL_MAX_ASYNC_CONCURRENT_STATEMENTS
        10023 => U32(0),                                   // SQL_ASYNC_DBC_FUNCTIONS
        10024 => U32(0),                                   // SQL_DRIVER_AWARE_POOLING_SUPPORTED
        10025 => U32(0),                                   // SQL_ASYNC_NOTIFICATION
        170 => U32(0),                                     // SQL_DDL_INDEX
        119 => U32(0),                                     // SQL_DATETIME_LITERALS
        155 => U32(0x7),                                   // SQL_SQL92_DATETIME_FUNCTIONS: CURRENT_DATE CURRENT_TIME CURRENT_TIMESTAMP
        95 => U32(0x1F),                                   // SQL_SUBQUERIES: COMPARISON EXISTS IN QUANTIFIED CORRELATED_SUBQUERIES
        96 => U32(0x3),                                    // SQL_UNION: UNION, UNION ALL
        109 | 110 => U32(0),                               // SQL_TIMEDATE_ADD/DIFF_INTERVALS: no TIMESTAMPADD/DIFF
        84 => U32(0),                                      // SQL_FILE_USAGE: not a file-based driver
        _ => return None,
    })
}

fn driver_version() -> String {
    // ODBC wants ##.##.#### (major.minor.release).
    let mut parts = env!("CARGO_PKG_VERSION").split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    format!("{:02}.{:02}.{:04}", parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

fn dbms_version(client: Option<&Client>) -> String {
    let version = client.map(|c| c.server_version.as_str()).unwrap_or("");
    let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    format!("{:02}.{:02}.{:04}", parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}
