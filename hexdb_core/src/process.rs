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
