// Statements: preparing and executing SQL, result sets read a page at a
// time, bound columns (row- or column-wise arrays), SQLGetData, and the
// result sets catalog functions build locally.

use crate::client::Client;
use crate::convert::{param_value, put, ParamBinding, Put};
use crate::ffi::*;
use crate::text::{count_parameters, rewrite_escapes};
use crate::{Dbc, Done, OdbcError};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::sync::Arc;

/// One result column.
#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub sql_type: SqlSmallInt,
    /// Column size: characters for text, digits for numbers.
    pub size: usize,
    pub digits: SqlSmallInt,
    pub nullable: SqlSmallInt,
    /// The tessellation it came from ("" for catalog results and values).
    pub table: String,
}

impl Column {
    /// A column of a SQL result, from HexDB's inferred type.
    pub fn from_hexdb(name: &str, kind: &str, table: &str, max_string: usize) -> Column {
        let (sql_type, size, digits) = match kind {
            "boolean" => (SQL_BIT, 1, 0),
            "integer" => (SQL_BIGINT, 19, 0),
            "number" => (SQL_DOUBLE, 15, 0),
            "json" => (SQL_WLONGVARCHAR, 1 << 30, 0),
            _ => (SQL_WVARCHAR, max_string, 0),
        };
        let nullable = if name == "id" { SQL_NO_NULLS } else { SQL_NULLABLE };
        Column { name: name.to_string(), sql_type, size, digits, nullable, table: table.to_string() }
    }

    pub fn text(name: &str) -> Column {
        Column { name: name.into(), sql_type: SQL_WVARCHAR, size: 128, digits: 0, nullable: SQL_NULLABLE, table: String::new() }
    }

    pub fn small(name: &str) -> Column {
        Column { name: name.into(), sql_type: SQL_SMALLINT, size: 5, digits: 0, nullable: SQL_NULLABLE, table: String::new() }
    }

    pub fn int(name: &str) -> Column {
        Column { name: name.into(), sql_type: SQL_INTEGER, size: 10, digits: 0, nullable: SQL_NULLABLE, table: String::new() }
    }

    /// The type's name, as SQLGetTypeInfo lists it.
    pub fn type_name(&self) -> &'static str {
        crate::catalog::type_name(self.sql_type)
    }

    /// Bytes needed to hold a value, for SQL_DESC_OCTET_LENGTH.
    pub fn octet_length(&self) -> usize {
        match self.sql_type {
            SQL_BIT | SQL_TINYINT => 1,
            SQL_SMALLINT => 2,
            SQL_INTEGER | SQL_REAL => 4,
            SQL_BIGINT | SQL_DOUBLE | SQL_FLOAT => 8,
            _ => self.size.saturating_mul(2),
        }
    }

    /// Characters needed to display a value, for SQL_DESC_DISPLAY_SIZE.
    pub fn display_size(&self) -> usize {
        match self.sql_type {
            SQL_BIT => 1,
            SQL_SMALLINT => 6,
            SQL_INTEGER => 11,
            SQL_BIGINT => 20,
            SQL_DOUBLE | SQL_FLOAT => 24,
            _ => self.size,
        }
    }
}

/// The next page of a result, fetched when the rows read so far run out.
struct Pending {
    client: Arc<Client>,
    sql: String,
    params: Vec<Value>,
    cursor: String,
}

pub struct ResultSet {
    pub columns: Vec<Column>,
    rows: VecDeque<Vec<Value>>,
    pending: Option<Pending>,
    /// The last row fetched, for SQLGetData.
    pub current: Option<Vec<Value>>,
    /// SQLGetData progress per column of the current row.
    pub progress: Vec<usize>,
    returned: usize,
}

impl ResultSet {
    /// A result built by the driver (catalog functions).
    pub fn local(columns: Vec<Column>, rows: Vec<Vec<Value>>) -> ResultSet {
        let n = columns.len();
        ResultSet { columns, rows: rows.into(), pending: None, current: None, progress: vec![0; n], returned: 0 }
    }

    fn from_page(client: Arc<Client>, sql: String, params: Vec<Value>, page: Value) -> ResultSet {
        let table = page["translated"]["tessellation"].as_str().unwrap_or_default().to_string();
        let max_string = client.settings.max_string_length;
        let columns: Vec<Column> = page["columns"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| Column::from_hexdb(c["name"].as_str().unwrap_or_default(), c["type"].as_str().unwrap_or("string"), &table, max_string))
            .collect();
        let mut result = ResultSet::local(columns, Vec::new());
        result.add_page(&page);
        if let Some(cursor) = page["next"].as_str() {
            result.pending = Some(Pending { client, sql, params, cursor: cursor.to_string() });
        }
        result
    }

    /// Add a page's rows, matched to this result's columns by name (a later
    /// page of `SELECT *` can list other fields).
    fn add_page(&mut self, page: &Value) {
        let names: Vec<&str> = page["columns"].as_array().into_iter().flatten().map(|c| c["name"].as_str().unwrap_or_default()).collect();
        let same = names.len() == self.columns.len() && names.iter().zip(&self.columns).all(|(n, c)| *n == c.name);
        let positions: Vec<Option<usize>> = self.columns.iter().map(|c| names.iter().position(|n| *n == c.name)).collect();
        for row in page["rows"].as_array().into_iter().flatten() {
            let Some(values) = row.as_array() else { continue };
            if same {
                self.rows.push_back(values.clone());
            } else {
                self.rows.push_back(positions.iter().map(|p| p.and_then(|i| values.get(i).cloned()).unwrap_or(Value::Null)).collect());
            }
        }
    }

    /// The next row, reading the next page when needed.
    fn next_row(&mut self, max_rows: usize) -> Result<Option<Vec<Value>>, OdbcError> {
        if max_rows > 0 && self.returned >= max_rows {
            return Ok(None);
        }
        loop {
            if let Some(row) = self.rows.pop_front() {
                self.returned += 1;
                return Ok(Some(row));
            }
            let Some(pending) = self.pending.take() else { return Ok(None) };
            let page = pending.client.sql(&pending.sql, &pending.params, Some(&pending.cursor))?;
            self.add_page(&page);
            if let Some(cursor) = page["next"].as_str() {
                self.pending = Some(Pending { cursor: cursor.to_string(), ..pending });
            }
        }
    }
}

/// A column bound with SQLBindCol.
#[derive(Debug, Clone, Copy)]
pub struct ColBinding {
    pub c_type: SqlSmallInt,
    pub ptr: *mut c_void,
    pub buffer_len: SqlLen,
    pub ind: *mut SqlLen,
}

pub struct StmtState {
    pub dbc: *const Dbc,
    /// The prepared statement, with ODBC escapes rewritten.
    pub sql: Option<String>,
    pub params: BTreeMap<u16, ParamBinding>,
    pub columns: BTreeMap<u16, ColBinding>,
    pub result: Option<ResultSet>,
    /// SQLExecute ran (a result from describing a prepared statement isn't fetchable yet).
    pub executed: bool,
    described_with: Option<Vec<Value>>,
    pub row_array_size: usize,
    pub rows_fetched: *mut SqlULen,
    pub row_status: *mut SqlUSmallInt,
    pub row_bind_type: usize,
    pub row_bind_offset: *mut SqlLen,
    pub max_rows: usize,
    pub query_timeout: usize,
    pub metadata_id: usize,
    pub paramset_size: usize,
    pub descriptors: [*mut crate::Desc; 4],
}

impl StmtState {
    pub fn new(dbc: *const Dbc) -> StmtState {
        StmtState {
            dbc,
            sql: None,
            params: BTreeMap::new(),
            columns: BTreeMap::new(),
            result: None,
            executed: false,
            described_with: None,
            row_array_size: 1,
            rows_fetched: std::ptr::null_mut(),
            row_status: std::ptr::null_mut(),
            row_bind_type: SQL_BIND_BY_COLUMN,
            row_bind_offset: std::ptr::null_mut(),
            max_rows: 0,
            query_timeout: 0,
            metadata_id: 0,
            paramset_size: 1,
            descriptors: [std::ptr::null_mut(); 4],
        }
    }

    pub fn client(&self) -> Result<Arc<Client>, OdbcError> {
        // SAFETY: a statement's connection outlives it (the driver manager
        // frees statements before their connection).
        let dbc = unsafe { self.dbc.as_ref() }.ok_or_else(|| OdbcError::new("08003", "Connection not open"))?;
        let state = dbc.state.lock().unwrap_or_else(|e| e.into_inner());
        state.client.clone().ok_or_else(|| OdbcError::new("08003", "Connection not open"))
    }

    pub fn close_cursor(&mut self) {
        self.result = None;
        self.executed = false;
        self.described_with = None;
    }

    pub fn prepare(&mut self, sql: &str) {
        self.close_cursor();
        self.sql = Some(rewrite_escapes(sql));
    }

    pub fn parameter_count(&self) -> usize {
        self.sql.as_deref().map(count_parameters).unwrap_or(0)
    }

    /// The bound parameters' values; when only describing, unbound ones are NULL.
    fn parameter_values(&self, describing: bool) -> Result<Vec<Value>, OdbcError> {
        if self.paramset_size > 1 {
            return Err(OdbcError::new("HYC00", "Arrays of parameters (SQL_ATTR_PARAMSET_SIZE > 1) aren't supported."));
        }
        (1..=self.parameter_count() as u16)
            .map(|i| match self.params.get(&i) {
                // SAFETY: the application keeps bound buffers valid until it
                // unbinds them (SQLBindParameter's contract).
                Some(binding) => unsafe { param_value(binding) },
                None if describing => Ok(Value::Null),
                None => Err(OdbcError::new("07002", format!("Parameter {} isn't bound.", i))),
            })
            .collect()
    }

    fn run(&mut self, params: Vec<Value>) -> Result<(), OdbcError> {
        let sql = self.sql.clone().ok_or_else(|| OdbcError::new("HY010", "No statement has been prepared."))?;
        let client = self.client()?;
        let page = client.sql(&sql, &params, None)?;
        self.result = Some(ResultSet::from_page(client, sql, params, page));
        Ok(())
    }

    pub fn execute(&mut self) -> Result<(), OdbcError> {
        let params = self.parameter_values(false)?;
        // A statement described before execution already has its first page.
        let reuse = self.result.is_some() && !self.executed && self.described_with.as_ref() == Some(&params);
        if !reuse {
            self.result = None;
            self.run(params)?;
        }
        self.described_with = None;
        self.executed = true;
        Ok(())
    }

    /// Make sure there's result metadata: run a prepared statement (with
    /// unbound parameters as NULL) to learn its columns.
    pub fn describe(&mut self) -> Result<(), OdbcError> {
        if self.result.is_none() && self.sql.is_some() {
            let params = self.parameter_values(true)?;
            self.run(params.clone())?;
            self.described_with = Some(params);
            self.executed = false;
        }
        Ok(())
    }

    /// Show a locally built result (catalog functions).
    pub fn set_local(&mut self, result: ResultSet) {
        self.close_cursor();
        self.sql = None;
        self.result = Some(result);
        self.executed = true;
    }

    pub fn columns(&mut self) -> Result<&[Column], OdbcError> {
        self.describe()?;
        Ok(self.result.as_ref().map(|r| r.columns.as_slice()).unwrap_or(&[]))
    }

    pub fn column(&mut self, n: u16) -> Result<Column, OdbcError> {
        let columns = self.columns()?;
        if n == 0 || n as usize > columns.len() {
            return Err(OdbcError::new("07009", format!("Invalid descriptor index {}: the result has {} columns.", n, columns.len())));
        }
        Ok(columns[n as usize - 1].clone())
    }

    /// SQLFetch / SQLFetchScroll(SQL_FETCH_NEXT): the next row set into the bound columns.
    pub fn fetch(&mut self, warnings: &mut Vec<OdbcError>) -> Result<Done, OdbcError> {
        if !self.executed || self.result.is_none() {
            return Err(OdbcError::new("24000", "Invalid cursor state: no result to fetch from."));
        }
        let size = self.row_array_size.max(1);
        let max_rows = self.max_rows;
        let bindings: Vec<(u16, ColBinding)> = self.columns.iter().map(|(n, b)| (*n, *b)).collect();
        let bind_type = self.row_bind_type;
        // SAFETY: the bind offset pointer is valid while set (SQLSetStmtAttr's contract).
        let offset = unsafe { self.row_bind_offset.as_ref().copied().unwrap_or(0) };
        let result = self.result.as_mut().unwrap_or_else(|| unreachable!());
        let mut fetched = 0usize;
        let mut statuses = Vec::with_capacity(size);
        for i in 0..size {
            let Some(row) = result.next_row(max_rows)? else { break };
            let mut status = SQL_ROW_SUCCESS;
            for (n, b) in &bindings {
                let Some(column) = result.columns.get(*n as usize - 1) else {
                    return Err(OdbcError::new("07009", format!("Column {} is bound but the result has {} columns.", n, result.columns.len())));
                };
                let value = row.get(*n as usize - 1).unwrap_or(&Value::Null);
                let (target, ind) = if bind_type == SQL_BIND_BY_COLUMN {
                    (
                        address(b.ptr, offset + (i as isize) * b.buffer_len.max(0)),
                        address(b.ind as *mut c_void, offset + (i * std::mem::size_of::<SqlLen>()) as isize) as *mut SqlLen,
                    )
                } else {
                    let stride = (i * bind_type) as isize;
                    (address(b.ptr, offset + stride), address(b.ind as *mut c_void, offset + stride) as *mut SqlLen)
                };
                let mut progress = 0;
                // SAFETY: bound buffers are valid for the row set (SQLBindCol's contract).
                match unsafe { put(value, column.sql_type, b.c_type, target, b.buffer_len, ind, &mut progress) }? {
                    Put::Warn(w) => {
                        status = SQL_ROW_SUCCESS_WITH_INFO;
                        warnings.push(w);
                    }
                    Put::Done | Put::NoData => {}
                }
            }
            statuses.push(status);
            result.current = Some(row);
            fetched += 1;
        }
        result.progress = vec![0; result.columns.len()];
        // SAFETY: the rows-fetched and row-status pointers are valid while set.
        unsafe {
            if let Some(out) = self.rows_fetched.as_mut() {
                *out = fetched;
            }
            if !self.row_status.is_null() {
                for i in 0..size {
                    *self.row_status.add(i) = statuses.get(i).copied().unwrap_or(SQL_ROW_NOROW);
                }
            }
        }
        if fetched == 0 {
            if let Some(r) = self.result.as_mut() {
                r.current = None;
            }
            return Ok(Done::NoData);
        }
        Ok(Done::Ok)
    }

    /// SQLGetData for column `n` of the current row.
    ///
    /// # Safety
    /// `ptr` and `ind` must be valid as described by `buffer_len`.
    pub unsafe fn get_data(
        &mut self,
        n: u16,
        c_type: SqlSmallInt,
        ptr: *mut c_void,
        buffer_len: SqlLen,
        ind: *mut SqlLen,
        warnings: &mut Vec<OdbcError>,
    ) -> Result<Done, OdbcError> {
        let result = self.result.as_mut().ok_or_else(|| OdbcError::new("24000", "Invalid cursor state: no result."))?;
        let Some(row) = &result.current else { return Err(OdbcError::new("24000", "Invalid cursor state: no current row; call SQLFetch first.")) };
        if n == 0 || n as usize > result.columns.len() {
            return Err(OdbcError::new("07009", format!("Invalid descriptor index {}: the result has {} columns.", n, result.columns.len())));
        }
        let i = n as usize - 1;
        let value = row.get(i).cloned().unwrap_or(Value::Null);
        match put(&value, result.columns[i].sql_type, c_type, ptr, buffer_len, ind, &mut result.progress[i])? {
            Put::Done => Ok(Done::Ok),
            Put::Warn(w) => {
                warnings.push(w);
                Ok(Done::Ok)
            }
            Put::NoData => Ok(Done::NoData),
        }
    }
}

fn address(base: *mut c_void, offset: isize) -> *mut c_void {
    if base.is_null() {
        base
    } else {
        (base as *mut u8).wrapping_offset(offset) as *mut c_void
    }
}
