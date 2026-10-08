// ODBC types and constants (from sql.h, sqlext.h and sqlucode.h), the subset
// this driver uses. Defined here so the driver doesn't link to a driver manager.

#![allow(dead_code)]

use std::ffi::c_void;

pub type SqlHandle = *mut c_void;
pub type SqlPointer = *mut c_void;
pub type SqlSmallInt = i16;
pub type SqlUSmallInt = u16;
pub type SqlInteger = i32;
pub type SqlUInteger = u32;
pub type SqlLen = isize;
pub type SqlULen = usize;
pub type SqlReturn = i16;
pub type SqlWChar = u16;

// Return codes
pub const SQL_SUCCESS: SqlReturn = 0;
pub const SQL_SUCCESS_WITH_INFO: SqlReturn = 1;
pub const SQL_ERROR: SqlReturn = -1;
pub const SQL_INVALID_HANDLE: SqlReturn = -2;
pub const SQL_NO_DATA: SqlReturn = 100;

// Handle types
pub const SQL_HANDLE_ENV: SqlSmallInt = 1;
pub const SQL_HANDLE_DBC: SqlSmallInt = 2;
pub const SQL_HANDLE_STMT: SqlSmallInt = 3;
pub const SQL_HANDLE_DESC: SqlSmallInt = 4;

// Lengths and indicators
pub const SQL_NTS: SqlInteger = -3;
pub const SQL_NULL_DATA: SqlLen = -1;
pub const SQL_DATA_AT_EXEC: SqlLen = -2;
pub const SQL_LEN_DATA_AT_EXEC_OFFSET: SqlLen = -100;
pub const SQL_NO_TOTAL: SqlLen = -4;

// SQL data types
pub const SQL_UNKNOWN_TYPE: SqlSmallInt = 0;
pub const SQL_CHAR: SqlSmallInt = 1;
pub const SQL_NUMERIC: SqlSmallInt = 2;
pub const SQL_DECIMAL: SqlSmallInt = 3;
pub const SQL_INTEGER: SqlSmallInt = 4;
pub const SQL_SMALLINT: SqlSmallInt = 5;
pub const SQL_FLOAT: SqlSmallInt = 6;
pub const SQL_REAL: SqlSmallInt = 7;
pub const SQL_DOUBLE: SqlSmallInt = 8;
pub const SQL_DATETIME: SqlSmallInt = 9;
pub const SQL_VARCHAR: SqlSmallInt = 12;
pub const SQL_TYPE_DATE: SqlSmallInt = 91;
pub const SQL_TYPE_TIME: SqlSmallInt = 92;
pub const SQL_TYPE_TIMESTAMP: SqlSmallInt = 93;
pub const SQL_LONGVARCHAR: SqlSmallInt = -1;
pub const SQL_BINARY: SqlSmallInt = -2;
pub const SQL_VARBINARY: SqlSmallInt = -3;
pub const SQL_LONGVARBINARY: SqlSmallInt = -4;
pub const SQL_BIGINT: SqlSmallInt = -5;
pub const SQL_TINYINT: SqlSmallInt = -6;
pub const SQL_BIT: SqlSmallInt = -7;
pub const SQL_WCHAR: SqlSmallInt = -8;
pub const SQL_WVARCHAR: SqlSmallInt = -9;
pub const SQL_WLONGVARCHAR: SqlSmallInt = -10;
pub const SQL_GUID: SqlSmallInt = -11;
pub const SQL_ALL_TYPES: SqlSmallInt = 0;

// C data types
pub const SQL_C_CHAR: SqlSmallInt = 1;
pub const SQL_C_NUMERIC: SqlSmallInt = 2;
pub const SQL_C_LONG: SqlSmallInt = 4;
pub const SQL_C_SHORT: SqlSmallInt = 5;
pub const SQL_C_FLOAT: SqlSmallInt = 7;
pub const SQL_C_DOUBLE: SqlSmallInt = 8;
pub const SQL_C_DATE: SqlSmallInt = 9;
pub const SQL_C_TIME: SqlSmallInt = 10;
pub const SQL_C_TIMESTAMP: SqlSmallInt = 11;
pub const SQL_C_TYPE_DATE: SqlSmallInt = 91;
pub const SQL_C_TYPE_TIME: SqlSmallInt = 92;
pub const SQL_C_TYPE_TIMESTAMP: SqlSmallInt = 93;
pub const SQL_C_DEFAULT: SqlSmallInt = 99;
pub const SQL_ARD_TYPE: SqlSmallInt = -99;
pub const SQL_C_BINARY: SqlSmallInt = -2;
pub const SQL_C_BIT: SqlSmallInt = -7;
pub const SQL_C_TINYINT: SqlSmallInt = -6;
pub const SQL_C_WCHAR: SqlSmallInt = -8;
pub const SQL_C_GUID: SqlSmallInt = -11;
pub const SQL_C_SSHORT: SqlSmallInt = -15;
pub const SQL_C_SLONG: SqlSmallInt = -16;
pub const SQL_C_USHORT: SqlSmallInt = -17;
pub const SQL_C_ULONG: SqlSmallInt = -18;
pub const SQL_C_SBIGINT: SqlSmallInt = -25;
pub const SQL_C_STINYINT: SqlSmallInt = -26;
pub const SQL_C_UBIGINT: SqlSmallInt = -27;
pub const SQL_C_UTINYINT: SqlSmallInt = -28;

// Nullability
pub const SQL_NO_NULLS: SqlSmallInt = 0;
pub const SQL_NULLABLE: SqlSmallInt = 1;
pub const SQL_NULLABLE_UNKNOWN: SqlSmallInt = 2;

// Environment attributes
pub const SQL_ATTR_ODBC_VERSION: SqlInteger = 200;
pub const SQL_ATTR_CONNECTION_POOLING: SqlInteger = 201;
pub const SQL_ATTR_CP_MATCH: SqlInteger = 202;
pub const SQL_ATTR_OUTPUT_NTS: SqlInteger = 10001;
pub const SQL_OV_ODBC3: i32 = 3;

// Connection attributes
pub const SQL_ATTR_ACCESS_MODE: SqlInteger = 101;
pub const SQL_ATTR_AUTOCOMMIT: SqlInteger = 102;
pub const SQL_ATTR_LOGIN_TIMEOUT: SqlInteger = 103;
pub const SQL_ATTR_TXN_ISOLATION: SqlInteger = 108;
pub const SQL_ATTR_CURRENT_CATALOG: SqlInteger = 109;
pub const SQL_ATTR_CONNECTION_TIMEOUT: SqlInteger = 113;
pub const SQL_ATTR_ANSI_APP: SqlInteger = 115;
pub const SQL_ATTR_CONNECTION_DEAD: SqlInteger = 1209;
pub const SQL_ATTR_AUTO_IPD: SqlInteger = 10001;
pub const SQL_ATTR_METADATA_ID: SqlInteger = 10014;
pub const SQL_MODE_READ_ONLY: usize = 1;
pub const SQL_AUTOCOMMIT_ON: usize = 1;

// Statement attributes
pub const SQL_ATTR_QUERY_TIMEOUT: SqlInteger = 0;
pub const SQL_ATTR_MAX_ROWS: SqlInteger = 1;
pub const SQL_ATTR_NOSCAN: SqlInteger = 2;
pub const SQL_ATTR_MAX_LENGTH: SqlInteger = 3;
pub const SQL_ATTR_ASYNC_ENABLE: SqlInteger = 4;
pub const SQL_ATTR_ROW_BIND_TYPE: SqlInteger = 5;
pub const SQL_ATTR_CURSOR_TYPE: SqlInteger = 6;
pub const SQL_ATTR_CONCURRENCY: SqlInteger = 7;
pub const SQL_ATTR_KEYSET_SIZE: SqlInteger = 8;
pub const SQL_ROWSET_SIZE: SqlInteger = 9;
pub const SQL_ATTR_SIMULATE_CURSOR: SqlInteger = 10;
pub const SQL_ATTR_RETRIEVE_DATA: SqlInteger = 11;
pub const SQL_ATTR_USE_BOOKMARKS: SqlInteger = 12;
pub const SQL_ATTR_ROW_NUMBER: SqlInteger = 14;
pub const SQL_ATTR_ENABLE_AUTO_IPD: SqlInteger = 15;
pub const SQL_ATTR_FETCH_BOOKMARK_PTR: SqlInteger = 16;
pub const SQL_ATTR_PARAM_BIND_OFFSET_PTR: SqlInteger = 17;
pub const SQL_ATTR_PARAM_BIND_TYPE: SqlInteger = 18;
pub const SQL_ATTR_PARAM_OPERATION_PTR: SqlInteger = 19;
pub const SQL_ATTR_PARAM_STATUS_PTR: SqlInteger = 20;
pub const SQL_ATTR_PARAMS_PROCESSED_PTR: SqlInteger = 21;
pub const SQL_ATTR_PARAMSET_SIZE: SqlInteger = 22;
pub const SQL_ATTR_ROW_BIND_OFFSET_PTR: SqlInteger = 23;
pub const SQL_ATTR_ROW_OPERATION_PTR: SqlInteger = 24;
pub const SQL_ATTR_ROW_STATUS_PTR: SqlInteger = 25;
pub const SQL_ATTR_ROWS_FETCHED_PTR: SqlInteger = 26;
pub const SQL_ATTR_ROW_ARRAY_SIZE: SqlInteger = 27;
pub const SQL_ATTR_CURSOR_SCROLLABLE: SqlInteger = -1;
pub const SQL_ATTR_CURSOR_SENSITIVITY: SqlInteger = -2;
pub const SQL_ATTR_APP_ROW_DESC: SqlInteger = 10010;
pub const SQL_ATTR_APP_PARAM_DESC: SqlInteger = 10011;
pub const SQL_ATTR_IMP_ROW_DESC: SqlInteger = 10012;
pub const SQL_ATTR_IMP_PARAM_DESC: SqlInteger = 10013;

pub const SQL_CURSOR_FORWARD_ONLY: usize = 0;
pub const SQL_CONCUR_READ_ONLY: usize = 1;
pub const SQL_BIND_BY_COLUMN: usize = 0;

// Row status
pub const SQL_ROW_SUCCESS: SqlUSmallInt = 0;
pub const SQL_ROW_NOROW: SqlUSmallInt = 3;
pub const SQL_ROW_ERROR: SqlUSmallInt = 5;
pub const SQL_ROW_SUCCESS_WITH_INFO: SqlUSmallInt = 6;

// SQLFreeStmt options
pub const SQL_CLOSE: SqlUSmallInt = 0;
pub const SQL_DROP: SqlUSmallInt = 1;
pub const SQL_UNBIND: SqlUSmallInt = 2;
pub const SQL_RESET_PARAMS: SqlUSmallInt = 3;

// Fetch orientation
pub const SQL_FETCH_NEXT: SqlSmallInt = 1;

// SQLDriverConnect completion
pub const SQL_DRIVER_NOPROMPT: SqlUSmallInt = 0;

// Diagnostic fields
pub const SQL_DIAG_RETURNCODE: SqlSmallInt = 1;
pub const SQL_DIAG_NUMBER: SqlSmallInt = 2;
pub const SQL_DIAG_ROW_COUNT: SqlSmallInt = 3;
pub const SQL_DIAG_SQLSTATE: SqlSmallInt = 4;
pub const SQL_DIAG_NATIVE: SqlSmallInt = 5;
pub const SQL_DIAG_MESSAGE_TEXT: SqlSmallInt = 6;
pub const SQL_DIAG_DYNAMIC_FUNCTION: SqlSmallInt = 7;
pub const SQL_DIAG_CLASS_ORIGIN: SqlSmallInt = 8;
pub const SQL_DIAG_SUBCLASS_ORIGIN: SqlSmallInt = 9;
pub const SQL_DIAG_CONNECTION_NAME: SqlSmallInt = 10;
pub const SQL_DIAG_SERVER_NAME: SqlSmallInt = 11;
pub const SQL_DIAG_DYNAMIC_FUNCTION_CODE: SqlSmallInt = 12;
pub const SQL_DIAG_CURSOR_ROW_COUNT: SqlSmallInt = -1249;
pub const SQL_DIAG_ROW_NUMBER: SqlSmallInt = -1248;
pub const SQL_DIAG_COLUMN_NUMBER: SqlSmallInt = -1247;

// Column attributes (SQLColAttribute)
pub const SQL_COLUMN_COUNT: SqlUSmallInt = 0;
pub const SQL_COLUMN_NAME: SqlUSmallInt = 1;
pub const SQL_COLUMN_LENGTH: SqlUSmallInt = 3;
pub const SQL_COLUMN_PRECISION: SqlUSmallInt = 4;
pub const SQL_COLUMN_SCALE: SqlUSmallInt = 5;
pub const SQL_COLUMN_NULLABLE: SqlUSmallInt = 7;
pub const SQL_DESC_CONCISE_TYPE: SqlUSmallInt = 2;
pub const SQL_DESC_DISPLAY_SIZE: SqlUSmallInt = 6;
pub const SQL_DESC_UNSIGNED: SqlUSmallInt = 8;
pub const SQL_DESC_FIXED_PREC_SCALE: SqlUSmallInt = 9;
pub const SQL_DESC_UPDATABLE: SqlUSmallInt = 10;
pub const SQL_DESC_AUTO_UNIQUE_VALUE: SqlUSmallInt = 11;
pub const SQL_DESC_CASE_SENSITIVE: SqlUSmallInt = 12;
pub const SQL_DESC_SEARCHABLE: SqlUSmallInt = 13;
pub const SQL_DESC_TYPE_NAME: SqlUSmallInt = 14;
pub const SQL_DESC_TABLE_NAME: SqlUSmallInt = 15;
pub const SQL_DESC_SCHEMA_NAME: SqlUSmallInt = 16;
pub const SQL_DESC_CATALOG_NAME: SqlUSmallInt = 17;
pub const SQL_DESC_LABEL: SqlUSmallInt = 18;
pub const SQL_DESC_BASE_COLUMN_NAME: SqlUSmallInt = 22;
pub const SQL_DESC_BASE_TABLE_NAME: SqlUSmallInt = 23;
pub const SQL_DESC_LITERAL_PREFIX: SqlUSmallInt = 27;
pub const SQL_DESC_LITERAL_SUFFIX: SqlUSmallInt = 28;
pub const SQL_DESC_LOCAL_TYPE_NAME: SqlUSmallInt = 29;
pub const SQL_DESC_NUM_PREC_RADIX: SqlUSmallInt = 32;
pub const SQL_DESC_COUNT: SqlUSmallInt = 1001;
pub const SQL_DESC_TYPE: SqlUSmallInt = 1002;
pub const SQL_DESC_LENGTH: SqlUSmallInt = 1003;
pub const SQL_DESC_PRECISION: SqlUSmallInt = 1005;
pub const SQL_DESC_SCALE: SqlUSmallInt = 1006;
pub const SQL_DESC_DATETIME_INTERVAL_CODE: SqlUSmallInt = 1007;
pub const SQL_DESC_NULLABLE: SqlUSmallInt = 1008;
pub const SQL_DESC_NAME: SqlUSmallInt = 1011;
pub const SQL_DESC_UNNAMED: SqlUSmallInt = 1012;
pub const SQL_DESC_OCTET_LENGTH: SqlUSmallInt = 1013;

pub const SQL_PRED_NONE: isize = 0;
pub const SQL_PRED_CHAR: isize = 1;
pub const SQL_PRED_BASIC: isize = 2;
pub const SQL_PRED_SEARCHABLE: isize = 3;
pub const SQL_ATTR_READONLY: isize = 0;
pub const SQL_NAMED: isize = 0;
pub const SQL_UNNAMED: isize = 1;

// SQLGetFunctions
pub const SQL_API_ALL_FUNCTIONS: SqlUSmallInt = 0;
pub const SQL_API_ODBC3_ALL_FUNCTIONS: SqlUSmallInt = 999;
pub const SQL_API_ODBC3_ALL_FUNCTIONS_SIZE: usize = 250;

/// The ODBC API functions this driver implements (SQL_API_* values).
pub const IMPLEMENTED_FUNCTIONS: &[SqlUSmallInt] = &[
    4,    // SQLBindCol
    5,    // SQLCancel
    6,    // SQLColAttribute
    7,    // SQLConnect
    8,    // SQLDescribeCol
    9,    // SQLDisconnect
    11,   // SQLExecDirect
    12,   // SQLExecute
    13,   // SQLFetch
    16,   // SQLFreeStmt
    18,   // SQLNumResultCols
    19,   // SQLPrepare
    20,   // SQLRowCount
    40,   // SQLColumns
    41,   // SQLDriverConnect
    43,   // SQLGetData
    44,   // SQLGetFunctions
    45,   // SQLGetInfo
    47,   // SQLGetTypeInfo
    52,   // SQLSpecialColumns
    53,   // SQLStatistics
    54,   // SQLTables
    60,   // SQLForeignKeys
    61,   // SQLMoreResults
    62,   // SQLNativeSql
    63,   // SQLNumParams
    65,   // SQLPrimaryKeys
    67,   // SQLProcedures
    72,   // SQLBindParameter
    1001, // SQLAllocHandle
    1003, // SQLCloseCursor
    1005, // SQLEndTran
    1006, // SQLFreeHandle
    1007, // SQLGetConnectAttr
    1010, // SQLGetDiagField
    1011, // SQLGetDiagRec
    1012, // SQLGetEnvAttr
    1014, // SQLGetStmtAttr
    1016, // SQLSetConnectAttr
    1019, // SQLSetEnvAttr
    1020, // SQLSetStmtAttr
    1021, // SQLFetchScroll
];

/// DATE_STRUCT
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DateStruct {
    pub year: i16,
    pub month: u16,
    pub day: u16,
}

/// TIME_STRUCT
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TimeStruct {
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
}

/// TIMESTAMP_STRUCT (fraction in nanoseconds)
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TimestampStruct {
    pub year: i16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub fraction: u32,
}
