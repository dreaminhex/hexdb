//! HexDB ODBC driver.
//!
//! An ODBC 3.8 driver that sends SQL to a HexDB server's `POST /sql` endpoint
//! over HTTP(S) and reads catalogs from `GET /sql/tables` and
//! `GET /sql/columns`. It implements the Unicode (W) entry points; driver
//! managers map ANSI calls onto them.
//!
//! Connection string keys: `Server` (http://host:7700), `ApiKey` (or `Token`),
//! `UID` and `PWD` (sign in instead of an API key), `PageSize` (rows per
//! request, default 1000), `MaxStringLength` (size reported for text
//! columns, default 4000), `Timeout` (seconds, default 60) and `CAFile` (PEM
//! certificates to trust instead of the system's). A `DSN` supplies any of
//! them from ODBC.INI.

mod api;
mod catalog;
mod client;
mod convert;
mod dsn;
pub mod ffi;
mod info;
mod stmt;
mod text;

pub use api::*;

use ffi::*;
use std::sync::Mutex;

/// An ODBC diagnostic: SQLSTATE, native error and message.
#[derive(Debug, Clone)]
pub struct OdbcError {
    pub state: &'static str,
    pub native: i32,
    pub message: String,
}

impl OdbcError {
    pub fn new(state: &'static str, message: impl Into<String>) -> OdbcError {
        OdbcError { state, native: 0, message: message.into() }
    }

    pub fn with_native(mut self, native: i32) -> OdbcError {
        self.native = native;
        self
    }

    /// Errors while connecting: a server that can't be reached is 08001.
    pub fn at_connect(mut self) -> OdbcError {
        if self.state == "08S01" {
            self.state = "08001";
        }
        self
    }
}

/// How a call finished when it didn't fail.
pub enum Done {
    Ok,
    NoData,
}

const ENV_MAGIC: u32 = 0x4845_5801;
const DBC_MAGIC: u32 = 0x4845_5802;
const STMT_MAGIC: u32 = 0x4845_5803;
const DESC_MAGIC: u32 = 0x4845_5804;

/// A handle: a type tag (to reject the wrong kind of handle), its
/// diagnostics, and its state.
pub struct Handle<T> {
    magic: u32,
    diags: Mutex<Vec<OdbcError>>,
    pub state: Mutex<T>,
}

impl<T> Handle<T> {
    fn new(magic: u32, state: T) -> *mut Handle<T> {
        Box::into_raw(Box::new(Handle { magic, diags: Mutex::new(Vec::new()), state: Mutex::new(state) }))
    }

    /// # Safety
    /// `ptr` must be null or a pointer this driver handed out.
    unsafe fn from_ptr<'a>(ptr: SqlHandle, magic: u32) -> Option<&'a Handle<T>> {
        let handle = (ptr as *const Handle<T>).as_ref()?;
        (handle.magic == magic).then_some(handle)
    }

    pub fn diagnostics(&self) -> Vec<OdbcError> {
        self.diags.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

pub struct EnvState {
    pub odbc_version: i32,
}

pub struct DbcState {
    pub env: *const Handle<EnvState>,
    pub client: Option<std::sync::Arc<client::Client>>,
    pub login_timeout: u64,
}

pub type Env = Handle<EnvState>;
pub type Dbc = Handle<DbcState>;
pub type Stmt = Handle<stmt::StmtState>;
/// Placeholder descriptors: SQLGetStmtAttr hands these out for the implicit
/// descriptors; the driver doesn't support descriptor functions.
pub type Desc = Handle<()>;

/// Run one ODBC call on a handle: clear its diagnostics, catch panics, and
/// turn the result into a return code (warnings into SQL_SUCCESS_WITH_INFO).
fn call<T>(ptr: SqlHandle, magic: u32, f: impl FnOnce(&Handle<T>, &mut Vec<OdbcError>) -> Result<Done, OdbcError>) -> SqlReturn {
    // SAFETY: the driver manager passes handles this driver allocated; the
    // magic number rejects anything else.
    let Some(handle) = (unsafe { Handle::<T>::from_ptr(ptr, magic) }) else { return SQL_INVALID_HANDLE };
    handle.diags.lock().unwrap_or_else(|e| e.into_inner()).clear();
    let mut warnings = Vec::new();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(handle, &mut warnings)));
    let mut diags = handle.diags.lock().unwrap_or_else(|e| e.into_inner());
    match outcome {
        Ok(Ok(done)) => {
            let code = match done {
                Done::NoData => SQL_NO_DATA,
                Done::Ok if warnings.is_empty() => SQL_SUCCESS,
                Done::Ok => SQL_SUCCESS_WITH_INFO,
            };
            diags.extend(warnings);
            code
        }
        Ok(Err(e)) => {
            diags.extend(warnings);
            diags.push(e);
            SQL_ERROR
        }
        Err(panic) => {
            let what = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown".into());
            diags.push(OdbcError::new("HY000", format!("Internal driver error: {}", what)));
            SQL_ERROR
        }
    }
}

fn env_call(ptr: SqlHandle, f: impl FnOnce(&Env, &mut EnvState, &mut Vec<OdbcError>) -> Result<Done, OdbcError>) -> SqlReturn {
    call::<EnvState>(ptr, ENV_MAGIC, |h, w| f(h, &mut h.state.lock().unwrap_or_else(|e| e.into_inner()), w))
}

fn dbc_call(ptr: SqlHandle, f: impl FnOnce(&Dbc, &mut DbcState, &mut Vec<OdbcError>) -> Result<Done, OdbcError>) -> SqlReturn {
    call::<DbcState>(ptr, DBC_MAGIC, |h, w| f(h, &mut h.state.lock().unwrap_or_else(|e| e.into_inner()), w))
}

fn stmt_call(ptr: SqlHandle, f: impl FnOnce(&Stmt, &mut stmt::StmtState, &mut Vec<OdbcError>) -> Result<Done, OdbcError>) -> SqlReturn {
    call::<stmt::StmtState>(ptr, STMT_MAGIC, |h, w| f(h, &mut h.state.lock().unwrap_or_else(|e| e.into_inner()), w))
}

/// Add a truncation warning if `truncated`.
fn truncation(truncated: bool, warnings: &mut Vec<OdbcError>) {
    if truncated {
        warnings.push(OdbcError::new("01004", "String data, right truncated"));
    }
}

/// The diagnostics of any kind of handle.
///
/// # Safety
/// `ptr` must be null or a handle this driver allocated, of `handle_type`.
unsafe fn diagnostics_of(handle_type: SqlSmallInt, ptr: SqlHandle) -> Option<Vec<OdbcError>> {
    Some(match handle_type {
        SQL_HANDLE_ENV => Handle::<EnvState>::from_ptr(ptr, ENV_MAGIC)?.diagnostics(),
        SQL_HANDLE_DBC => Handle::<DbcState>::from_ptr(ptr, DBC_MAGIC)?.diagnostics(),
        SQL_HANDLE_STMT => Handle::<stmt::StmtState>::from_ptr(ptr, STMT_MAGIC)?.diagnostics(),
        SQL_HANDLE_DESC => Handle::<()>::from_ptr(ptr, DESC_MAGIC)?.diagnostics(),
        _ => return None,
    })
}
