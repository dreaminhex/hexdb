// The exported ODBC functions. Functions that take or return text are the
// Unicode (W) versions; driver managers map ANSI calls onto them.

#![allow(non_snake_case, clippy::missing_safety_doc, clippy::too_many_arguments)]

use crate::catalog;
use crate::client::{Client, Settings};
use crate::ffi::*;
use crate::info::{self, Info};
use crate::stmt::StmtState;
use crate::text::{wide_in, wide_out_bytes, wide_out_chars};
use crate::*;
use std::sync::Arc;

const MESSAGE_PREFIX: &str = "[HexDB][ODBC] ";

// ---------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "system" fn SQLAllocHandle(handle_type: SqlSmallInt, input: SqlHandle, output: *mut SqlHandle) -> SqlReturn {
    if output.is_null() {
        return SQL_ERROR;
    }
    match handle_type {
        SQL_HANDLE_ENV => {
            *output = Handle::new(ENV_MAGIC, EnvState { odbc_version: SQL_OV_ODBC3 }) as SqlHandle;
            SQL_SUCCESS
        }
        SQL_HANDLE_DBC => {
            let Some(env) = Handle::<EnvState>::from_ptr(input, ENV_MAGIC) else { return SQL_INVALID_HANDLE };
            *output = Handle::new(DBC_MAGIC, DbcState { env: env as *const Env, client: None, login_timeout: 0 }) as SqlHandle;
            SQL_SUCCESS
        }
        SQL_HANDLE_STMT => dbc_call(input, |dbc, state, _| {
            if state.client.is_none() {
                return Err(OdbcError::new("08003", "Connection not open"));
            }
            *output = Handle::new(STMT_MAGIC, StmtState::new(dbc as *const Dbc)) as SqlHandle;
            Ok(Done::Ok)
        }),
        SQL_HANDLE_DESC => dbc_call(input, |_, _, _| Err(OdbcError::new("HYC00", "Explicit descriptors aren't supported."))),
        _ => SQL_ERROR,
    }
}

#[no_mangle]
pub unsafe extern "system" fn SQLFreeHandle(handle_type: SqlSmallInt, handle: SqlHandle) -> SqlReturn {
    match handle_type {
        SQL_HANDLE_ENV => {
            if Handle::<EnvState>::from_ptr(handle, ENV_MAGIC).is_none() {
                return SQL_INVALID_HANDLE;
            }
            drop(Box::from_raw(handle as *mut Env));
        }
        SQL_HANDLE_DBC => {
            if Handle::<DbcState>::from_ptr(handle, DBC_MAGIC).is_none() {
                return SQL_INVALID_HANDLE;
            }
            drop(Box::from_raw(handle as *mut Dbc));
        }
        SQL_HANDLE_STMT => {
            let Some(stmt) = Handle::<StmtState>::from_ptr(handle, STMT_MAGIC) else { return SQL_INVALID_HANDLE };
            let descriptors = stmt.state.lock().unwrap_or_else(|e| e.into_inner()).descriptors;
            for d in descriptors.into_iter().filter(|d| !d.is_null()) {
                drop(Box::from_raw(d));
            }
            drop(Box::from_raw(handle as *mut Stmt));
        }
        SQL_HANDLE_DESC => return SQL_ERROR, // implicit descriptors are freed with their statement
        _ => return SQL_INVALID_HANDLE,
    }
    SQL_SUCCESS
}

#[no_mangle]
pub unsafe extern "system" fn SQLFreeStmt(stmt: SqlHandle, option: SqlUSmallInt) -> SqlReturn {
    if option == SQL_DROP {
        return SQLFreeHandle(SQL_HANDLE_STMT, stmt);
    }
    stmt_call(stmt, |_, s, _| {
        match option {
            SQL_CLOSE => s.close_cursor(),
            SQL_UNBIND => s.columns.clear(),
            SQL_RESET_PARAMS => s.params.clear(),
            _ => return Err(OdbcError::new("HY092", "Invalid option")),
        }
        Ok(Done::Ok)
    })
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "system" fn SQLSetEnvAttr(env: SqlHandle, attribute: SqlInteger, value: SqlPointer, _len: SqlInteger) -> SqlReturn {
    env_call(env, |_, state, _| {
        match attribute {
            SQL_ATTR_ODBC_VERSION => state.odbc_version = value as usize as i32,
            SQL_ATTR_CONNECTION_POOLING | SQL_ATTR_CP_MATCH | SQL_ATTR_OUTPUT_NTS => {}
            _ => return Err(OdbcError::new("HY092", format!("Invalid environment attribute {}", attribute))),
        }
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLGetEnvAttr(env: SqlHandle, attribute: SqlInteger, value: SqlPointer, _len: SqlInteger, out_len: *mut SqlInteger) -> SqlReturn {
    env_call(env, |_, state, _| {
        let v: i32 = match attribute {
            SQL_ATTR_ODBC_VERSION => state.odbc_version,
            SQL_ATTR_CONNECTION_POOLING | SQL_ATTR_CP_MATCH => 0,
            SQL_ATTR_OUTPUT_NTS => 1,
            _ => return Err(OdbcError::new("HY092", format!("Invalid environment attribute {}", attribute))),
        };
        if !value.is_null() {
            std::ptr::write_unaligned(value as *mut i32, v);
        }
        if !out_len.is_null() {
            *out_len = 4;
        }
        Ok(Done::Ok)
    })
}

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

fn connect(state: &mut DbcState, settings: Settings) -> Result<(), OdbcError> {
    if state.client.is_some() {
        return Err(OdbcError::new("08002", "Connection name in use: the connection is already open."));
    }
    state.client = Some(Arc::new(Client::connect(settings)?));
    Ok(())
}

#[no_mangle]
pub unsafe extern "system" fn SQLConnectW(
    dbc: SqlHandle,
    dsn: *const SqlWChar,
    dsn_len: SqlSmallInt,
    user: *const SqlWChar,
    user_len: SqlSmallInt,
    password: *const SqlWChar,
    password_len: SqlSmallInt,
) -> SqlReturn {
    dbc_call(dbc, |_, state, _| {
        let dsn = wide_in(dsn, dsn_len as isize).unwrap_or_default();
        let settings = Settings::resolve(&format!("DSN={}", crate::text::connection_value(&dsn)), wide_in(user, user_len as isize), wide_in(password, password_len as isize))?;
        connect(state, settings)?;
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLDriverConnectW(
    dbc: SqlHandle,
    _window: SqlHandle,
    input: *const SqlWChar,
    input_len: SqlSmallInt,
    output: *mut SqlWChar,
    output_max: SqlSmallInt,
    output_len: *mut SqlSmallInt,
    _completion: SqlUSmallInt,
) -> SqlReturn {
    dbc_call(dbc, |_, state, warnings| {
        let text = wide_in(input, input_len as isize).unwrap_or_default();
        // The driver has no dialog: whatever the completion mode, the
        // connection string (and DSN) must hold everything needed.
        let settings = Settings::resolve(&text, None, None)?;
        let completed = settings.connection_string();
        connect(state, settings)?;
        truncation(wide_out_chars(&completed, output, output_max as isize, output_len), warnings);
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLDisconnect(dbc: SqlHandle) -> SqlReturn {
    dbc_call(dbc, |_, state, _| {
        if state.client.take().is_none() {
            return Err(OdbcError::new("08003", "Connection not open"));
        }
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLGetInfoW(dbc: SqlHandle, info_type: SqlUSmallInt, value: SqlPointer, buffer_len: SqlSmallInt, out_len: *mut SqlSmallInt) -> SqlReturn {
    dbc_call(dbc, |_, state, warnings| {
        let client = state.client.as_deref();
        match info::get(info_type, client) {
            Some(Info::Str(s)) => truncation(wide_out_bytes(&s, value as *mut SqlWChar, buffer_len as isize, out_len), warnings),
            Some(Info::U16(v)) => {
                if !value.is_null() {
                    std::ptr::write_unaligned(value as *mut u16, v);
                }
                if !out_len.is_null() {
                    *out_len = 2;
                }
            }
            Some(Info::U32(v)) => {
                if !value.is_null() {
                    std::ptr::write_unaligned(value as *mut u32, v);
                }
                if !out_len.is_null() {
                    *out_len = 4;
                }
            }
            None => return Err(OdbcError::new("HY096", format!("Information type {} isn't supported.", info_type))),
        }
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLGetFunctions(dbc: SqlHandle, function: SqlUSmallInt, supported: *mut SqlUSmallInt) -> SqlReturn {
    dbc_call(dbc, |_, _, _| {
        if supported.is_null() {
            return Ok(Done::Ok);
        }
        match function {
            SQL_API_ODBC3_ALL_FUNCTIONS => {
                let bits = std::slice::from_raw_parts_mut(supported, SQL_API_ODBC3_ALL_FUNCTIONS_SIZE);
                bits.fill(0);
                for f in IMPLEMENTED_FUNCTIONS {
                    bits[(*f >> 4) as usize] |= 1 << (*f & 0xF);
                }
            }
            SQL_API_ALL_FUNCTIONS => {
                let flags = std::slice::from_raw_parts_mut(supported, 100);
                for (i, flag) in flags.iter_mut().enumerate() {
                    *flag = IMPLEMENTED_FUNCTIONS.contains(&(i as u16)) as u16;
                }
            }
            f => *supported = IMPLEMENTED_FUNCTIONS.contains(&f) as u16,
        }
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLSetConnectAttrW(dbc: SqlHandle, attribute: SqlInteger, value: SqlPointer, _len: SqlInteger) -> SqlReturn {
    // SQL_ATTR_ANSI_APP: returning an error tells the driver manager the
    // driver behaves the same for ANSI and Unicode applications.
    if attribute == SQL_ATTR_ANSI_APP {
        return SQL_ERROR;
    }
    dbc_call(dbc, |_, state, warnings| {
        match attribute {
            SQL_ATTR_LOGIN_TIMEOUT => state.login_timeout = value as usize as u64,
            SQL_ATTR_ACCESS_MODE | SQL_ATTR_TXN_ISOLATION | SQL_ATTR_CONNECTION_TIMEOUT | SQL_ATTR_CURRENT_CATALOG | SQL_ATTR_METADATA_ID => {}
            SQL_ATTR_AUTOCOMMIT if value as usize != SQL_AUTOCOMMIT_ON => {
                warnings.push(OdbcError::new("01S02", "HexDB's SQL is read-only; autocommit stays on."));
            }
            SQL_ATTR_AUTOCOMMIT => {}
            // Driver-manager and tracing attributes the driver doesn't need.
            _ => {}
        }
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLGetConnectAttrW(dbc: SqlHandle, attribute: SqlInteger, value: SqlPointer, buffer_len: SqlInteger, out_len: *mut SqlInteger) -> SqlReturn {
    dbc_call(dbc, |_, state, warnings| {
        let number: u32 = match attribute {
            SQL_ATTR_CURRENT_CATALOG => {
                truncation(wide_out_bytes("", value as *mut SqlWChar, buffer_len as isize, out_len), warnings);
                return Ok(Done::Ok);
            }
            SQL_ATTR_ACCESS_MODE => SQL_MODE_READ_ONLY as u32,
            SQL_ATTR_AUTOCOMMIT => SQL_AUTOCOMMIT_ON as u32,
            SQL_ATTR_LOGIN_TIMEOUT => state.login_timeout as u32,
            SQL_ATTR_CONNECTION_TIMEOUT | SQL_ATTR_METADATA_ID | SQL_ATTR_AUTO_IPD => 0,
            SQL_ATTR_TXN_ISOLATION => 0,
            SQL_ATTR_CONNECTION_DEAD => state.client.is_none() as u32,
            _ => return Err(OdbcError::new("HY092", format!("Invalid connection attribute {}", attribute))),
        };
        if !value.is_null() {
            std::ptr::write_unaligned(value as *mut u32, number);
        }
        if !out_len.is_null() {
            *out_len = 4;
        }
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLEndTran(handle_type: SqlSmallInt, handle: SqlHandle, _completion: SqlSmallInt) -> SqlReturn {
    // Nothing to commit: HexDB's SQL is read-only.
    match handle_type {
        SQL_HANDLE_ENV => env_call(handle, |_, _, _| Ok(Done::Ok)),
        SQL_HANDLE_DBC => dbc_call(handle, |_, _, _| Ok(Done::Ok)),
        _ => SQL_INVALID_HANDLE,
    }
}

#[no_mangle]
pub unsafe extern "system" fn SQLNativeSqlW(
    dbc: SqlHandle,
    input: *const SqlWChar,
    input_len: SqlInteger,
    output: *mut SqlWChar,
    output_max: SqlInteger,
    output_len: *mut SqlInteger,
) -> SqlReturn {
    dbc_call(dbc, |_, _, warnings| {
        let text = wide_in(input, input_len as isize).ok_or_else(|| OdbcError::new("HY009", "Invalid use of null pointer"))?;
        truncation(wide_out_chars(&crate::text::rewrite_escapes(&text), output, output_max as isize, output_len), warnings);
        Ok(Done::Ok)
    })
}

// ---------------------------------------------------------------------------
// Statements
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "system" fn SQLPrepareW(stmt: SqlHandle, text: *const SqlWChar, len: SqlInteger) -> SqlReturn {
    stmt_call(stmt, |_, s, _| {
        let sql = wide_in(text, len as isize).ok_or_else(|| OdbcError::new("HY009", "Invalid use of null pointer"))?;
        s.prepare(&sql);
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLExecute(stmt: SqlHandle) -> SqlReturn {
    stmt_call(stmt, |_, s, _| {
        if s.sql.is_none() {
            return Err(OdbcError::new("HY010", "Function sequence error: no statement has been prepared."));
        }
        s.execute()?;
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLExecDirectW(stmt: SqlHandle, text: *const SqlWChar, len: SqlInteger) -> SqlReturn {
    stmt_call(stmt, |_, s, _| {
        let sql = wide_in(text, len as isize).ok_or_else(|| OdbcError::new("HY009", "Invalid use of null pointer"))?;
        s.prepare(&sql);
        s.execute()?;
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLNumParams(stmt: SqlHandle, count: *mut SqlSmallInt) -> SqlReturn {
    stmt_call(stmt, |_, s, _| {
        if !count.is_null() {
            *count = s.parameter_count() as SqlSmallInt;
        }
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLBindParameter(
    stmt: SqlHandle,
    number: SqlUSmallInt,
    io_type: SqlSmallInt,
    c_type: SqlSmallInt,
    sql_type: SqlSmallInt,
    _column_size: SqlULen,
    _digits: SqlSmallInt,
    ptr: SqlPointer,
    buffer_len: SqlLen,
    ind: *mut SqlLen,
) -> SqlReturn {
    stmt_call(stmt, |_, s, _| {
        const SQL_PARAM_INPUT: SqlSmallInt = 1;
        if number == 0 {
            return Err(OdbcError::new("07009", "Invalid descriptor index 0"));
        }
        if io_type != SQL_PARAM_INPUT {
            return Err(OdbcError::new("HYC00", "Only input parameters are supported."));
        }
        s.params.insert(number, crate::convert::ParamBinding { c_type, sql_type, ptr, buffer_len, ind });
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLNumResultCols(stmt: SqlHandle, count: *mut SqlSmallInt) -> SqlReturn {
    stmt_call(stmt, |_, s, _| {
        let n = s.columns()?.len();
        if !count.is_null() {
            *count = n as SqlSmallInt;
        }
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLDescribeColW(
    stmt: SqlHandle,
    number: SqlUSmallInt,
    name: *mut SqlWChar,
    name_max: SqlSmallInt,
    name_len: *mut SqlSmallInt,
    data_type: *mut SqlSmallInt,
    size: *mut SqlULen,
    digits: *mut SqlSmallInt,
    nullable: *mut SqlSmallInt,
) -> SqlReturn {
    stmt_call(stmt, |_, s, warnings| {
        let column = s.column(number)?;
        truncation(wide_out_chars(&column.name, name, name_max as isize, name_len), warnings);
        if !data_type.is_null() {
            *data_type = column.sql_type;
        }
        if !size.is_null() {
            *size = column.size;
        }
        if !digits.is_null() {
            *digits = column.digits;
        }
        if !nullable.is_null() {
            *nullable = column.nullable;
        }
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLColAttributeW(
    stmt: SqlHandle,
    number: SqlUSmallInt,
    field: SqlUSmallInt,
    text: SqlPointer,
    buffer_len: SqlSmallInt,
    text_len: *mut SqlSmallInt,
    numeric: *mut SqlLen,
) -> SqlReturn {
    stmt_call(stmt, |_, s, warnings| {
        let put_number = |v: isize| {
            if !numeric.is_null() {
                *numeric = v;
            }
            Ok(Done::Ok)
        };
        if field == SQL_DESC_COUNT || field == SQL_COLUMN_COUNT {
            return put_number(s.columns()?.len() as isize);
        }
        let c = s.column(number)?;
        let string = match field {
            SQL_DESC_NAME | SQL_COLUMN_NAME | SQL_DESC_LABEL | SQL_DESC_BASE_COLUMN_NAME => Some(c.name.clone()),
            SQL_DESC_TYPE_NAME | SQL_DESC_LOCAL_TYPE_NAME => Some(c.type_name().to_string()),
            SQL_DESC_TABLE_NAME | SQL_DESC_BASE_TABLE_NAME => Some(c.table.clone()),
            SQL_DESC_SCHEMA_NAME | SQL_DESC_CATALOG_NAME => Some(String::new()),
            SQL_DESC_LITERAL_PREFIX | SQL_DESC_LITERAL_SUFFIX => {
                Some(if matches!(c.sql_type, SQL_WVARCHAR | SQL_WLONGVARCHAR) { "'".to_string() } else { String::new() })
            }
            _ => None,
        };
        if let Some(string) = string {
            truncation(wide_out_bytes(&string, text as *mut SqlWChar, buffer_len as isize, text_len), warnings);
            return Ok(Done::Ok);
        }
        let is_text = matches!(c.sql_type, SQL_WVARCHAR | SQL_WLONGVARCHAR);
        let is_number = matches!(c.sql_type, SQL_BIGINT | SQL_DOUBLE | SQL_SMALLINT | SQL_INTEGER);
        put_number(match field {
            SQL_DESC_TYPE | SQL_DESC_CONCISE_TYPE => c.sql_type as isize,
            SQL_DESC_LENGTH | SQL_COLUMN_LENGTH | SQL_DESC_PRECISION | SQL_COLUMN_PRECISION => c.size.min(isize::MAX as usize) as isize,
            SQL_DESC_SCALE | SQL_COLUMN_SCALE => c.digits as isize,
            SQL_DESC_DISPLAY_SIZE => c.display_size().min(isize::MAX as usize) as isize,
            SQL_DESC_OCTET_LENGTH => c.octet_length().min(isize::MAX as usize) as isize,
            SQL_DESC_NULLABLE | SQL_COLUMN_NULLABLE => c.nullable as isize,
            SQL_DESC_UNSIGNED => (!is_number) as isize,
            SQL_DESC_UPDATABLE => SQL_ATTR_READONLY,
            SQL_DESC_CASE_SENSITIVE => is_text as isize,
            SQL_DESC_SEARCHABLE => match c.sql_type {
                SQL_WVARCHAR => SQL_PRED_SEARCHABLE,
                SQL_WLONGVARCHAR => SQL_PRED_NONE,
                _ => SQL_PRED_BASIC,
            },
            SQL_DESC_NUM_PREC_RADIX => match c.sql_type {
                SQL_BIGINT | SQL_SMALLINT | SQL_INTEGER => 10,
                SQL_DOUBLE => 2,
                _ => 0,
            },
            SQL_DESC_UNNAMED => SQL_NAMED,
            SQL_DESC_FIXED_PREC_SCALE | SQL_DESC_AUTO_UNIQUE_VALUE | SQL_DESC_DATETIME_INTERVAL_CODE => 0,
            _ => 0,
        })
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLBindCol(stmt: SqlHandle, number: SqlUSmallInt, c_type: SqlSmallInt, ptr: SqlPointer, buffer_len: SqlLen, ind: *mut SqlLen) -> SqlReturn {
    stmt_call(stmt, |_, s, _| {
        if number == 0 {
            return Err(OdbcError::new("07009", "Bookmark columns aren't supported."));
        }
        if ptr.is_null() && ind.is_null() {
            s.columns.remove(&number);
        } else {
            s.columns.insert(number, crate::stmt::ColBinding { c_type, ptr, buffer_len, ind });
        }
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLFetch(stmt: SqlHandle) -> SqlReturn {
    stmt_call(stmt, |_, s, warnings| s.fetch(warnings))
}

#[no_mangle]
pub unsafe extern "system" fn SQLFetchScroll(stmt: SqlHandle, orientation: SqlSmallInt, _offset: SqlLen) -> SqlReturn {
    stmt_call(stmt, |_, s, warnings| {
        if orientation != SQL_FETCH_NEXT {
            return Err(OdbcError::new("HY106", "Only SQL_FETCH_NEXT is supported: cursors are forward-only."));
        }
        s.fetch(warnings)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLGetData(stmt: SqlHandle, number: SqlUSmallInt, c_type: SqlSmallInt, ptr: SqlPointer, buffer_len: SqlLen, ind: *mut SqlLen) -> SqlReturn {
    stmt_call(stmt, |_, s, warnings| s.get_data(number, c_type, ptr, buffer_len, ind, warnings))
}

#[no_mangle]
pub unsafe extern "system" fn SQLRowCount(stmt: SqlHandle, count: *mut SqlLen) -> SqlReturn {
    // SELECT doesn't report a row count.
    stmt_call(stmt, |_, _, _| {
        if !count.is_null() {
            *count = -1;
        }
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLMoreResults(stmt: SqlHandle) -> SqlReturn {
    stmt_call(stmt, |_, s, _| {
        s.close_cursor();
        Ok(Done::NoData)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLCloseCursor(stmt: SqlHandle) -> SqlReturn {
    stmt_call(stmt, |_, s, _| {
        if s.result.is_none() {
            return Err(OdbcError::new("24000", "Invalid cursor state: no cursor is open."));
        }
        s.close_cursor();
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLCancel(stmt: SqlHandle) -> SqlReturn {
    // Requests run synchronously; there's nothing in flight to cancel.
    stmt_call(stmt, |_, _, _| Ok(Done::Ok))
}

#[no_mangle]
pub unsafe extern "system" fn SQLSetStmtAttrW(stmt: SqlHandle, attribute: SqlInteger, value: SqlPointer, _len: SqlInteger) -> SqlReturn {
    stmt_call(stmt, |_, s, warnings| {
        let n = value as usize;
        match attribute {
            SQL_ATTR_ROW_ARRAY_SIZE | SQL_ROWSET_SIZE => {
                if n == 0 {
                    return Err(OdbcError::new("HY024", "The row array size must be at least 1."));
                }
                s.row_array_size = n;
            }
            SQL_ATTR_ROWS_FETCHED_PTR => s.rows_fetched = value as *mut SqlULen,
            SQL_ATTR_ROW_STATUS_PTR => s.row_status = value as *mut SqlUSmallInt,
            SQL_ATTR_ROW_BIND_TYPE => s.row_bind_type = n,
            SQL_ATTR_ROW_BIND_OFFSET_PTR => s.row_bind_offset = value as *mut SqlLen,
            SQL_ATTR_MAX_ROWS => s.max_rows = n,
            SQL_ATTR_QUERY_TIMEOUT => s.query_timeout = n,
            SQL_ATTR_METADATA_ID => s.metadata_id = n,
            SQL_ATTR_PARAMSET_SIZE => s.paramset_size = n.max(1),
            SQL_ATTR_CURSOR_TYPE if n != SQL_CURSOR_FORWARD_ONLY => warnings.push(OdbcError::new("01S02", "Option value changed: cursors are forward-only.")),
            SQL_ATTR_CONCURRENCY if n != SQL_CONCUR_READ_ONLY => warnings.push(OdbcError::new("01S02", "Option value changed: cursors are read-only.")),
            SQL_ATTR_CURSOR_SCROLLABLE if n != 0 => return Err(OdbcError::new("HYC00", "Scrollable cursors aren't supported.")),
            SQL_ATTR_ASYNC_ENABLE if n != 0 => return Err(OdbcError::new("HYC00", "Asynchronous execution isn't supported.")),
            SQL_ATTR_USE_BOOKMARKS if n != 0 => return Err(OdbcError::new("HYC00", "Bookmarks aren't supported.")),
            SQL_ATTR_CURSOR_TYPE
            | SQL_ATTR_CONCURRENCY
            | SQL_ATTR_CURSOR_SCROLLABLE
            | SQL_ATTR_ASYNC_ENABLE
            | SQL_ATTR_USE_BOOKMARKS
            | SQL_ATTR_CURSOR_SENSITIVITY
            | SQL_ATTR_NOSCAN
            | SQL_ATTR_MAX_LENGTH
            | SQL_ATTR_RETRIEVE_DATA
            | SQL_ATTR_KEYSET_SIZE
            | SQL_ATTR_SIMULATE_CURSOR
            | SQL_ATTR_ENABLE_AUTO_IPD
            | SQL_ATTR_PARAM_BIND_OFFSET_PTR
            | SQL_ATTR_PARAM_BIND_TYPE
            | SQL_ATTR_PARAM_OPERATION_PTR
            | SQL_ATTR_PARAM_STATUS_PTR
            | SQL_ATTR_PARAMS_PROCESSED_PTR
            | SQL_ATTR_ROW_OPERATION_PTR
            | SQL_ATTR_FETCH_BOOKMARK_PTR
            | SQL_ATTR_APP_ROW_DESC
            | SQL_ATTR_APP_PARAM_DESC => {}
            _ => return Err(OdbcError::new("HY092", format!("Invalid statement attribute {}", attribute))),
        }
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLGetStmtAttrW(stmt: SqlHandle, attribute: SqlInteger, value: SqlPointer, _buffer_len: SqlInteger, out_len: *mut SqlInteger) -> SqlReturn {
    stmt_call(stmt, |_, s, _| {
        let v: usize = match attribute {
            SQL_ATTR_APP_ROW_DESC | SQL_ATTR_APP_PARAM_DESC | SQL_ATTR_IMP_ROW_DESC | SQL_ATTR_IMP_PARAM_DESC => {
                let i = (attribute - SQL_ATTR_APP_ROW_DESC) as usize;
                if s.descriptors[i].is_null() {
                    s.descriptors[i] = Handle::new(DESC_MAGIC, ());
                }
                s.descriptors[i] as usize
            }
            SQL_ATTR_ROW_ARRAY_SIZE | SQL_ROWSET_SIZE => s.row_array_size,
            SQL_ATTR_ROWS_FETCHED_PTR => s.rows_fetched as usize,
            SQL_ATTR_ROW_STATUS_PTR => s.row_status as usize,
            SQL_ATTR_ROW_BIND_TYPE => s.row_bind_type,
            SQL_ATTR_ROW_BIND_OFFSET_PTR => s.row_bind_offset as usize,
            SQL_ATTR_MAX_ROWS => s.max_rows,
            SQL_ATTR_QUERY_TIMEOUT => s.query_timeout,
            SQL_ATTR_METADATA_ID => s.metadata_id,
            SQL_ATTR_PARAMSET_SIZE => s.paramset_size,
            SQL_ATTR_CURSOR_TYPE => SQL_CURSOR_FORWARD_ONLY,
            SQL_ATTR_CONCURRENCY => SQL_CONCUR_READ_ONLY,
            SQL_ATTR_RETRIEVE_DATA => 1,
            SQL_ATTR_CURSOR_SCROLLABLE
            | SQL_ATTR_CURSOR_SENSITIVITY
            | SQL_ATTR_ASYNC_ENABLE
            | SQL_ATTR_USE_BOOKMARKS
            | SQL_ATTR_NOSCAN
            | SQL_ATTR_MAX_LENGTH
            | SQL_ATTR_KEYSET_SIZE
            | SQL_ATTR_SIMULATE_CURSOR
            | SQL_ATTR_ENABLE_AUTO_IPD
            | SQL_ATTR_ROW_NUMBER
            | SQL_ATTR_PARAM_BIND_TYPE => 0,
            SQL_ATTR_PARAM_BIND_OFFSET_PTR | SQL_ATTR_PARAM_OPERATION_PTR | SQL_ATTR_PARAM_STATUS_PTR | SQL_ATTR_PARAMS_PROCESSED_PTR
            | SQL_ATTR_ROW_OPERATION_PTR | SQL_ATTR_FETCH_BOOKMARK_PTR => 0,
            _ => return Err(OdbcError::new("HY092", format!("Invalid statement attribute {}", attribute))),
        };
        if !value.is_null() {
            std::ptr::write_unaligned(value as *mut usize, v);
        }
        if !out_len.is_null() {
            *out_len = std::mem::size_of::<usize>() as SqlInteger;
        }
        Ok(Done::Ok)
    })
}

// ---------------------------------------------------------------------------
// Catalog functions
// ---------------------------------------------------------------------------

/// Run a catalog function and show its result on the statement.
fn catalog_call(stmt: SqlHandle, f: impl FnOnce(&Client, bool) -> Result<crate::stmt::ResultSet, OdbcError>) -> SqlReturn {
    stmt_call(stmt, |_, s, _| {
        let client = s.client()?;
        let literal = s.metadata_id != 0;
        let result = f(&client, literal)?;
        s.set_local(result);
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLTablesW(
    stmt: SqlHandle,
    catalog: *const SqlWChar,
    catalog_len: SqlSmallInt,
    schema: *const SqlWChar,
    schema_len: SqlSmallInt,
    table: *const SqlWChar,
    table_len: SqlSmallInt,
    types: *const SqlWChar,
    types_len: SqlSmallInt,
) -> SqlReturn {
    let (c, s, t, ty) = (wide_in(catalog, catalog_len as isize), wide_in(schema, schema_len as isize), wide_in(table, table_len as isize), wide_in(types, types_len as isize));
    catalog_call(stmt, |client, literal| catalog::tables(client, c, s, t, ty, literal))
}

#[no_mangle]
pub unsafe extern "system" fn SQLColumnsW(
    stmt: SqlHandle,
    catalog: *const SqlWChar,
    catalog_len: SqlSmallInt,
    schema: *const SqlWChar,
    schema_len: SqlSmallInt,
    table: *const SqlWChar,
    table_len: SqlSmallInt,
    column: *const SqlWChar,
    column_len: SqlSmallInt,
) -> SqlReturn {
    let (c, s, t, col) = (wide_in(catalog, catalog_len as isize), wide_in(schema, schema_len as isize), wide_in(table, table_len as isize), wide_in(column, column_len as isize));
    catalog_call(stmt, |client, literal| catalog::columns(client, c, s, t, col, literal))
}

#[no_mangle]
pub unsafe extern "system" fn SQLGetTypeInfoW(stmt: SqlHandle, data_type: SqlSmallInt) -> SqlReturn {
    stmt_call(stmt, |_, s, _| {
        s.set_local(catalog::type_info(data_type));
        Ok(Done::Ok)
    })
}

#[no_mangle]
pub unsafe extern "system" fn SQLPrimaryKeysW(
    stmt: SqlHandle,
    _catalog: *const SqlWChar,
    _catalog_len: SqlSmallInt,
    _schema: *const SqlWChar,
    _schema_len: SqlSmallInt,
    table: *const SqlWChar,
    table_len: SqlSmallInt,
) -> SqlReturn {
    let t = wide_in(table, table_len as isize);
    catalog_call(stmt, |client, _| catalog::primary_keys(client, t))
}

#[no_mangle]
pub unsafe extern "system" fn SQLSpecialColumnsW(
    stmt: SqlHandle,
    identifier_type: SqlUSmallInt,
    _catalog: *const SqlWChar,
    _catalog_len: SqlSmallInt,
    _schema: *const SqlWChar,
    _schema_len: SqlSmallInt,
    table: *const SqlWChar,
    table_len: SqlSmallInt,
    _scope: SqlUSmallInt,
    _nullable: SqlUSmallInt,
) -> SqlReturn {
    let t = wide_in(table, table_len as isize);
    catalog_call(stmt, |client, _| catalog::special_columns(client, identifier_type, t))
}

#[no_mangle]
pub unsafe extern "system" fn SQLForeignKeysW(
    stmt: SqlHandle,
    _pk_catalog: *const SqlWChar,
    _pk_catalog_len: SqlSmallInt,
    _pk_schema: *const SqlWChar,
    _pk_schema_len: SqlSmallInt,
    _pk_table: *const SqlWChar,
    _pk_table_len: SqlSmallInt,
    _fk_catalog: *const SqlWChar,
    _fk_catalog_len: SqlSmallInt,
    _fk_schema: *const SqlWChar,
    _fk_schema_len: SqlSmallInt,
    _fk_table: *const SqlWChar,
    _fk_table_len: SqlSmallInt,
) -> SqlReturn {
    catalog_call(stmt, |_, _| Ok(catalog::foreign_keys()))
}

#[no_mangle]
pub unsafe extern "system" fn SQLStatisticsW(
    stmt: SqlHandle,
    _catalog: *const SqlWChar,
    _catalog_len: SqlSmallInt,
    _schema: *const SqlWChar,
    _schema_len: SqlSmallInt,
    _table: *const SqlWChar,
    _table_len: SqlSmallInt,
    _unique: SqlUSmallInt,
    _reserved: SqlUSmallInt,
) -> SqlReturn {
    catalog_call(stmt, |_, _| Ok(catalog::statistics()))
}

#[no_mangle]
pub unsafe extern "system" fn SQLProceduresW(
    stmt: SqlHandle,
    _catalog: *const SqlWChar,
    _catalog_len: SqlSmallInt,
    _schema: *const SqlWChar,
    _schema_len: SqlSmallInt,
    _procedure: *const SqlWChar,
    _procedure_len: SqlSmallInt,
) -> SqlReturn {
    catalog_call(stmt, |_, _| Ok(catalog::procedures()))
}

// ---------------------------------------------------------------------------
// Diagnostics (these read a handle's diagnostics without clearing them)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "system" fn SQLGetDiagRecW(
    handle_type: SqlSmallInt,
    handle: SqlHandle,
    record: SqlSmallInt,
    state: *mut SqlWChar,
    native: *mut SqlInteger,
    message: *mut SqlWChar,
    message_max: SqlSmallInt,
    message_len: *mut SqlSmallInt,
) -> SqlReturn {
    let Some(diags) = diagnostics_of(handle_type, handle) else { return SQL_INVALID_HANDLE };
    if record < 1 || message_max < 0 {
        return SQL_ERROR;
    }
    let Some(d) = diags.get(record as usize - 1) else { return SQL_NO_DATA };
    if !state.is_null() {
        for (i, u) in d.state.encode_utf16().take(5).enumerate() {
            *state.add(i) = u;
        }
        *state.add(5) = 0;
    }
    if !native.is_null() {
        *native = d.native;
    }
    let truncated = wide_out_chars(&format!("{}{}", MESSAGE_PREFIX, d.message), message, message_max as isize, message_len);
    if truncated {
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
    }
}

#[no_mangle]
pub unsafe extern "system" fn SQLGetDiagFieldW(
    handle_type: SqlSmallInt,
    handle: SqlHandle,
    record: SqlSmallInt,
    field: SqlSmallInt,
    info: SqlPointer,
    buffer_len: SqlSmallInt,
    out_len: *mut SqlSmallInt,
) -> SqlReturn {
    let Some(diags) = diagnostics_of(handle_type, handle) else { return SQL_INVALID_HANDLE };
    let text = |s: &str| {
        if wide_out_bytes(s, info as *mut SqlWChar, buffer_len as isize, out_len) {
            SQL_SUCCESS_WITH_INFO
        } else {
            SQL_SUCCESS
        }
    };
    macro_rules! number {
        ($t:ty, $v:expr) => {{
            if !info.is_null() {
                std::ptr::write_unaligned(info as *mut $t, $v);
            }
            SQL_SUCCESS
        }};
    }
    // Header fields.
    match field {
        SQL_DIAG_NUMBER => return number!(SqlInteger, diags.len() as SqlInteger),
        SQL_DIAG_RETURNCODE => return number!(SqlReturn, if diags.is_empty() { SQL_SUCCESS } else { SQL_ERROR }),
        SQL_DIAG_CURSOR_ROW_COUNT | SQL_DIAG_ROW_COUNT => return number!(SqlLen, -1),
        SQL_DIAG_DYNAMIC_FUNCTION => return text(""),
        SQL_DIAG_DYNAMIC_FUNCTION_CODE => return number!(SqlInteger, 0),
        _ => {}
    }
    if record < 1 {
        return SQL_ERROR;
    }
    let Some(d) = diags.get(record as usize - 1) else { return SQL_NO_DATA };
    match field {
        SQL_DIAG_SQLSTATE => text(d.state),
        SQL_DIAG_MESSAGE_TEXT => text(&format!("{}{}", MESSAGE_PREFIX, d.message)),
        SQL_DIAG_NATIVE => number!(SqlInteger, d.native),
        SQL_DIAG_CLASS_ORIGIN | SQL_DIAG_SUBCLASS_ORIGIN => {
            let odbc = d.state.starts_with("IM") || d.state.starts_with("HY") || d.state == "01S02" || d.state == "01S07" || d.state == "07009";
            text(if odbc { "ODBC 3.0" } else { "ISO 9075" })
        }
        SQL_DIAG_CONNECTION_NAME | SQL_DIAG_SERVER_NAME => text(""),
        SQL_DIAG_ROW_NUMBER => number!(SqlLen, -2),
        SQL_DIAG_COLUMN_NUMBER => number!(SqlInteger, -2),
        _ => SQL_ERROR,
    }
}
