use super::{Result, Sandbox};
use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreated, RulesetCreatedAttr, Scope,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule,
};
use std::collections::BTreeMap;

pub(super) fn ruleset(sandbox: &Sandbox) -> Result<RulesetCreated> {
    ruleset_paths(&sandbox.read, &sandbox.write, &sandbox.secrets)
}

pub(super) fn ruleset_paths(
    read: &[std::path::PathBuf],
    write: &[std::path::PathBuf],
    secrets: &[std::path::PathBuf],
) -> Result<RulesetCreated> {
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V9))?
        .scope(Scope::AbstractUnixSocket | Scope::Signal)?
        .create()?;
    for (paths, access) in [
        (read, AccessFs::from_read(ABI::V9)),
        (secrets, AccessFs::from_read(ABI::V9)),
        (write, AccessFs::from_all(ABI::V9)),
    ] {
        for path in paths {
            let descriptor = open_root(path)?;
            let rights = if descriptor.metadata()?.is_dir() {
                access
            } else {
                access & AccessFs::from_file(ABI::V9)
            };
            ruleset = ruleset.add_rule(PathBeneath::new(descriptor, rights))?;
        }
    }
    ruleset = ruleset.add_rule(PathBeneath::new(
        PathFd::new("/dev/null")?,
        AccessFs::ReadFile | AccessFs::WriteFile,
    ))?;
    ruleset = ruleset.add_rule(PathBeneath::new(
        PathFd::new("/dev/urandom")?,
        AccessFs::ReadFile,
    ))?;
    Ok(ruleset)
}

pub(super) fn filter(allow_inet: bool) -> Result<BpfProgram> {
    let mut rules = BTreeMap::new();
    for syscall in [
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_io_uring_setup,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
    ] {
        rules.insert(syscall, vec![]);
    }
    let condition = |arg, op, value| SeccompCondition::new(arg, SeccompCmpArgLen::Dword, op, value);
    let mut socket_rules = vec![SeccompRule::new(vec![
        condition(0, SeccompCmpOp::Ne, libc::AF_UNIX as u64)?,
        condition(
            0,
            SeccompCmpOp::Ne,
            if allow_inet {
                libc::AF_INET as u64
            } else {
                libc::AF_UNIX as u64
            },
        )?,
        condition(
            0,
            SeccompCmpOp::Ne,
            if allow_inet {
                libc::AF_INET6 as u64
            } else {
                libc::AF_UNIX as u64
            },
        )?,
    ])?];
    if allow_inet {
        for family in [libc::AF_INET, libc::AF_INET6] {
            socket_rules.push(SeccompRule::new(vec![
                condition(0, SeccompCmpOp::Eq, family as u64)?,
                condition(2, SeccompCmpOp::Ne, 0)?,
                condition(2, SeccompCmpOp::Ne, libc::IPPROTO_TCP as u64)?,
            ])?);
            socket_rules.push(SeccompRule::new(vec![
                condition(0, SeccompCmpOp::Eq, family as u64)?,
                condition(1, SeccompCmpOp::Ne, libc::SOCK_STREAM as u64)?,
                condition(
                    1,
                    SeccompCmpOp::Ne,
                    (libc::SOCK_STREAM | libc::SOCK_CLOEXEC) as u64,
                )?,
                condition(
                    1,
                    SeccompCmpOp::Ne,
                    (libc::SOCK_STREAM | libc::SOCK_NONBLOCK) as u64,
                )?,
                condition(
                    1,
                    SeccompCmpOp::Ne,
                    (libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK) as u64,
                )?,
            ])?);
        }
    }
    rules.insert(libc::SYS_socket, socket_rules.clone());
    rules.insert(libc::SYS_socketpair, socket_rules);
    let mut clone_rules = Vec::new();
    for flag in [
        libc::CLONE_NEWUSER,
        libc::CLONE_NEWNET,
        libc::CLONE_NEWNS,
        libc::CLONE_NEWPID,
        libc::CLONE_NEWIPC,
        libc::CLONE_NEWUTS,
        libc::CLONE_NEWCGROUP,
    ] {
        clone_rules.push(SeccompRule::new(vec![condition(
            0,
            SeccompCmpOp::MaskedEq(flag as u64),
            flag as u64,
        )?])?);
    }
    rules.insert(libc::SYS_clone, clone_rules);
    let program: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        std::env::consts::ARCH
            .try_into()
            .map_err(|_| super::Error::Architecture)?,
    )?
    .try_into()?;
    let program = {
        let mut protected = vec![
            seccompiler::sock_filter {
                code: 0x20,
                jt: 0,
                jf: 0,
                k: 0,
            },
            seccompiler::sock_filter {
                code: 0x15,
                jt: 0,
                jf: 1,
                k: libc::SYS_clone3 as u32,
            },
            seccompiler::sock_filter {
                code: 0x06,
                jt: 0,
                jf: 0,
                k: libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32,
            },
        ];
        protected.extend(program);
        protected
    };
    #[cfg(target_arch = "x86_64")]
    let program = {
        let mut protected = vec![
            seccompiler::sock_filter {
                code: 0x20,
                jt: 0,
                jf: 0,
                k: 0,
            },
            seccompiler::sock_filter {
                code: 0x35,
                jt: 0,
                jf: 1,
                k: 0x4000_0000,
            },
            seccompiler::sock_filter {
                code: 0x06,
                jt: 0,
                jf: 0,
                k: libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
            },
        ];
        protected.extend(program);
        protected
    };
    Ok(program)
}

fn open_root(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let how = OpenHow {
        flags: (libc::O_PATH | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: 0x04,
    };
    let descriptor = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(descriptor as i32) })
}
