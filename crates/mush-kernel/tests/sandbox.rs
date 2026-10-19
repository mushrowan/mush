#![cfg(target_os = "linux")]

use std::sync::Arc;

use mush_kernel::{Kernel, KernelConfig, NoHost};
use mush_sandbox::{Profile, Sandbox};

#[allow(clippy::unwrap_used)]
async fn confined(workspace: &std::path::Path) -> Kernel {
    let sandbox = Sandbox::new(Profile::default(), workspace, &[], &[]).unwrap();
    Kernel::start(
        KernelConfig {
            cwd: workspace.to_path_buf(),
            sandbox: Some(Arc::new(sandbox)),
            env: vec![
                ("AWS_SECRET_ACCESS_KEY".into(), "private".into()),
                ("MUSH_CONTROL_SOCKET".into(), "/secret.sock".into()),
            ],
            ..KernelConfig::default()
        },
        Arc::new(NoHost),
    )
    .await
    .unwrap()
}

#[allow(clippy::unwrap_used)]
async fn probe(code: &str) {
    let workspace = tempfile::tempdir().unwrap();
    let kernel = confined(workspace.path()).await;
    let output = kernel.execute(code).await.unwrap();
    kernel.shutdown().await.unwrap();
    assert!(output.ok, "{output:?}");
}

#[tokio::test]
#[cfg_attr(not(mush_landlock_supported), ignore = "requires Linux Landlock ABI 9")]
async fn sandbox_kernel_denies_outside_ssh_read() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join(".ssh/id_ed25519");
    std::fs::create_dir(secret.parent().unwrap()).unwrap();
    std::fs::write(&secret, "private").unwrap();
    let kernel = confined(workspace.path()).await;
    let output = kernel
        .execute(&format!(
            r#"
import pathlib
try:
    pathlib.Path({secret:?}).read_text()
except PermissionError:
    pass
else:
    raise AssertionError('outside SSH read allowed')
"#,
            secret = secret.to_str().unwrap()
        ))
        .await
        .unwrap();
    kernel.shutdown().await.unwrap();
    assert!(output.ok, "{output:?}");
}

#[tokio::test]
#[cfg_attr(not(mush_landlock_supported), ignore = "requires Linux Landlock ABI 9")]
async fn sandbox_kernel_denies_internet_sockets() {
    probe(
        r#"
import socket
for family in (socket.AF_INET, socket.AF_INET6):
    try:
        socket.socket(family, socket.SOCK_STREAM)
    except PermissionError:
        pass
    else:
        raise AssertionError('internet socket allowed')
"#,
    )
    .await;
}

#[tokio::test]
#[cfg_attr(not(mush_landlock_supported), ignore = "requires Linux Landlock ABI 9")]
async fn sandbox_kernel_denies_keyring_syscalls() {
    probe(
        r#"
import ctypes, errno, platform
libc = ctypes.CDLL(None, use_errno=True)
keyctl = {'x86_64': 250, 'aarch64': 219}[platform.machine()]
assert libc.syscall(keyctl, 11, -3, 0, 0, 0) == -1
assert ctypes.get_errno() == errno.EPERM
numbers = {'x86_64': (248, 249, 250), 'aarch64': (217, 218, 219)}[platform.machine()]
for number in numbers:
    assert libc.syscall(number, -1, 0, 0, 0, 0) == -1
    assert ctypes.get_errno() == errno.EPERM
"#,
    )
    .await;
}

#[tokio::test]
#[cfg_attr(not(mush_landlock_supported), ignore = "requires Linux Landlock ABI 9")]
async fn sandbox_kernel_workspace_is_writable_runtime_is_readonly_environment_is_clean() {
    probe(
        r#"
import os, pathlib, sys, tempfile
assert 'AWS_SECRET_ACCESS_KEY' not in os.environ
assert 'MUSH_CONTROL_SOCKET' not in os.environ
pathlib.Path('allowed').write_text('ok')
assert pathlib.Path('allowed').read_text() == 'ok'
with tempfile.NamedTemporaryFile() as f:
    f.write(b'private temp')
binary = pathlib.Path(sys.executable).resolve()
assert binary.read_bytes()[:4] == b'\x7fELF'
try:
    binary.open('r+b')
except (PermissionError, OSError):
    pass
else:
    raise AssertionError('runtime write allowed')
"#,
    )
    .await;
}

#[tokio::test]
#[cfg_attr(not(mush_landlock_supported), ignore = "requires Linux Landlock ABI 9")]
async fn sandbox_does_not_bootstrap_an_unprepared_managed_environment() {
    let workspace = tempfile::tempdir().unwrap();
    let venv = mush_kernel::ManagedVenv::at(workspace.path().join("missing-venv"));
    let sandbox = Sandbox::new(Profile::default(), workspace.path(), &[], &[]).unwrap();
    let result = Kernel::start(
        KernelConfig {
            managed_venv: Some(venv.clone()),
            cwd: workspace.path().to_path_buf(),
            sandbox: Some(Arc::new(sandbox)),
            ..KernelConfig::default()
        },
        Arc::new(NoHost),
    )
    .await;
    if std::env::var_os(mush_kernel::python::PYTHON_ENV).is_some() {
        result.unwrap().shutdown().await.unwrap();
    } else {
        assert!(result.is_err());
    }
    assert!(!venv.dir.exists());
}

struct Echo;

#[async_trait::async_trait]
impl mush_kernel::HostRequestHandler for Echo {
    async fn handle(
        &self,
        kind: &str,
        _: serde_json::Value,
    ) -> Result<serde_json::Value, mush_kernel::HostError> {
        Ok(serde_json::json!({"kind": kind}))
    }
}

#[tokio::test]
#[cfg_attr(not(mush_landlock_supported), ignore = "requires Linux Landlock ABI 9")]
async fn sandbox_gates_custom_host_handlers_on_stdio_channel() {
    let workspace = tempfile::tempdir().unwrap();
    let sandbox = Sandbox::new(Profile::default(), workspace.path(), &[], &[]).unwrap();
    let kernel = Kernel::start(
        KernelConfig {
            cwd: workspace.path().to_path_buf(),
            sandbox: Some(Arc::new(sandbox)),
            ..KernelConfig::default()
        },
        Arc::new(Echo),
    )
    .await
    .unwrap();
    let output = kernel
        .execute(
            r#"
assert (await host_request('bash.poll', {}))['kind'] == 'bash.poll'
try:
    await host_request('harness.get', {})
except RuntimeError:
    pass
else:
    raise AssertionError('custom host handler bypassed gate')
"#,
        )
        .await
        .unwrap();
    kernel.shutdown().await.unwrap();
    assert!(output.ok, "{output:?}");
}
