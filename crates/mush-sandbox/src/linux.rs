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
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V9))?
        .scope(Scope::AbstractUnixSocket | Scope::Signal)?
        .create()?;
    for (paths, access) in [
        (&sandbox.read, AccessFs::from_read(ABI::V9)),
        (&sandbox.secrets, AccessFs::from_read(ABI::V9)),
        (&sandbox.write, AccessFs::from_all(ABI::V9)),
    ] {
        for path in paths {
            let rights = if path.is_dir() {
                access
            } else {
                access & AccessFs::from_file(ABI::V9)
            };
            ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(path)?, rights))?;
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

pub(super) fn filter() -> Result<BpfProgram> {
    let mut rules = BTreeMap::new();
    for syscall in [
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_io_uring_setup,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
    ] {
        rules.insert(syscall, vec![]);
    }
    let socket_rules = vec![SeccompRule::new(vec![SeccompCondition::new(
        0,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Ne,
        libc::AF_UNIX as u64,
    )?])?];
    rules.insert(libc::SYS_socket, socket_rules.clone());
    rules.insert(libc::SYS_socketpair, socket_rules);
    let program: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        std::env::consts::ARCH
            .try_into()
            .map_err(|_| super::Error::Architecture)?,
    )?
    .try_into()?;
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
