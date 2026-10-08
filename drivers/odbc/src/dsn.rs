// Reading a DSN's settings from ODBC.INI through the platform's installer
// library (odbccp32 on Windows, odbcinst on unixODBC, iodbcinst on iODBC).
// It's loaded at run time so the driver doesn't depend on a driver manager.

use std::sync::OnceLock;

#[cfg(windows)]
type GetProfile = unsafe extern "system" fn(*const u16, *const u16, *const u16, *mut u16, i32, *const u16) -> i32;
#[cfg(not(windows))]
type GetProfile = unsafe extern "C" fn(*const u8, *const u8, *const u8, *mut u8, i32, *const u8) -> i32;

struct Installer {
    _library: libloading::Library,
    get: GetProfile,
}

fn installer() -> Option<&'static Installer> {
    static INSTALLER: OnceLock<Option<Installer>> = OnceLock::new();
    INSTALLER
        .get_or_init(|| {
            #[cfg(windows)]
            let (names, symbol): (&[&str], &[u8]) = (&["odbccp32.dll"], b"SQLGetPrivateProfileStringW\0");
            #[cfg(not(windows))]
            let (names, symbol): (&[&str], &[u8]) = (
                &["libodbcinst.so.2", "libodbcinst.so", "libodbcinst.2.dylib", "libodbcinst.dylib", "libiodbcinst.so.2", "libiodbcinst.dylib"],
                b"SQLGetPrivateProfileString\0",
            );
            for name in names {
                // SAFETY: loading the platform's ODBC installer library, whose
                // initializers are expected to be safe to run.
                let Ok(library) = (unsafe { libloading::Library::new(*name) }) else { continue };
                // SAFETY: the symbol has the documented signature `GetProfile`.
                let get = unsafe { library.get::<GetProfile>(symbol).map(|s| *s) };
                if let Ok(get) = get {
                    return Some(Installer { _library: library, get });
                }
            }
            None
        })
        .as_ref()
}

/// The value of `key` in the DSN's section of ODBC.INI, if set.
pub fn read(dsn: &str, key: &str) -> Option<String> {
    let installer = installer()?;
    #[cfg(windows)]
    {
        let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let (section, entry, default, file) = (wide(dsn), wide(key), wide(""), wide("ODBC.INI"));
        let mut buffer = vec![0u16; 4096];
        // SAFETY: every string is NUL-terminated and the buffer holds 4096 characters.
        let n = unsafe { (installer.get)(section.as_ptr(), entry.as_ptr(), default.as_ptr(), buffer.as_mut_ptr(), buffer.len() as i32, file.as_ptr()) };
        let n = (n.max(0) as usize).min(buffer.len());
        let value = String::from_utf16_lossy(&buffer[..n]);
        (!value.is_empty()).then_some(value)
    }
    #[cfg(not(windows))]
    {
        let narrow = |s: &str| s.bytes().chain(std::iter::once(0)).collect::<Vec<u8>>();
        let (section, entry, default, file) = (narrow(dsn), narrow(key), narrow(""), narrow("ODBC.INI"));
        let mut buffer = vec![0u8; 4096];
        // SAFETY: every string is NUL-terminated and the buffer holds 4096 bytes.
        let n = unsafe { (installer.get)(section.as_ptr(), entry.as_ptr(), default.as_ptr(), buffer.as_mut_ptr(), buffer.len() as i32, file.as_ptr()) };
        let n = (n.max(0) as usize).min(buffer.len());
        let value = String::from_utf8_lossy(&buffer[..n]).trim_end_matches('\0').to_string();
        (!value.is_empty()).then_some(value)
    }
}
