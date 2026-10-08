//! The ODBC driver against a real server, called the way a driver manager
//! calls it: connect, catalog functions, queries with paging, parameters,
//! bound column arrays, SQLGetData in pieces, and diagnostics.

use anyhow::{bail, Result};
use hexdb_odbc::ffi::*;
use hexdb_odbc::*;
use hexdb_tests::{TestServer, TEST_ADMIN_LOGIN, TEST_ADMIN_PASSWORD};
use serde_json::json;
use std::ffi::c_void;
use std::ptr::{null, null_mut};

/// A NUL-terminated UTF-16 string.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

struct Odbc {
    env: SqlHandle,
    dbc: SqlHandle,
}

impl Drop for Odbc {
    fn drop(&mut self) {
        unsafe {
            SQLDisconnect(self.dbc);
            SQLFreeHandle(SQL_HANDLE_DBC, self.dbc);
            SQLFreeHandle(SQL_HANDLE_ENV, self.env);
        }
    }
}

fn diag(handle_type: SqlSmallInt, handle: SqlHandle) -> (String, String) {
    let mut state = [0u16; 6];
    let mut native = 0;
    let mut message = [0u16; 1024];
    let mut len = 0i16;
    let rc = unsafe { SQLGetDiagRecW(handle_type, handle, 1, state.as_mut_ptr(), &mut native, message.as_mut_ptr(), 1024, &mut len) };
    if rc == SQL_NO_DATA {
        return (String::new(), String::new());
    }
    (String::from_utf16_lossy(&state[..5]), String::from_utf16_lossy(&message[..len.max(0) as usize]))
}

fn check(rc: SqlReturn, handle_type: SqlSmallInt, handle: SqlHandle, what: &str) -> Result<()> {
    if rc == SQL_SUCCESS || rc == SQL_SUCCESS_WITH_INFO {
        return Ok(());
    }
    let (state, message) = diag(handle_type, handle);
    bail!("{} returned {}: {} {}", what, rc, state, message)
}

/// Connect with a connection string; returns the handles or the diagnostic.
fn connect(connection: &str) -> Result<Odbc, (String, String)> {
    unsafe {
        let mut env = null_mut();
        assert_eq!(SQLAllocHandle(SQL_HANDLE_ENV, null_mut(), &mut env), SQL_SUCCESS);
        assert_eq!(SQLSetEnvAttr(env, SQL_ATTR_ODBC_VERSION, 3 as SqlPointer, 0), SQL_SUCCESS);
        let mut dbc = null_mut();
        assert_eq!(SQLAllocHandle(SQL_HANDLE_DBC, env, &mut dbc), SQL_SUCCESS);
        let input = wide(connection);
        let mut out = [0u16; 512];
        let mut out_len = 0i16;
        let rc = SQLDriverConnectW(dbc, null_mut(), input.as_ptr(), SQL_NTS as i16, out.as_mut_ptr(), 512, &mut out_len, SQL_DRIVER_NOPROMPT);
        if rc != SQL_SUCCESS && rc != SQL_SUCCESS_WITH_INFO {
            let d = diag(SQL_HANDLE_DBC, dbc);
            SQLFreeHandle(SQL_HANDLE_DBC, dbc);
            SQLFreeHandle(SQL_HANDLE_ENV, env);
            return Err(d);
        }
        let completed = String::from_utf16_lossy(&out[..out_len as usize]);
        assert!(completed.contains("SERVER="), "{}", completed);
        assert!(!completed.to_ascii_uppercase().contains("APIKEY") && !completed.contains("PWD"), "secrets stay out: {}", completed);
        Ok(Odbc { env, dbc })
    }
}

impl Odbc {
    fn stmt(&self) -> SqlHandle {
        let mut stmt = null_mut();
        unsafe { assert_eq!(SQLAllocHandle(SQL_HANDLE_STMT, self.dbc, &mut stmt), SQL_SUCCESS) };
        stmt
    }

    fn query(&self, sql: &str) -> Result<SqlHandle> {
        let stmt = self.stmt();
        let text = wide(sql);
        check(unsafe { SQLExecDirectW(stmt, text.as_ptr(), SQL_NTS) }, SQL_HANDLE_STMT, stmt, sql)?;
        Ok(stmt)
    }
}

/// Every remaining row, each value read with SQLGetData as text (None for NULL).
fn rows(stmt: SqlHandle) -> Result<Vec<Vec<Option<String>>>> {
    let mut columns = 0i16;
    check(unsafe { SQLNumResultCols(stmt, &mut columns) }, SQL_HANDLE_STMT, stmt, "SQLNumResultCols")?;
    let mut out = Vec::new();
    loop {
        let rc = unsafe { SQLFetch(stmt) };
        if rc == SQL_NO_DATA {
            break;
        }
        check(rc, SQL_HANDLE_STMT, stmt, "SQLFetch")?;
        let mut row = Vec::new();
        for c in 1..=columns as u16 {
            row.push(get_text(stmt, c, 16)?);
        }
        out.push(row);
    }
    Ok(out)
}

/// One value as text, read `piece` characters at a time.
fn get_text(stmt: SqlHandle, column: u16, piece: usize) -> Result<Option<String>> {
    let mut text = Vec::new();
    loop {
        let mut buf = vec![0u16; piece + 1];
        let mut ind: SqlLen = 0;
        let rc = unsafe { SQLGetData(stmt, column, SQL_C_WCHAR, buf.as_mut_ptr() as *mut c_void, ((piece + 1) * 2) as SqlLen, &mut ind) };
        match rc {
            SQL_NO_DATA => break,
            SQL_SUCCESS | SQL_SUCCESS_WITH_INFO => {
                if ind == SQL_NULL_DATA {
                    return Ok(None);
                }
                let n = ((ind as usize) / 2).min(piece);
                text.extend_from_slice(&buf[..n]);
                if rc == SQL_SUCCESS {
                    break;
                }
            }
            _ => {
                let (state, message) = diag(SQL_HANDLE_STMT, stmt);
                bail!("SQLGetData: {} {}", state, message)
            }
        }
    }
    Ok(Some(String::from_utf16_lossy(&text)))
}

fn describe(stmt: SqlHandle, column: u16) -> (String, SqlSmallInt, SqlULen, SqlSmallInt) {
    let mut name = [0u16; 128];
    let (mut len, mut ty, mut size, mut digits, mut nullable) = (0i16, 0i16, 0usize, 0i16, 0i16);
    let rc = unsafe { SQLDescribeColW(stmt, column, name.as_mut_ptr(), 128, &mut len, &mut ty, &mut size, &mut digits, &mut nullable) };
    assert_eq!(rc, SQL_SUCCESS);
    (String::from_utf16_lossy(&name[..len as usize]), ty, size, nullable)
}

fn free(stmt: SqlHandle) {
    unsafe { assert_eq!(SQLFreeHandle(SQL_HANDLE_STMT, stmt), SQL_SUCCESS) };
}

fn seed(server: &TestServer) -> Result<()> {
    let docs: Vec<_> = (0..10)
        .map(|i| {
            let customer = ["ada", "bo", "cy"][i % 3];
            json!({
                "n": i,
                "customer": customer,
                "total": i as f64 * 2.5,
                "paid": i % 2 == 0,
                "placed": format!("2026-01-{:02}", i + 1),
                "note": if i == 3 { serde_json::Value::Null } else { json!(format!("note {} ✓ ünïcode", i)) },
                "tags": ["a", "b"],
            })
        })
        .collect();
    let res = server.request(reqwest::Method::POST, "/orders/_bulk", Some(&json!(docs)), &[])?;
    assert!(res.status.is_success(), "{}", res.body);
    Ok(())
}

fn connection(server: &TestServer, extra: &str) -> String {
    format!("Driver={{HexDB}};Server={};ApiKey={};{}", server.url(""), server.token(), extra)
}

#[test]
fn queries_page_and_convert() -> Result<()> {
    let server = TestServer::start()?;
    seed(&server)?;
    let odbc = connect(&connection(&server, "PageSize=3")).map_err(|e| anyhow::anyhow!("{:?}", e))?;

    // Every page arrives (PageSize=3, 10 rows), with types from the values.
    let stmt = odbc.query("SELECT n, customer, total, paid, note, tags FROM orders ORDER BY n")?;
    assert_eq!(describe(stmt, 1).1, SQL_BIGINT);
    assert_eq!(describe(stmt, 2), ("customer".into(), SQL_WVARCHAR, 4000, SQL_NULLABLE));
    assert_eq!(describe(stmt, 3).1, SQL_DOUBLE);
    assert_eq!(describe(stmt, 4).1, SQL_BIT);
    assert_eq!(describe(stmt, 6).1, SQL_WLONGVARCHAR);
    let all = rows(stmt)?;
    assert_eq!(all.len(), 10);
    assert_eq!(all[3], vec![Some("3".into()), Some("ada".into()), Some("7.5".into()), Some("0".into()), None, Some("[\"a\",\"b\"]".into())]);
    assert_eq!(all[9][4].as_deref(), Some("note 9 ✓ ünïcode"), "unicode survives, read in 16-character pieces");
    free(stmt);

    // Aggregates and escapes.
    let stmt = odbc.query("SELECT customer, COUNT(*) AS n, SUM(total) FROM orders WHERE placed >= {d '2026-01-04'} GROUP BY customer ORDER BY customer")?;
    assert_eq!(rows(stmt)?, vec![
        vec![Some("ada".into()), Some("3".into()), Some("45.0".into())],
        vec![Some("bo".into()), Some("2".into()), Some("27.5".into())],
        vec![Some("cy".into()), Some("2".into()), Some("32.5".into())],
    ]);
    free(stmt);

    // Bound columns, four rows at a time, column-wise.
    let stmt = odbc.query("SELECT n, total, customer FROM orders ORDER BY n")?;
    unsafe {
        let mut n = [0i64; 4];
        let mut total = [0f64; 4];
        let mut customer = [[0u16; 8]; 4];
        let mut ind_n = [0isize; 4];
        let mut ind_t = [0isize; 4];
        let mut ind_c = [0isize; 4];
        let mut fetched: SqlULen = 0;
        let mut status = [0u16; 4];
        assert_eq!(SQLSetStmtAttrW(stmt, SQL_ATTR_ROW_ARRAY_SIZE, 4 as SqlPointer, 0), SQL_SUCCESS);
        assert_eq!(SQLSetStmtAttrW(stmt, SQL_ATTR_ROWS_FETCHED_PTR, &mut fetched as *mut _ as SqlPointer, 0), SQL_SUCCESS);
        assert_eq!(SQLSetStmtAttrW(stmt, SQL_ATTR_ROW_STATUS_PTR, status.as_mut_ptr() as SqlPointer, 0), SQL_SUCCESS);
        assert_eq!(SQLBindCol(stmt, 1, SQL_C_SBIGINT, n.as_mut_ptr() as SqlPointer, 8, ind_n.as_mut_ptr()), SQL_SUCCESS);
        assert_eq!(SQLBindCol(stmt, 2, SQL_C_DOUBLE, total.as_mut_ptr() as SqlPointer, 8, ind_t.as_mut_ptr()), SQL_SUCCESS);
        assert_eq!(SQLBindCol(stmt, 3, SQL_C_WCHAR, customer.as_mut_ptr() as SqlPointer, 16, ind_c.as_mut_ptr()), SQL_SUCCESS);
        let mut seen = Vec::new();
        let mut sets = Vec::new();
        loop {
            let rc = SQLFetch(stmt);
            if rc == SQL_NO_DATA {
                break;
            }
            check(rc, SQL_HANDLE_STMT, stmt, "array fetch")?;
            sets.push((fetched, status));
            for i in 0..fetched {
                let name = String::from_utf16_lossy(&customer[i][..(ind_c[i] as usize / 2)]);
                seen.push((n[i], total[i], name));
            }
        }
        assert_eq!(seen.len(), 10);
        assert_eq!(seen[5], (5, 12.5, "cy".into()));
        // 4 + 4 + 2: the last row set is partial and says so.
        assert_eq!(sets.iter().map(|s| s.0).collect::<Vec<_>>(), [4, 4, 2]);
        assert_eq!(sets[2].1, [SQL_ROW_SUCCESS, SQL_ROW_SUCCESS, SQL_ROW_NOROW, SQL_ROW_NOROW]);
        assert_eq!(fetched, 0, "SQL_NO_DATA reports no rows");
    }
    free(stmt);
    Ok(())
}

#[test]
fn parameters_and_prepared_statements() -> Result<()> {
    let server = TestServer::start()?;
    seed(&server)?;
    let odbc = connect(&connection(&server, "")).map_err(|e| anyhow::anyhow!("{:?}", e))?;
    let stmt = odbc.stmt();
    let text = wide("SELECT n FROM orders WHERE customer = ? AND n >= ? ORDER BY n");
    unsafe {
        assert_eq!(SQLPrepareW(stmt, text.as_ptr(), SQL_NTS), SQL_SUCCESS);
        let mut count = 0i16;
        SQLNumParams(stmt, &mut count);
        assert_eq!(count, 2);
        let mut customer = wide("ada");
        let mut customer_len: SqlLen = SQL_NTS as SqlLen;
        let mut min = 1i32;
        assert_eq!(SQLBindParameter(stmt, 1, 1, SQL_C_WCHAR, SQL_WVARCHAR, 10, 0, customer.as_mut_ptr() as SqlPointer, 0, &mut customer_len), SQL_SUCCESS);
        assert_eq!(SQLBindParameter(stmt, 2, 1, SQL_C_SLONG, SQL_INTEGER, 10, 0, &mut min as *mut _ as SqlPointer, 0, null_mut()), SQL_SUCCESS);
        // Described before execution, as many tools do.
        let mut columns = 0i16;
        assert_eq!(SQLNumResultCols(stmt, &mut columns), SQL_SUCCESS);
        assert_eq!(columns, 1);
        assert_eq!(SQLFetch(stmt), SQL_ERROR, "not executed yet");
        check(SQLExecute(stmt), SQL_HANDLE_STMT, stmt, "execute")?;
        assert_eq!(rows(stmt)?, vec![vec![Some("3".into())], vec![Some("6".into())], vec![Some("9".into())]]);

        // Execute again with new values.
        SQLCloseCursor(stmt);
        customer = wide("bo");
        min = 5;
        let _ = (&customer, &min); // read by the driver through the bound pointers
        assert_eq!(SQLBindParameter(stmt, 1, 1, SQL_C_WCHAR, SQL_WVARCHAR, 10, 0, customer.as_mut_ptr() as SqlPointer, 0, &mut customer_len), SQL_SUCCESS);
        check(SQLExecute(stmt), SQL_HANDLE_STMT, stmt, "execute again")?;
        assert_eq!(rows(stmt)?, vec![vec![Some("7".into())]]);

        // A missing parameter is an error with SQLSTATE 07002.
        SQLFreeStmt(stmt, SQL_RESET_PARAMS);
        assert_eq!(SQLExecute(stmt), SQL_ERROR);
        assert_eq!(diag(SQL_HANDLE_STMT, stmt).0, "07002");
    }
    free(stmt);
    Ok(())
}

#[test]
fn catalog_info_and_errors() -> Result<()> {
    let server = TestServer::start()?;
    seed(&server)?;
    let odbc = connect(&connection(&server, "")).map_err(|e| anyhow::anyhow!("{:?}", e))?;
    unsafe {
        // SQLTables / SQLColumns / SQLPrimaryKeys / SQLGetTypeInfo.
        let stmt = odbc.stmt();
        let pattern = wide("ord%");
        check(SQLTablesW(stmt, null(), 0, null(), 0, pattern.as_ptr(), SQL_NTS as i16, null(), 0), SQL_HANDLE_STMT, stmt, "SQLTables")?;
        assert_eq!(rows(stmt)?, vec![vec![None, None, Some("orders".into()), Some("TABLE".into()), Some("".into())]]);
        let table = wide("orders");
        check(SQLColumnsW(stmt, null(), 0, null(), 0, table.as_ptr(), SQL_NTS as i16, null(), 0), SQL_HANDLE_STMT, stmt, "SQLColumns")?;
        let columns = rows(stmt)?;
        let names: Vec<_> = columns.iter().map(|r| r[3].clone().unwrap_or_default()).collect();
        assert_eq!(names[0], "id");
        assert!(names.contains(&"customer".to_string()) && names.contains(&"total".to_string()), "{:?}", names);
        let total = columns.iter().find(|r| r[3].as_deref() == Some("total")).unwrap();
        assert_eq!((total[4].as_deref(), total[5].as_deref()), (Some("8"), Some("DOUBLE")));
        check(SQLPrimaryKeysW(stmt, null(), 0, null(), 0, table.as_ptr(), SQL_NTS as i16), SQL_HANDLE_STMT, stmt, "SQLPrimaryKeys")?;
        assert_eq!(rows(stmt)?[0][3].as_deref(), Some("id"));
        check(SQLGetTypeInfoW(stmt, SQL_ALL_TYPES), SQL_HANDLE_STMT, stmt, "SQLGetTypeInfo")?;
        assert_eq!(rows(stmt)?.len(), 5);
        free(stmt);

        // SQLGetInfo.
        let mut buf = [0u16; 64];
        let mut len = 0i16;
        assert_eq!(SQLGetInfoW(odbc.dbc, 17, buf.as_mut_ptr() as SqlPointer, 128, &mut len), SQL_SUCCESS);
        assert_eq!(String::from_utf16_lossy(&buf[..len as usize / 2]), "HexDB");
        assert_eq!(SQLGetInfoW(odbc.dbc, 47, buf.as_mut_ptr() as SqlPointer, 128, &mut len), SQL_SUCCESS);
        assert_eq!(String::from_utf16_lossy(&buf[..len as usize / 2]), TEST_ADMIN_LOGIN);
        let mut txn = 9u16;
        assert_eq!(SQLGetInfoW(odbc.dbc, 46, &mut txn as *mut _ as SqlPointer, 2, null_mut()), SQL_SUCCESS);
        assert_eq!(txn, 0);

        // Errors carry HexDB's message and an SQLSTATE.
        let stmt = odbc.stmt();
        let bad = wide("SELECT * FROM orders o JOIN customers c ON o.n = c.n");
        assert_eq!(SQLExecDirectW(stmt, bad.as_ptr(), SQL_NTS), SQL_ERROR);
        let (state, message) = diag(SQL_HANDLE_STMT, stmt);
        assert_eq!(state, "42000");
        assert!(message.starts_with("[HexDB][ODBC]") && message.contains("JOIN"), "{}", message);
        let missing = wide("SELECT * FROM nothing_here");
        assert_eq!(SQLExecDirectW(stmt, missing.as_ptr(), SQL_NTS), SQL_ERROR);
        assert_eq!(diag(SQL_HANDLE_STMT, stmt).0, "42S02");
        free(stmt);
    }

    // Signing in with UID/PWD works; wrong credentials and unreachable servers don't.
    let by_login = format!("Server={};UID={};PWD={{{}}}", server.url(""), TEST_ADMIN_LOGIN, TEST_ADMIN_PASSWORD);
    let odbc2 = connect(&by_login).map_err(|e| anyhow::anyhow!("{:?}", e))?;
    let stmt = odbc2.query("SELECT COUNT(*) FROM orders")?;
    assert_eq!(rows(stmt)?, vec![vec![Some("10".into())]]);
    free(stmt);
    let wrong = connect(&format!("Server={};ApiKey=hxk_not_a_key", server.url(""))).err().expect("bad key refused");
    assert_eq!(wrong.0, "28000", "{:?}", wrong);
    let port = TestServer::free_port()?;
    let gone = connect(&format!("Server=http://127.0.0.1:{};ApiKey=x;Timeout=5", port)).err().expect("no server");
    assert_eq!(gone.0, "08001", "{:?}", gone);
    let none = connect("Driver={HexDB};ApiKey=x").err().expect("no server given");
    assert_eq!(none.0, "08001");
    Ok(())
}
