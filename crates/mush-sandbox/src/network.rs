use crate::{Error, Result};
use std::net::IpAddr;
#[cfg(target_os = "linux")]
use std::net::SocketAddr;

fn parse(entry: &str) -> Result<(&str, u16)> {
    let invalid = || Error::NetworkEntry(entry.into());
    let (host, port, bracketed) = if let Some(rest) = entry.strip_prefix('[') {
        let (host, port) = rest.split_once("]:").ok_or_else(invalid)?;
        (host, port, true)
    } else {
        let (host, port) = entry.rsplit_once(':').ok_or_else(invalid)?;
        (host, port, false)
    };
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(invalid)?;
    if bracketed {
        host.parse::<std::net::Ipv6Addr>().map_err(|_| invalid())?;
    } else if host.parse::<std::net::Ipv4Addr>().is_err() {
        let name = host.strip_suffix('.').unwrap_or(host);
        if name.len() > 253
            || name.split('.').any(|label| {
                label.is_empty()
                    || label.len() > 63
                    || !label.as_bytes()[0].is_ascii_alphanumeric()
                    || !label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                    || !label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
        {
            return Err(invalid());
        }
    }
    Ok((host.strip_suffix('.').unwrap_or(host), port))
}

pub(crate) fn validate_entry(entry: &str) -> Result<()> {
    parse(entry).map(|_| ())
}

#[cfg(target_os = "linux")]
pub(crate) fn append_host(hosts: &mut String, host: &str, address: IpAddr) {
    if host.parse::<IpAddr>().is_err() {
        hosts.push_str(&format!("{address} {host}\n"));
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn endpoint_at(endpoints: &[SocketAddr], index: u32) -> std::io::Result<SocketAddr> {
    endpoints.get(index as usize).copied().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unlisted network destination",
        )
    })
}

#[cfg(target_os = "linux")]
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct Manifest {
    pub endpoints: Vec<SocketAddr>,
    pub socket: std::path::PathBuf,
    pub read: Vec<std::path::PathBuf>,
    pub write: Vec<std::path::PathBuf>,
    pub secrets: Vec<std::path::PathBuf>,
    pub command: Vec<Vec<u8>>,
}

#[cfg(target_os = "linux")]
mod host {
    use super::*;
    use std::net::ToSocketAddrs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;

    pub(crate) struct Network {
        pub directory: tempfile::TempDir,
        pub endpoints: Vec<SocketAddr>,
        pub bwrap: PathBuf,
        pub helper: PathBuf,
        shutdown: Option<tokio::sync::oneshot::Sender<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl std::fmt::Debug for Network {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Network")
                .field("endpoints", &self.endpoints)
                .finish_non_exhaustive()
        }
    }

    impl Drop for Network {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn executable(name: &str) -> Option<PathBuf> {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .map(|path| path.join(name))
            .find(|path| path.is_file())
    }

    pub(crate) fn tools() -> Result<(PathBuf, PathBuf)> {
        let bwrap = option_env!("MUSH_BWRAP")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute() && p.is_file())
            .or_else(|| executable("bwrap"))
            .ok_or_else(|| Error::NetworkUnavailable("bwrap was not found".into()))?;
        let executable = std::env::current_exe()?;
        let parent = executable
            .parent()
            .ok_or_else(|| Error::NetworkUnavailable("executable has no parent".into()))?;
        let helper = [
            parent.join("mush-sandbox-net"),
            parent.join("../mush-sandbox-net"),
        ]
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| {
            Error::NetworkUnavailable(format!(
                "mush-sandbox-net was not found beside {}",
                executable.display()
            ))
        })?;
        Ok((bwrap, helper))
    }

    impl Network {
        pub(crate) fn check_namespaces(&self) -> Result<()> {
            let status = std::process::Command::new(&self.bwrap)
                .args([
                    "--unshare-user",
                    "--unshare-net",
                    "--cap-add",
                    "CAP_NET_ADMIN",
                    "--cap-add",
                    "CAP_NET_BIND_SERVICE",
                    "--cap-add",
                    "CAP_SETPCAP",
                    "--ro-bind",
                    "/",
                    "/",
                    "--",
                    self.helper
                        .to_str()
                        .ok_or_else(|| Error::NetworkUnavailable("non-UTF-8 helper path".into()))?,
                ])
                .arg("--check")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            if !status.is_ok_and(|s| s.success()) {
                return Err(Error::NetworkUnavailable(
                    "bubblewrap namespace probe failed".into(),
                ));
            }
            Ok(())
        }

        pub(crate) fn new(entries: &[String]) -> Result<Arc<Self>> {
            let (bwrap, helper) = tools()?;
            let directory = tempfile::Builder::new()
                .prefix("mush-net-")
                .tempdir_in("/tmp")?;
            let mut endpoints = Vec::new();
            let mut hosts = String::new();
            for entry in entries {
                let (host, port) = parse(entry)?;
                let resolved: Vec<_> = (host, port).to_socket_addrs()?.collect();
                if resolved.is_empty() {
                    return Err(Error::NetworkEntry(entry.clone()));
                }
                for endpoint in resolved {
                    if endpoint.ip().is_unspecified() || endpoint.ip().is_multicast() {
                        return Err(Error::NetworkEntry(entry.clone()));
                    }
                    if !endpoints.contains(&endpoint) {
                        endpoints.push(endpoint);
                    }
                    append_host(&mut hosts, host, endpoint.ip());
                }
            }
            std::fs::write(directory.path().join("hosts"), hosts)?;
            std::fs::write(directory.path().join("resolv.conf"), "")?;
            std::fs::write(directory.path().join("nsswitch.conf"), "hosts: files\n")?;
            let listener = std::os::unix::net::UnixListener::bind(directory.path().join("relay"))?;
            listener.set_nonblocking(true)?;
            let allowed = endpoints.clone();
            let (shutdown, mut stop) = tokio::sync::oneshot::channel();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let thread = std::thread::Builder::new().name("mush-net-relay".into()).spawn(move || {
                runtime.block_on(async move {
                    let Ok(listener) = tokio::net::UnixListener::from_std(listener) else { return; };
                    let mut tasks = tokio::task::JoinSet::new();
                    loop {
                        tokio::select! {
                            _ = &mut stop => break,
                            Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
                            accepted = listener.accept(), if tasks.len() < 64 => {
                                let Ok((mut stream, _)) = accepted else { break; };
                                let endpoints = allowed.clone();
                                tasks.spawn(async move {
                                    let index = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read_u32()).await??;
                                    let endpoint = endpoint_at(&endpoints, index)?;
                                    let mut destination = tokio::time::timeout(std::time::Duration::from_secs(10), tokio::net::TcpStream::connect(endpoint)).await??;
                                    tokio::io::copy_bidirectional(&mut stream, &mut destination).await?;
                                    Ok::<_, std::io::Error>(())
                                });
                            }
                        }
                    }
                    tasks.shutdown().await;
                });
            })?;
            Ok(Arc::new(Self {
                directory,
                endpoints,
                bwrap,
                helper,
                shutdown: Some(shutdown),
                thread: Some(thread),
            }))
        }

        pub(crate) fn wrap(
            &self,
            command: &mut tokio::process::Command,
            read: &[PathBuf],
            write: &[PathBuf],
            secrets: &[PathBuf],
        ) -> Result<()> {
            use std::os::unix::ffi::OsStrExt;
            let original = command.as_std();
            let manifest = Manifest {
                endpoints: self.endpoints.clone(),
                socket: self.directory.path().join("relay"),
                read: read
                    .iter()
                    .cloned()
                    .chain(
                        ["/etc/hosts", "/etc/resolv.conf", "/etc/nsswitch.conf"].map(PathBuf::from),
                    )
                    .collect(),
                write: write.into(),
                secrets: secrets.into(),
                command: std::iter::once(original.get_program())
                    .chain(original.get_args())
                    .map(|s| s.as_bytes().to_vec())
                    .collect(),
            };
            let mut file = tempfile::NamedTempFile::new_in(self.directory.path())?;
            serde_json::to_writer(&mut file, &manifest).map_err(std::io::Error::other)?;
            let (_, path) = file.keep().map_err(|e| e.error)?;
            let mut wrapped = std::process::Command::new(&self.bwrap);
            wrapped.args([
                "--die-with-parent",
                "--unshare-user",
                "--unshare-net",
                "--cap-add",
                "CAP_NET_ADMIN",
                "--cap-add",
                "CAP_NET_BIND_SERVICE",
                "--cap-add",
                "CAP_SETPCAP",
                "--ro-bind",
                "/",
                "/",
                "--tmpfs",
                "/run",
                "--tmpfs",
                "/etc",
            ]);
            for root in read.iter().chain(secrets) {
                if root.starts_with("/run") || root.starts_with("/etc") {
                    wrapped.arg("--ro-bind").arg(root).arg(root);
                }
            }
            for root in write {
                wrapped.arg("--bind").arg(root).arg(root);
            }
            for file in ["hosts", "resolv.conf", "nsswitch.conf"] {
                wrapped
                    .arg("--ro-bind")
                    .arg(self.directory.path().join(file))
                    .arg(Path::new("/etc").join(file));
            }
            wrapped.arg("--").arg(&self.helper).arg(path);
            if let Some(cwd) = original.get_current_dir() {
                wrapped.current_dir(cwd);
            }
            wrapped.env_clear();
            for (key, value) in original.get_envs() {
                if let Some(value) = value {
                    wrapped.env(key, value);
                }
            }
            *command = wrapped.into();
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use host::Network;
