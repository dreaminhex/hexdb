// The sandbox on Windows: an AppContainer and a nested job object.
//
// The launcher creates (or reuses) the AppContainer profile named by the
// policy, grants its SID access to the policy's folders, starts the program
// suspended inside the container, puts it in a job that holds the limits and
// dies with the launcher, resumes it, and waits for it.

use super::Policy;
use anyhow::{bail, Context, Result};
use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, LocalFree, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE};
use windows_sys::Win32::NetworkManagement::WindowsFirewall::{NetworkIsolationGetAppContainerConfig, NetworkIsolationSetAppContainerConfig};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSidToSidW, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE,
    SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName};
use windows_sys::Win32::Security::{
    EqualSid, GetAce, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, NO_INHERITANCE, PSECURITY_DESCRIPTOR, PSID, SECURITY_CAPABILITIES,
    SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_LIMIT_PROCESS_TIME, JOB_OBJECT_UILIMIT_DESKTOP,
    JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS, JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES,
    JOB_OBJECT_UILIMIT_READCLIPBOARD, JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess, InitializeProcThreadAttributeList, ResumeThread, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED, EXTENDED_STARTUPINFO_PRESENT, INFINITE,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, STARTF_USESTDHANDLES,
    STARTUPINFOEXW,
};

/// FILE_GENERIC_READ | FILE_GENERIC_EXECUTE.
const READ_EXECUTE: u32 = 0x0012_00A9;
/// Read, write, execute and delete ("Modify").
const MODIFY: u32 = 0x0013_01BF;
const SE_GROUP_ENABLED: u32 = 4;
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const INHERIT_ONLY_ACE: u8 = 0x08;
const ALREADY_EXISTS: i32 = 0x8007_00B7_u32 as i32;
/// Every AppContainer, and the capabilities for the internet and private networks.
const ALL_APPLICATION_PACKAGES: &str = "S-1-15-2-1";
const NETWORK_CAPABILITIES: &[&str] = &["S-1-15-3-1", "S-1-15-3-3"];

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

fn sid_from_string(text: &str) -> Result<PSID> {
    let mut sid: PSID = null_mut();
    // SAFETY: a valid string; the SID is freed with the process (the launcher is short-lived).
    if unsafe { ConvertStringSidToSidW(wide(OsStr::new(text)).as_ptr(), &mut sid) } == 0 {
        bail!("bad SID {}", text);
    }
    Ok(sid)
}

/// The AppContainer's SID, creating its profile the first time.
fn container_sid(name: &str) -> Result<PSID> {
    let wname = wide(OsStr::new(name));
    let mut sid: PSID = null_mut();
    // SAFETY: valid strings, no capabilities; the SID lives as long as the launcher.
    unsafe {
        let hr = CreateAppContainerProfile(wname.as_ptr(), wname.as_ptr(), wide(OsStr::new("HexDB sandbox")).as_ptr(), null(), 0, &mut sid);
        if hr == ALREADY_EXISTS {
            let hr = DeriveAppContainerSidFromAppContainerName(wname.as_ptr(), &mut sid);
            if hr != 0 {
                bail!("couldn't find the AppContainer {} (0x{:08x})", name, hr);
            }
        } else if hr != 0 {
            bail!("couldn't create the AppContainer {} (0x{:08x})", name, hr);
        }
    }
    Ok(sid)
}

/// True if the ACL already allows `sid` at least `mask` on the object itself.
unsafe fn allows(acl: *const ACL, sid: PSID, mask: u32) -> bool {
    if acl.is_null() {
        return false;
    }
    for i in 0..(*acl).AceCount as u32 {
        let mut ace: *mut c_void = null_mut();
        if GetAce(acl, i, &mut ace) == 0 {
            continue;
        }
        let header = &*(ace as *const ACE_HEADER);
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE || header.AceFlags & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        let allowed = &*(ace as *const ACCESS_ALLOWED_ACE);
        if allowed.Mask & mask == mask && EqualSid(&allowed.SidStart as *const u32 as PSID, sid) != 0 {
            return true;
        }
    }
    false
}

/// Give the container `mask` on `path` (and, for a folder, everything in it).
/// Skipped when it (or every AppContainer) already has it, so the ACL is
/// written, and propagated through a folder, only the first time.
fn grant(path: &Path, sid: PSID, everyone: PSID, mask: u32) -> Result<()> {
    let wpath = wide(path.as_os_str());
    // SAFETY: Win32 security calls with valid pointers; returned buffers are freed with LocalFree.
    unsafe {
        let mut dacl: *mut ACL = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        let err = GetNamedSecurityInfoW(wpath.as_ptr(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, null_mut(), null_mut(), &mut dacl, null_mut(), &mut descriptor);
        if err != 0 {
            bail!("can't read the permissions of {} (error {})", path.display(), err);
        }
        let result = if allows(dacl, sid, mask) || allows(dacl, everyone, mask) {
            Ok(())
        } else {
            let access = EXPLICIT_ACCESS_W {
                grfAccessPermissions: mask,
                grfAccessMode: GRANT_ACCESS,
                grfInheritance: if path.is_dir() { SUB_CONTAINERS_AND_OBJECTS_INHERIT } else { NO_INHERITANCE },
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: null_mut(),
                    MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_WELL_KNOWN_GROUP,
                    ptstrName: sid as *mut u16,
                },
            };
            let mut updated: *mut ACL = null_mut();
            let err = SetEntriesInAclW(1, &access, dacl, &mut updated);
            let result = if err != 0 {
                Err(anyhow::anyhow!("can't build the permissions for {} (error {})", path.display(), err))
            } else {
                let err = SetNamedSecurityInfoW(wpath.as_ptr(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, null_mut(), null_mut(), updated, null());
                LocalFree(updated as *mut c_void);
                if err != 0 {
                    Err(anyhow::anyhow!("can't give the sandbox access to {} (error {})", path.display(), err))
                } else {
                    Ok(())
                }
            };
            result
        };
        LocalFree(descriptor);
        result
    }
}

/// Let the container reach localhost (needs an administrator). Windows blocks
/// AppContainers from loopback otherwise, and plugins call the HexDB API there.
fn exempt_loopback(sid: PSID, name: &str) {
    // SAFETY: the list returned by the Get call is read, copied and leaked
    // (the launcher is short-lived); the Set call only reads our array.
    unsafe {
        let mut count = 0u32;
        let mut list: *mut SID_AND_ATTRIBUTES = null_mut();
        let mut all: Vec<SID_AND_ATTRIBUTES> = Vec::new();
        if NetworkIsolationGetAppContainerConfig(&mut count, &mut list) == 0 && !list.is_null() {
            all = std::slice::from_raw_parts(list, count as usize).to_vec();
        }
        if all.iter().any(|entry| EqualSid(entry.Sid, sid) != 0) {
            return;
        }
        all.push(SID_AND_ATTRIBUTES { Sid: sid, Attributes: 0 });
        if NetworkIsolationSetAppContainerConfig(all.len() as u32, all.as_ptr()) != 0 {
            eprintln!(
                "sandbox: Windows keeps this AppContainer off localhost (the HexDB API, and anything else on this machine) until an administrator runs: CheckNetIsolation.exe LoopbackExempt -a -n={} (HexDB does it itself when it runs as a service)",
                name
            );
        }
    }
}

/// A job that holds the limits and kills the program when the launcher exits.
fn job(policy: &Policy) -> Result<HANDLE> {
    // SAFETY: plain Win32 calls; the handle is kept until the launcher exits.
    unsafe {
        let job = CreateJobObjectW(null(), null());
        if job.is_null() {
            bail!("couldn't create the sandbox's job object");
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        let limits = &mut info.BasicLimitInformation;
        limits.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        if policy.max_processes > 0 {
            limits.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            limits.ActiveProcessLimit = policy.max_processes;
        }
        if policy.cpu_seconds > 0 {
            limits.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_TIME;
            // In 100-nanosecond units.
            limits.PerProcessUserTimeLimit = (policy.cpu_seconds as i64) * 10_000_000;
        }
        if policy.memory_mb > 0 {
            limits.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            info.ProcessMemoryLimit = (policy.memory_mb as usize) * 1024 * 1024;
        }
        if SetInformationJobObject(job, JobObjectExtendedLimitInformation, &info as *const _ as *const c_void, std::mem::size_of_val(&info) as u32) == 0 {
            CloseHandle(job);
            bail!("couldn't set the sandbox's limits");
        }
        let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
            UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
                | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                | JOB_OBJECT_UILIMIT_EXITWINDOWS
                | JOB_OBJECT_UILIMIT_GLOBALATOMS
                | JOB_OBJECT_UILIMIT_HANDLES
                | JOB_OBJECT_UILIMIT_READCLIPBOARD
                | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
                | JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
        };
        SetInformationJobObject(job, JobObjectBasicUIRestrictions, &ui as *const _ as *const c_void, std::mem::size_of_val(&ui) as u32);
        Ok(job)
    }
}

/// The user's local application data folder, from the shell.
fn local_app_data() -> Option<std::ffi::OsString> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath};
    // SAFETY: the returned string is read up to its terminator, then freed once.
    unsafe {
        let mut path: *mut u16 = null_mut();
        let ok = SHGetKnownFolderPath(&FOLDERID_LocalAppData, 0, null_mut(), &mut path) == 0 && !path.is_null();
        let result = ok.then(|| {
            let len = (0..).take_while(|&i| *path.add(i) != 0).count();
            std::ffi::OsString::from_wide(std::slice::from_raw_parts(path, len))
        });
        CoTaskMemFree(path as *const c_void);
        result
    }
}

/// Quote one argument the way the C runtime parses command lines.
fn quote(arg: &str, out: &mut String) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
            continue;
        }
        let escaped = if c == '"' { backslashes * 2 + 1 } else { backslashes };
        out.extend(std::iter::repeat_n('\\', escaped));
        out.push(c);
        backslashes = 0;
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
}

fn command_line(program: &Path, args: &[String]) -> String {
    let mut line = String::new();
    quote(&program.to_string_lossy(), &mut line);
    for arg in args {
        line.push(' ');
        quote(arg, &mut line);
    }
    line
}

pub fn run(policy: &Policy, program: &Path, args: &[String]) -> Result<i32> {
    let ext = program.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if ext != "exe" && ext != "com" {
        bail!(
            "{} isn't a program file; a sandboxed command must start with an .exe (run scripts through their interpreter, such as node or python, or set sandbox = false)",
            program.display()
        );
    }
    // Starting an AppContainer needs LOCALAPPDATA (Windows redirects it to the
    // container's own folder); the parser worker's environment is cleared.
    if std::env::var_os("LOCALAPPDATA").is_none() {
        if let Some(path) = local_app_data() {
            std::env::set_var("LOCALAPPDATA", path);
        }
    }
    let sid = container_sid(&policy.name)?;
    let everyone = sid_from_string(ALL_APPLICATION_PACKAGES)?;
    // Read access is best effort (the program may already be readable by
    // every AppContainer, as under Program Files); write access must work.
    for path in policy.read.iter().map(|p| p.as_path()).chain([program]) {
        if let Err(e) = grant(path, sid, everyone, READ_EXECUTE) {
            eprintln!("sandbox: {:#}", e);
        }
    }
    for path in &policy.write {
        grant(path, sid, everyone, MODIFY)?;
    }
    let mut capabilities = Vec::new();
    if policy.network {
        for text in NETWORK_CAPABILITIES {
            capabilities.push(SID_AND_ATTRIBUTES { Sid: sid_from_string(text)?, Attributes: SE_GROUP_ENABLED });
        }
        exempt_loopback(sid, &policy.name);
    }
    let security = SECURITY_CAPABILITIES {
        AppContainerSid: sid,
        Capabilities: if capabilities.is_empty() { null_mut() } else { capabilities.as_mut_ptr() },
        CapabilityCount: capabilities.len() as u32,
        Reserved: 0,
    };
    let job = job(policy)?;

    // SAFETY: the attribute list, handles and strings outlive CreateProcessW;
    // the list is deleted once, and the process handle is waited on and closed.
    unsafe {
        // Only the three standard handles are inherited.
        let mut handles: Vec<HANDLE> = Vec::new();
        let std_handles = [GetStdHandle(STD_INPUT_HANDLE), GetStdHandle(STD_OUTPUT_HANDLE), GetStdHandle(STD_ERROR_HANDLE)];
        for handle in std_handles {
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE && !handles.contains(&handle) {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT);
                handles.push(handle);
            }
        }
        let mut size = 0usize;
        InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut size);
        let mut buffer = vec![0u64; size.div_ceil(8)];
        let attributes = buffer.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if InitializeProcThreadAttributeList(attributes, 2, 0, &mut size) == 0 {
            bail!("couldn't prepare the sandbox (error {})", GetLastError());
        }
        let ok = UpdateProcThreadAttribute(
            attributes,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            &security as *const _ as *const c_void,
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
            null_mut(),
            null(),
        ) != 0
            && (handles.is_empty()
                || UpdateProcThreadAttribute(
                    attributes,
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles.as_ptr() as *const c_void,
                    handles.len() * std::mem::size_of::<HANDLE>(),
                    null_mut(),
                    null(),
                ) != 0);
        if !ok {
            let error = GetLastError();
            DeleteProcThreadAttributeList(attributes);
            bail!("couldn't prepare the sandbox (error {})", error);
        }
        let mut startup: STARTUPINFOEXW = std::mem::zeroed();
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = std_handles[0];
        startup.StartupInfo.hStdOutput = std_handles[1];
        startup.StartupInfo.hStdError = std_handles[2];
        startup.lpAttributeList = attributes;
        let application = wide(program.as_os_str());
        let mut line = wide(OsStr::new(&command_line(program, args)));
        let mut process: PROCESS_INFORMATION = std::mem::zeroed();
        let created = CreateProcessW(
            application.as_ptr(),
            line.as_mut_ptr(),
            null(),
            null(),
            if handles.is_empty() { 0 } else { 1 },
            EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_NO_WINDOW,
            null(),
            null(),
            &startup.StartupInfo,
            &mut process,
        );
        let error = GetLastError();
        DeleteProcThreadAttributeList(attributes);
        if created == 0 {
            return Err(std::io::Error::from_raw_os_error(error as i32)).with_context(|| format!("couldn't start {} in its AppContainer", program.display()));
        }
        if AssignProcessToJobObject(job, process.hProcess) == 0 {
            // Never let it run without its limits.
            TerminateProcess(process.hProcess, 1);
            bail!("couldn't put {} in the sandbox's job", program.display());
        }
        ResumeThread(process.hThread);
        CloseHandle(process.hThread);
        WaitForSingleObject(process.hProcess, INFINITE);
        let mut code = 1u32;
        GetExitCodeProcess(process.hProcess, &mut code);
        CloseHandle(process.hProcess);
        Ok(code as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_quoted_like_the_c_runtime_parses_them() {
        let line = command_line(Path::new(r"C:\Program Files\node\node.exe"), &["index.mjs".into(), "a b".into(), r#"say "hi""#.into(), r"C:\my dir\".into(), r"C:\plain\".into(), String::new()]);
        assert_eq!(line, r#""C:\Program Files\node\node.exe" index.mjs "a b" "say \"hi\"" "C:\my dir\\" C:\plain\ """#);
    }
}
