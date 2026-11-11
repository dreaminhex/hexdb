// The sandbox on Linux: rlimits, no new privileges, Landlock, seccomp, exec.

use super::{exec, is_within, set_rlimits, Policy};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

/// System folders a runtime (node, python, a binary) needs to read.
const SYSTEM_READ: &[&str] = &[
    "/usr", "/lib", "/lib32", "/lib64", "/libx32", "/bin", "/sbin", "/etc", "/proc", "/sys", "/dev", "/nix/store", "/run/systemd/resolve", "/run/resolvconf",
];
/// Devices it may also write.
const DEVICES: &[&str] = &["/dev/null", "/dev/zero", "/dev/full", "/dev/random", "/dev/urandom"];

pub fn run(policy: &Policy, program: &Path, args: &[String]) -> Result<i32> {
    set_rlimits(policy);
    // SAFETY: a plain prctl on this process.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        bail!("PR_SET_NO_NEW_PRIVS failed");
    }
    if !landlock::apply(policy, program)? {
        eprintln!("sandbox: this kernel has no Landlock (Linux 5.13 or later), so file access isn't restricted");
    }
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64"))]
    seccomp::apply(policy)?;
    exec(program, args)
}

/// Add `root` (or, if a denied path is inside it, its other children) to `rules`.
fn add_tree(rules: &mut Vec<(PathBuf, u64)>, root: &Path, access: u64, deny: &[PathBuf]) {
    if !root.exists() || deny.iter().any(|d| is_within(root, d)) {
        return;
    }
    if deny.iter().any(|d| is_within(d, root)) {
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                add_tree(rules, &entry.path(), access, deny);
            }
        }
        return;
    }
    rules.push((root.to_path_buf(), access));
}

mod landlock {
    use super::*;
    use std::os::fd::AsRawFd;

    const CREATE_RULESET_VERSION: libc::c_uint = 1;
    const RULE_PATH_BENEATH: libc::c_int = 1;

    const EXECUTE: u64 = 1 << 0;
    const WRITE_FILE: u64 = 1 << 1;
    const READ_FILE: u64 = 1 << 2;
    const READ_DIR: u64 = 1 << 3;
    const MAKE_CHAR: u64 = 1 << 6;
    const MAKE_BLOCK: u64 = 1 << 11;
    /// Rights 0..=12, Landlock ABI 1.
    const ABI1: u64 = (1 << 13) - 1;
    const REFER: u64 = 1 << 13;
    const TRUNCATE: u64 = 1 << 14;
    /// Rights that apply to files (the rest are about folders).
    const FILE_RIGHTS: u64 = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE;
    const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1;
    const SCOPE_SIGNAL: u64 = 2;

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
        scoped: u64,
    }

    #[repr(C, packed)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    /// Restrict this process's file access. False if the kernel has no Landlock.
    pub fn apply(policy: &Policy, program: &Path) -> Result<bool> {
        // SAFETY: the version query takes no pointers.
        let abi = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<RulesetAttr>(), 0usize, CREATE_RULESET_VERSION) };
        if abi < 1 {
            return Ok(false);
        }
        let mut handled = ABI1;
        if abi >= 2 {
            handled |= REFER;
        }
        if abi >= 3 {
            handled |= TRUNCATE;
        }
        let read = EXECUTE | READ_FILE | READ_DIR;
        let write = handled & !(MAKE_CHAR | MAKE_BLOCK);

        let mut rules = Vec::new();
        for root in SYSTEM_READ {
            add_tree(&mut rules, Path::new(root), read, &policy.deny);
        }
        for device in DEVICES {
            rules.push((PathBuf::from(device), READ_FILE | WRITE_FILE | (handled & TRUNCATE)));
        }
        // The program's folder, and the real one if it is a link (node from nvm, say).
        let mut program_dirs: Vec<PathBuf> = program.parent().map(Path::to_path_buf).into_iter().collect();
        if let Some(real) = std::fs::canonicalize(program).ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
            program_dirs.push(real);
        }
        for dir in program_dirs.into_iter().filter(|d| d.parent().is_some()) {
            rules.push((dir, read));
        }
        for path in &policy.read {
            rules.push((path.clone(), read));
        }
        for path in &policy.write {
            rules.push((path.clone(), write));
        }

        // ABI 6 adds scopes: no signals to, or abstract sockets of, processes outside the sandbox.
        let scoped = abi >= 6;
        let attr = RulesetAttr { handled_access_fs: handled, handled_access_net: 0, scoped: if scoped { SCOPE_SIGNAL | SCOPE_ABSTRACT_UNIX_SOCKET } else { 0 } };
        let size = if scoped { std::mem::size_of::<RulesetAttr>() } else { std::mem::size_of::<u64>() };
        // SAFETY: attr is valid for `size` bytes (a prefix of the struct for older ABIs).
        let ruleset = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &attr as *const RulesetAttr, size, 0u32) };
        if ruleset < 0 {
            bail!("landlock_create_ruleset failed: {}", std::io::Error::last_os_error());
        }
        let ruleset = ruleset as libc::c_int;
        for (path, access) in rules {
            let Ok(file) = std::fs::OpenOptions::new().read(true).custom_flags_path().open(&path) else { continue };
            let access = if file.metadata().map(|m| m.is_dir()).unwrap_or(false) { access } else { access & FILE_RIGHTS };
            let rule = PathBeneathAttr { allowed_access: access & handled, parent_fd: file.as_raw_fd() };
            // SAFETY: the rule is valid for the call; a refused rule only means less access.
            unsafe { libc::syscall(libc::SYS_landlock_add_rule, ruleset, RULE_PATH_BENEATH, &rule as *const PathBeneathAttr, 0u32) };
        }
        // SAFETY: restricts this process only; no_new_privs is already set.
        let restricted = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) };
        let error = std::io::Error::last_os_error();
        // SAFETY: the ruleset descriptor is ours and closed once.
        unsafe { libc::close(ruleset) };
        if restricted != 0 {
            bail!("landlock_restrict_self failed: {}", error);
        }
        Ok(true)
    }

    /// O_PATH opens: no read permission needed, works on folders and devices.
    trait PathOpen {
        fn custom_flags_path(&mut self) -> &mut Self;
    }

    impl PathOpen for std::fs::OpenOptions {
        fn custom_flags_path(&mut self) -> &mut Self {
            use std::os::unix::fs::OpenOptionsExt;
            self.custom_flags(libc::O_PATH | libc::O_CLOEXEC)
        }
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "riscv64"))]
mod seccomp {
    use super::Policy;
    use seccompiler::{BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule};
    use std::collections::BTreeMap;

    /// Refused outright: debugging other processes, kernel and namespace
    /// administration, and io_uring (whose operations skip this filter).
    const DENIED: &[libc::c_long] = &[
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_pidfd_getfd,
        libc::SYS_pidfd_send_signal,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_kexec_load,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_userfaultfd,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_reboot,
        libc::SYS_acct,
        libc::SYS_syslog,
        libc::SYS_open_by_handle_at,
        libc::SYS_name_to_handle_at,
    ];

    fn arg0(op: SeccompCmpOp, value: u64) -> anyhow::Result<SeccompCondition> {
        Ok(SeccompCondition::new(0, SeccompCmpArgLen::Dword, op, value)?)
    }

    pub fn apply(policy: &Policy) -> anyhow::Result<()> {
        // c_long is i64 on 64-bit Linux.
        #[allow(clippy::useless_conversion)]
        let mut rules: BTreeMap<i64, Vec<SeccompRule>> = DENIED.iter().map(|n| (i64::from(*n), Vec::new())).collect();
        // Sockets: none, or IPv4/IPv6 (and netlink, which name lookups use). Never Unix sockets.
        let socket = if policy.network {
            vec![SeccompRule::new(vec![
                arg0(SeccompCmpOp::Ne, libc::AF_INET as u64)?,
                arg0(SeccompCmpOp::Ne, libc::AF_INET6 as u64)?,
                arg0(SeccompCmpOp::Ne, libc::AF_NETLINK as u64)?,
            ])?]
        } else {
            Vec::new()
        };
        rules.insert(libc::SYS_socket, socket);
        // Signals only to itself (its pid stays the same across exec): never the server.
        let me = std::process::id() as u64;
        for call in [libc::SYS_kill, libc::SYS_tgkill, libc::SYS_rt_sigqueueinfo, libc::SYS_rt_tgsigqueueinfo] {
            rules.insert(call, vec![SeccompRule::new(vec![arg0(SeccompCmpOp::Ne, me)?])?]);
        }
        let filter = SeccompFilter::new(rules, SeccompAction::Allow, SeccompAction::Errno(libc::EPERM as u32), std::env::consts::ARCH.try_into()?)?;
        let program: BpfProgram = filter.try_into()?;
        seccompiler::apply_filter(&program)?;
        Ok(())
    }
}
