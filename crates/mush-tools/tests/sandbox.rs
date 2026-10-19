#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use mush_agent::tool::AgentTool;
use mush_kernel::{HostError, HostRequestHandler};
use mush_sandbox::{Profile, Sandbox};
use mush_tools::ipython::{IpythonTool, MushHost};
use serde_json::{Value, json};

struct Extension(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl HostRequestHandler for Extension {
    async fn handle(&self, _kind: &str, _payload: Value) -> Result<Value, HostError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(json!({}))
    }
}

#[tokio::test]
#[cfg_attr(not(mush_landlock_supported), ignore = "requires Linux Landlock ABI 9")]
async fn sandbox_host_denies_extensions_and_edit_before_dispatch() {
    let workspace = tempfile::tempdir().unwrap();
    let sandbox = Arc::new(Sandbox::new(Profile::default(), workspace.path(), &[], &[]).unwrap());
    let host = MushHost::new(Arc::from(workspace.path()));
    let calls = Arc::new(AtomicUsize::new(0));
    host.set_extension(Arc::new(Extension(calls.clone())));
    host.set_sandbox(Some(sandbox));
    for kind in ["unknown", "harness.get", "rlm.spawn", "edit", "websearch"] {
        assert!(host.handle(kind, json!({})).await.is_err(), "{kind}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
#[cfg_attr(not(mush_landlock_supported), ignore = "requires Linux Landlock ABI 9")]
async fn sandbox_bash_uses_stdio_host_channel_and_confines_jobs() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("private");
    std::fs::write(&secret, "private").unwrap();
    std::fs::write(workspace.path().join(".envrc"), "export FROM_ENVRC=unsafe").unwrap();
    let sandbox = Arc::new(Sandbox::new(Profile::default(), workspace.path(), &[], &[]).unwrap());
    let tool = IpythonTool::new(Arc::from(workspace.path()))
        .with_sandbox(sandbox)
        .unwrap();
    let code = format!(
        r#"
import os, pathlib, sys
pathlib.Path('editable').write_text('before')
await edit('editable', 'before', 'after')
assert pathlib.Path('editable').read_text() == 'after'
result = await bash("printf safe > allowed; cat allowed; test -z \"$MUSH_CONTROL_SOCKET\"; test -z \"$FROM_ENVRC\"; test -z \"$AWS_SECRET_ACCESS_KEY\"")
assert result.ok, result.output
assert result.output == 'safe', result.output
assert pathlib.Path('allowed').read_text() == 'safe'
result = await bash('cat ' + {secret:?})
assert not result.ok
probe = "import socket; socket.socket(socket.AF_INET)"
result = await bash(sys.executable + ' -c ' + __import__('shlex').quote(probe))
assert not result.ok, result.output
probe = "import ctypes,errno,platform; c=ctypes.CDLL(None,use_errno=True); ns={{'x86_64':(248,249,250),'aarch64':(217,218,219)}}[platform.machine()]; assert all(c.syscall(n,-1,0,0,0,0)==-1 and ctypes.get_errno()==errno.EPERM for n in ns)"
result = await bash(sys.executable + ' -c ' + __import__('shlex').quote(probe))
assert result.ok, result.output
try:
    await host_request('edit', {{'path': {secret:?}, 'old': 'private', 'new': 'leaked'}})
except RuntimeError:
    pass
else:
    raise AssertionError('host edit allowed')
try:
    attach_image({secret:?})
except PermissionError:
    pass
else:
    raise AssertionError('outside attachment allowed')
print('confined')
"#,
        secret = secret.to_str().unwrap()
    );
    let result = tool.execute(json!({"code": code})).await;
    tool.shutdown().await;
    assert!(result.outcome.is_success(), "{result:?}");
    assert_eq!(std::fs::read_to_string(secret).unwrap(), "private");
}

#[allow(clippy::unwrap_used)]
async fn job_probe(code: &str) {
    let workspace = tempfile::tempdir().unwrap();
    let sandbox = Arc::new(Sandbox::new(Profile::default(), workspace.path(), &[], &[]).unwrap());
    let jobs = mush_tools::kernel_jobs::KernelJobs::default();
    jobs.set_sandbox(Some(sandbox));
    let python = mush_kernel::PythonPlan::resolve(None, None)
        .unwrap()
        .python_if_present()
        .unwrap();
    let quote = |text: &str| format!("'{}'", text.replace('\'', "'\"'\"'"));
    let command = format!("{} -c {}", quote(python.to_str().unwrap()), quote(code));
    let (id, _) = jobs
        .start(workspace.path(), &command, None, None, None)
        .await
        .unwrap();
    let output = jobs.wait(&id).await.unwrap();
    assert_eq!(output.exit_code, Some(0), "{output:?}");
}

#[tokio::test]
#[cfg_attr(not(mush_landlock_supported), ignore = "requires Linux Landlock ABI 9")]
async fn sandbox_bash_denies_outside_ssh_read() {
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join(".ssh/id_ed25519");
    std::fs::create_dir(secret.parent().unwrap()).unwrap();
    std::fs::write(&secret, "private").unwrap();
    job_probe(&format!(
        r#"
import pathlib
try:
    pathlib.Path({:?}).read_bytes()
except PermissionError:
    pass
else:
    raise AssertionError('outside SSH read allowed')
"#,
        secret.to_str().unwrap()
    ))
    .await;
}

#[tokio::test]
#[cfg_attr(not(mush_landlock_supported), ignore = "requires Linux Landlock ABI 9")]
async fn sandbox_bash_denies_internet_sockets() {
    job_probe(
        r#"
import socket
for family in (socket.AF_INET, socket.AF_INET6):
    try:
        socket.socket(family)
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
async fn sandbox_bash_denies_keyring_syscalls() {
    job_probe(
        r#"
import ctypes, errno, platform
c = ctypes.CDLL(None, use_errno=True)
keyctl = {'x86_64': 250, 'aarch64': 219}[platform.machine()]
assert c.syscall(keyctl, 11, -3, 0, 0, 0) == -1
assert ctypes.get_errno() == errno.EPERM
for number in {'x86_64': (248,249,250), 'aarch64': (217,218,219)}[platform.machine()]:
    assert c.syscall(number, -1, 0, 0, 0, 0) == -1
    assert ctypes.get_errno() == errno.EPERM
"#,
    )
    .await;
}

#[tokio::test]
#[cfg_attr(not(mush_landlock_supported), ignore = "requires Linux Landlock ABI 9")]
async fn sandbox_host_allowlist_is_exact_and_edit_uses_safe_broker() {
    let workspace = tempfile::tempdir().unwrap();
    let config_path = workspace.path().join("sandbox.toml");
    std::fs::write(
        &config_path,
        "[profile.test]\nallowed_host_requests = [\"trusted.request\", \"edit\", \"websearch\"]\n",
    )
    .unwrap();
    let profile = mush_sandbox::Config::load(&config_path)
        .unwrap()
        .unwrap()
        .resolve(workspace.path(), Some("test"))
        .unwrap();
    let sandbox = Arc::new(Sandbox::new(profile, workspace.path(), &[], &[]).unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let host = MushHost::new(Arc::from(workspace.path()));
    host.set_extension(Arc::new(Extension(calls.clone())));
    host.set_sandbox(Some(sandbox));
    assert!(host.handle("trusted.request", json!({})).await.is_ok());
    for kind in ["trusted.request.child", "edit", "websearch"] {
        assert!(host.handle(kind, json!({})).await.is_err(), "{kind}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let target = workspace.path().join("allowed");
    std::fs::write(&target, "before").unwrap();
    assert!(
        host.handle(
            "edit",
            json!({"path": "allowed", "old": "before", "new": "after"})
        )
        .await
        .is_ok()
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "after");
}

#[tokio::test]
#[cfg_attr(not(mush_landlock_supported), ignore = "requires Linux Landlock ABI 9")]
async fn sandbox_runtime_grants_reach_bash_and_setter_rejects_live_kernel() {
    let workspace = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let python = mush_kernel::PythonPlan::resolve(None, None)
        .unwrap()
        .python_if_present()
        .unwrap();
    let output = tokio::process::Command::new(&python)
        .args(["-m", "venv", "--without-pip"])
        .arg(runtime.path().join("venv"))
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut tool = IpythonTool::new(Arc::from(workspace.path()));
    tool.kernel_config_mut().python = Some(runtime.path().join("venv/bin/python"));
    let sandbox = Arc::new(Sandbox::new(Profile::default(), workspace.path(), &[], &[]).unwrap());
    tool.set_sandbox(Some(sandbox)).unwrap();
    let result = tool.execute(json!({"code": "import shlex, sys; r = await bash(shlex.quote(sys.executable) + ' -c ' + shlex.quote('print(42)')); assert r.ok and r.output == '42\\n', r.output"})).await;
    assert!(result.outcome.is_success(), "{result:?}");
    assert!(tool.set_sandbox(None).is_err());
    tool.shutdown().await;
}

#[tokio::test]
#[cfg_attr(not(mush_landlock_supported), ignore = "requires Linux Landlock ABI 9")]
async fn sandbox_direct_kernel_start_configures_host_jobs() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("private");
    std::fs::write(&secret, "private").unwrap();
    let sandbox = Arc::new(Sandbox::new(Profile::default(), workspace.path(), &[], &[]).unwrap());
    let host = Arc::new(MushHost::new(Arc::from(workspace.path())));
    let kernel = mush_kernel::Kernel::start(
        mush_kernel::KernelConfig {
            cwd: workspace.path().to_path_buf(),
            sandbox: Some(sandbox),
            ..mush_kernel::KernelConfig::default()
        },
        host,
    )
    .await
    .unwrap();
    let output = kernel
        .execute(&format!(
            "r = await bash('cat ' + {:?}); assert not r.ok",
            secret.to_str().unwrap()
        ))
        .await
        .unwrap();
    kernel.shutdown().await.unwrap();
    assert!(output.ok, "{output:?}");
}
