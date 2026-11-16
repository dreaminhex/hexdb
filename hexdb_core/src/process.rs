// HexDB Core: child processes (plugins, script functions)
//
// Child processes must not outlive the server: if HexDB is killed or crashes,
// a plugin left running would keep ports, files and pipes open and could
// write with a stale API key. On Linux each child asks the kernel for SIGKILL
// when its parent dies; on Windows every child joins a job object that kills
// its members when the server's handle to it closes (which happens however
// the server exits). Graceful shutdown still stops children normally.

/// Prepare a command so its process dies with the server (call before spawn).
pub fn contain(command: &mut tokio::process::Command) {
    #[cfg(target_os = "linux")]
    {
        extern "C" {
            fn prctl(option: i32, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i32;
        }
        const PR_SET_PDEATHSIG: i32 = 1;
        const SIGKILL: u64 = 9;
        // SAFETY: prctl is async-signal-safe, and only this process's own
        // death signal changes between fork and exec.
        unsafe {
            command.pre_exec(|| {
                prctl(PR_SET_PDEATHSIG, SIGKILL, 0, 0, 0);
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

/// Start the process in a process group of its own (Unix), so `kill_group`
/// stops whatever it started too.
pub fn own_group(command: &mut tokio::process::Command) {
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(not(unix))]
    let _ = command;
}

/// Kill the process group led by `pid` (from `own_group`; Unix). On Windows the
/// caller kills the process itself; its job object stops the rest with the server.
pub fn kill_group(pid: u32) {
    #[cfg(unix)]
    {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        const SIGKILL: i32 = 9;
        if let Ok(pid) = i32::try_from(pid) {
            if pid > 0 {
                // SAFETY: kill only sends a signal; a negative pid names the group.
                unsafe {
                    kill(-pid, SIGKILL);
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// A job object of its own for one process and everything it starts (Windows):
/// dropping it kills them all. Elsewhere it does nothing (see `own_group`).
pub struct RunJob {
    #[cfg(windows)]
    handle: usize,
}

impl RunJob {
    /// A job for `child` (which may already be in the server's job: jobs nest).
    pub fn for_child(child: &tokio::process::Child) -> Option<RunJob> {
        #[cfg(windows)]
        {
            let handle = windows_job::new_kill_on_close()?;
            let job = RunJob { handle };
            windows_job::assign(handle, child).then_some(job)
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            None
        }
    }
}

impl Drop for RunJob {
    fn drop(&mut self) {
        #[cfg(windows)]
        windows_job::close(self.handle);
    }
}

/// Attach a started process to the server's job (Windows; a no-op elsewhere).
pub fn adopt(child: &tokio::process::Child) {
    #[cfg(windows)]
    windows_job::adopt(child);
    #[cfg(not(windows))]
    let _ = child;
}

#[cfg(windows)]
mod windows_job {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// The job handle, as an integer so it can live in a static. Never closed:
    /// it closes when the server exits, which kills the job's processes.
    static JOB: OnceLock<usize> = OnceLock::new();

    /// A new job that kills its processes when its last handle closes.
    pub fn new_kill_on_close() -> Option<usize> {
        // SAFETY: plain Win32 calls with valid arguments; failures return null/0.
        let handle = unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                windows_sys::Win32::Foundation::CloseHandle(job);
                return None;
            }
            job as usize
        };
        Some(handle)
    }

    /// Put a process in a job. False if it couldn't.
    pub fn assign(job: usize, child: &tokio::process::Child) -> bool {
        let Some(process) = child.raw_handle() else { return false };
        // SAFETY: both handles are valid for the duration of the call.
        unsafe { AssignProcessToJobObject(job as *mut core::ffi::c_void, process) != 0 }
    }

    /// Close a job's handle (killing its processes, for a kill-on-close job).
    pub fn close(job: usize) {
        // SAFETY: the handle is ours and closed once.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(job as *mut core::ffi::c_void);
        }
    }

    fn job() -> Option<usize> {
        let handle = *JOB.get_or_init(|| {
            // SAFETY: plain Win32 calls with valid arguments; failures return null/0.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return 0;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    return 0;
                }
                job as usize
            }
        });
        (handle != 0).then_some(handle)
    }

    pub fn adopt(child: &tokio::process::Child) {
        let (Some(job), Some(process)) = (job(), child.raw_handle()) else {
            tracing::debug!("A child process couldn't join the server's job; it may outlive a crash.");
            return;
        };
        // SAFETY: both handles are valid for the duration of the call.
        unsafe {
            AssignProcessToJobObject(job as *mut core::ffi::c_void, process);
        }
    }
}
