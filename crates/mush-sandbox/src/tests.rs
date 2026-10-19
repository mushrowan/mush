use super::*;
use std::path::Path;

fn config(text: &str) -> Result<Config> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sandbox.toml");
    std::fs::write(&path, text).unwrap();
    Config::load(&path).map(Option::unwrap)
}

#[test]
fn absent_config_is_optional() {
    let dir = tempfile::tempdir().unwrap();
    assert!(Config::load(&dir.path().join("absent")).unwrap().is_none());
}

#[test]
fn unknown_keys_are_errors() {
    for text in [
        "typo = true",
        "[profile.dev]\ntyop = []",
        "[project.\"/repo\"]\ntyop = []",
        "[project.\"/repo\".profile.dev]\ntyop = []",
    ] {
        assert!(config(text).is_err(), "{text}");
    }
}

#[test]
fn profile_and_project_extensions_merge() {
    let cfg = config(
        r#"
[profile.dev]
read = ["/shared"]
allowed_host_requests = ["attach_image"]
[project."/repo"]
default = "dev"
[project."/repo".profile.dev]
write = ["/build"]
websearch = true
"#,
    )
    .unwrap();
    let profile = cfg.resolve(Path::new("/repo"), None).unwrap();
    assert_eq!(profile.read(), &[PathBuf::from("/shared")]);
    assert_eq!(profile.write(), &[PathBuf::from("/build")]);
    assert!(profile.websearch());
    assert!(profile.allows_host_request("attach_image"));
    assert!(profile.allows_host_request("edit"));
    assert!(!profile.allows_host_request("arbitrary"));
}

#[test]
fn explicit_profile_overrides_default() {
    let cfg = config(
        r#"
[profile.a]
websearch = true
[profile.b]
[project."/repo"]
default = "a"
"#,
    )
    .unwrap();
    assert!(
        !cfg.resolve(Path::new("/repo"), Some("b"))
            .unwrap()
            .websearch()
    );
    assert!(cfg.resolve(Path::new("/repo"), Some("missing")).is_err());
}

#[test]
fn network_is_rejected_even_in_unused_profile() {
    assert!(config("[profile.dev]\nnetwork = [\"example.com\"]").is_err());
}

#[test]
fn relative_grants_and_projects_are_rejected() {
    assert!(config("[profile.dev]\nread = [\"relative\"]").is_err());
    assert!(config("[project.relative]").is_err());
}

#[test]
fn baseline_has_only_safe_host_requests() {
    let profile = Profile::default();
    assert!(profile.allows_host_request("bash.start"));
    assert!(profile.allows_host_request("edit"));
    assert!(!profile.allows_host_request("arbitrary"));
    assert!(!profile.websearch());
}

#[test]
fn project_can_disable_websearch() {
    let cfg = config(
        r#"
[profile.dev]
websearch = true
allowed_host_requests = ["websearch"]
[project."/repo"]
default = "dev"
[project."/repo".profile.dev]
websearch = false
"#,
    )
    .unwrap();
    let profile = cfg.resolve(Path::new("/repo"), None).unwrap();
    assert!(!profile.websearch());
    assert!(!profile.allows_host_request("websearch"));
}

#[test]
#[cfg_attr(not(mush_landlock_supported), ignore = "requires Linux Landlock ABI 9")]
fn secret_inside_workspace_is_rejected() {
    Sandbox::check_support().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret");
    std::fs::write(&secret, "private").unwrap();
    let cfg = config(&format!(
        "[profile.dev]\nsecrets = [{}]",
        toml::Value::String(secret.display().to_string())
    ))
    .unwrap();
    let profile = cfg.resolve(Path::new("/repo"), Some("dev")).unwrap();
    assert!(matches!(
        Sandbox::new(profile, dir.path(), &[], &[]),
        Err(Error::WritableOverlap { .. })
    ));
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[cfg_attr(not(mush_landlock_supported), ignore = "requires Linux Landlock ABI 9")]
async fn enforcement_blocks_files_sockets_and_keyrings() {
    Sandbox::check_support().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let readonly = outside.path().join("readonly");
    let denied = outside.path().join("denied");
    std::fs::write(&readonly, "readable").unwrap();
    std::fs::write(&denied, "private").unwrap();
    let socket = outside.path().join("host.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    use std::os::linux::net::SocketAddrExt;
    let abstract_name = format!("mush-sandbox-test-{}", std::process::id());
    let address =
        std::os::unix::net::SocketAddr::from_abstract_name(abstract_name.as_bytes()).unwrap();
    let _abstract_listener = std::os::unix::net::UnixListener::bind_addr(&address).unwrap();
    let executable = std::env::current_exe().unwrap();
    let sandbox = Sandbox::new(
        Profile::default(),
        workspace.path(),
        &[readonly.clone(), executable.clone()],
        &[],
    )
    .unwrap();
    let mut command = tokio::process::Command::new(&executable);
    command.args(["--exact", "tests::enforcement_probe", "--nocapture"]);
    command.env("OPENAI_API_KEY", "not-a-real-key");
    sandbox.prepare_command(&mut command).unwrap();
    command.env("MUSH_SANDBOX_PROBE", "1");
    command.env("MUSH_PROBE_WORKSPACE", workspace.path());
    command.env("MUSH_PROBE_READONLY", &readonly);
    command.env("MUSH_PROBE_DENIED", &denied);
    command.env("MUSH_PROBE_SOCKET", &socket);
    command.env("MUSH_PROBE_ABSTRACT", &abstract_name);
    command.env("MUSH_PROBE_PARENT", std::process::id().to_string());
    let output = command.output().await.unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn enforcement_probe() {
    if std::env::var_os("MUSH_SANDBOX_PROBE").is_none() {
        return;
    }
    let workspace = PathBuf::from(std::env::var_os("MUSH_PROBE_WORKSPACE").unwrap());
    let readonly = PathBuf::from(std::env::var_os("MUSH_PROBE_READONLY").unwrap());
    let denied = PathBuf::from(std::env::var_os("MUSH_PROBE_DENIED").unwrap());
    assert!(std::env::var_os("OPENAI_API_KEY").is_none());
    assert_eq!(std::fs::read_to_string(&readonly).unwrap(), "readable");
    assert!(std::fs::write(&readonly, "no").is_err());
    assert!(std::fs::read_to_string(&denied).is_err());
    assert!(std::fs::write(&denied, "no").is_err());
    std::fs::write(workspace.join("yes"), "yes").unwrap();
    let temp = PathBuf::from(std::env::var_os("TMPDIR").unwrap());
    assert_ne!(temp, Path::new("/tmp"));
    std::fs::write(temp.join("yes"), "yes").unwrap();
    assert!(std::fs::write("/tmp/mush-sandbox-denied", "no").is_err());
    for family in [
        libc::AF_INET,
        libc::AF_INET6,
        libc::AF_PACKET,
        libc::AF_VSOCK,
        libc::AF_BLUETOOTH,
        libc::AF_NETLINK,
    ] {
        assert_eq!(unsafe { libc::socket(family, libc::SOCK_DGRAM, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
        let mut fds = [-1; 2];
        assert_eq!(
            unsafe { libc::socketpair(family, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
    }
    for syscall in [
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_io_uring_setup,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
    ] {
        assert_eq!(unsafe { libc::syscall(syscall, 0, 0, 0, 0, 0, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
    }
    #[cfg(target_arch = "x86_64")]
    {
        assert_eq!(unsafe { libc::syscall(0x4000_0000 | libc::SYS_getpid) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
    }
    let (mut a, mut b) = std::os::unix::net::UnixStream::pair().unwrap();
    use std::io::{Read, Write};
    a.write_all(b"ok").unwrap();
    let mut bytes = [0; 2];
    b.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"ok");
    let listener = std::os::unix::net::UnixListener::bind(workspace.join("ipc")).unwrap();
    let _stream = std::os::unix::net::UnixStream::connect(
        listener.local_addr().unwrap().as_pathname().unwrap(),
    )
    .unwrap();
    let outside_socket = PathBuf::from(std::env::var_os("MUSH_PROBE_SOCKET").unwrap());
    assert!(std::os::unix::net::UnixStream::connect(outside_socket).is_err());
    use std::os::linux::net::SocketAddrExt;
    let abstract_name = std::env::var("MUSH_PROBE_ABSTRACT").unwrap();
    let address =
        std::os::unix::net::SocketAddr::from_abstract_name(abstract_name.as_bytes()).unwrap();
    assert!(std::os::unix::net::UnixStream::connect_addr(&address).is_err());
    let parent: i32 = std::env::var("MUSH_PROBE_PARENT").unwrap().parse().unwrap();
    assert_eq!(unsafe { libc::kill(parent, libc::SIGCONT) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EPERM)
    );
}

#[test]
fn longest_configured_project_ancestor_wins() {
    let cfg = config(
        r#"
[profile.wide]
websearch = true
[profile.narrow]
[project."/repo"]
default = "wide"
[project."/repo/private"]
default = "narrow"
"#,
    )
    .unwrap();
    assert!(
        !cfg.resolve(Path::new("/repo/private/nested"), None)
            .unwrap()
            .websearch()
    );
    assert!(
        cfg.resolve(Path::new("/repo/public"), None)
            .unwrap()
            .websearch()
    );
    assert!(
        !cfg.resolve(Path::new("/repo-unrelated"), None)
            .unwrap()
            .websearch()
    );
}

#[cfg(target_os = "linux")]
#[test]
#[cfg_attr(not(mush_landlock_supported), ignore = "requires Linux Landlock ABI 9")]
fn snapshot_uses_canonical_grants() {
    Sandbox::check_support().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let granted = outside.path().join("granted");
    std::fs::create_dir(&granted).unwrap();
    let alias = workspace.path().join("alias");
    std::os::unix::fs::symlink(&granted, &alias).unwrap();
    let cfg = config(&format!(
        "[profile.dev]\nread = [{}]",
        toml::Value::String(alias.display().to_string())
    ))
    .unwrap();
    let profile = cfg.resolve(Path::new("/repo"), Some("dev")).unwrap();
    let sandbox = Sandbox::new(profile, workspace.path(), &[], &[]).unwrap();
    assert_eq!(sandbox.profile().read(), &[granted.canonicalize().unwrap()]);
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink("/etc", &alias).unwrap();
    let inherited = Sandbox::new(sandbox.profile().clone(), workspace.path(), &[], &[]).unwrap();
    assert_eq!(inherited.profile().read(), sandbox.profile().read());
    assert!(!inherited.allows_read(Path::new("/etc/passwd")));
}

#[cfg(target_os = "linux")]
#[test]
#[cfg_attr(not(mush_landlock_supported), ignore = "requires Linux Landlock ABI 9")]
fn brokered_edit_confines_paths_and_requires_unique_text() {
    Sandbox::check_support().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let path = workspace.path().join("file");
    let external = outside.path().join("file");
    std::fs::write(&path, "unique text").unwrap();
    std::fs::write(&external, "unique text").unwrap();
    let sandbox = Sandbox::new(Profile::default(), workspace.path(), &[], &[]).unwrap();
    assert!(sandbox.edit_file(&external, "unique", "changed").is_err());
    assert!(
        sandbox
            .edit_file(&workspace.path().join("../file"), "unique", "changed")
            .is_err()
    );
    let link = workspace.path().join("link");
    std::os::unix::fs::symlink(&external, &link).unwrap();
    assert!(sandbox.edit_file(&link, "unique", "changed").is_err());
    let directory_link = workspace.path().join("directory-link");
    std::os::unix::fs::symlink(outside.path(), &directory_link).unwrap();
    assert!(
        sandbox
            .edit_file(&directory_link.join("file"), "unique", "changed")
            .is_err()
    );
    let hardlink = workspace.path().join("hardlink");
    std::fs::hard_link(&external, &hardlink).unwrap();
    assert!(sandbox.edit_file(&hardlink, "unique", "changed").is_err());
    assert!(sandbox.edit_file(&path, "", "changed").is_err());
    assert!(sandbox.edit_file(&path, "missing", "changed").is_err());
    sandbox.edit_file(&path, "unique", "changed").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "changed text");
    assert_eq!(std::fs::read_to_string(&external).unwrap(), "unique text");
    std::fs::write(&path, "repeat repeat").unwrap();
    assert!(sandbox.edit_file(&path, "repeat", "changed").is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "repeat repeat");
}

#[test]
fn unconfigured_repository_uses_builtin_baseline() {
    let cfg = config(
        r#"
[profile.unselected]
websearch = true
read = ["/etc"]
write = ["/build"]
[project."/elsewhere"]
default = "unselected"
"#,
    )
    .unwrap();
    assert_eq!(
        cfg.resolve(Path::new("/repo"), None).unwrap(),
        Profile::default()
    );
    let empty = config("").unwrap();
    assert_eq!(
        empty.resolve(Path::new("/repo"), None).unwrap(),
        Profile::default()
    );
}
