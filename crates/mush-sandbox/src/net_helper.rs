use crate::network::Manifest;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::task::JoinSet;

const MAX_FORWARDERS: usize = 64;

pub(crate) fn run(path: &Path) -> io::Result<i32> {
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(path)?).map_err(io::Error::other)?;
    validate_endpoints(&manifest.endpoints)?;
    let mut command = workload_command(&manifest.command)?;
    configure_loopback(&manifest.endpoints)?;
    let listeners = manifest
        .endpoints
        .iter()
        .map(std::net::TcpListener::bind)
        .collect::<io::Result<Vec<_>>>()?;
    for listener in &listeners {
        listener.set_nonblocking(true)?;
    }
    drop_capabilities()?;
    crate::helper_confine(&manifest, &mut command).map_err(io::Error::other)?;
    die_with_supervisor(&mut command);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(supervise(command, listeners, &manifest.socket))
}

fn validate_endpoints(endpoints: &[SocketAddr]) -> io::Result<()> {
    let mut unique = BTreeSet::new();
    for endpoint in endpoints {
        if endpoint.port() == 0
            || endpoint.ip().is_unspecified()
            || endpoint.ip().is_multicast()
            || endpoint.ip() == IpAddr::V4(std::net::Ipv4Addr::BROADCAST)
            || matches!(endpoint, SocketAddr::V6(address) if address.scope_id() != 0)
            || !unique.insert(*endpoint)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid relay endpoint",
            ));
        }
    }
    if endpoints.len() > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many relay endpoints",
        ));
    }
    Ok(())
}

fn workload_command(arguments: &[Vec<u8>]) -> io::Result<std::process::Command> {
    let Some((program, arguments)) = arguments.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing workload command",
        ));
    };
    if program.is_empty() || program.contains(&0) || arguments.iter().any(|arg| arg.contains(&0)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid workload command",
        ));
    }
    let mut command = std::process::Command::new(OsString::from_vec(program.clone()));
    command.args(arguments.iter().cloned().map(OsString::from_vec));
    Ok(command)
}

async fn supervise(
    command: std::process::Command,
    listeners: Vec<std::net::TcpListener>,
    socket: &Path,
) -> io::Result<i32> {
    let listeners = listeners
        .into_iter()
        .map(TcpListener::from_std)
        .collect::<io::Result<Vec<_>>>()?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let mut child = command.spawn()?;
    let mut forwarders = JoinSet::new();
    let mut next_listener = 0;
    loop {
        tokio::select! {
            _ = terminate.recv() => signal_child(&child, libc::SIGTERM)?,
            _ = interrupt.recv() => signal_child(&child, libc::SIGINT)?,
            status = child.wait() => {
                let status = status?;
                return Ok(status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(1)));
            }
            result = forwarders.join_next(), if !forwarders.is_empty() => {
                if let Some(Err(error)) = result {
                    return Err(io::Error::other(error));
                }
            }
            accepted = std::future::poll_fn(|cx| {
                for offset in 0..listeners.len() {
                    let index = (next_listener + offset) % listeners.len();
                    match listeners[index].poll_accept(cx) {
                        std::task::Poll::Ready(accepted) => {
                            next_listener = (index + 1) % listeners.len();
                            return std::task::Poll::Ready(accepted.map(|(stream, _)| (index, stream)));
                        }
                        std::task::Poll::Pending => {}
                    }
                }
                std::task::Poll::Pending
            }), if forwarders.len() < MAX_FORWARDERS => {
                let (index, stream) = accepted?;
                let socket = socket.to_owned();
                forwarders.spawn(async move {
                    let _ = forward(stream, &socket, index as u32).await;
                });
            }
        }
    }
}

fn die_with_supervisor(command: &mut std::process::Command) {
    let supervisor = std::process::id() as libc::pid_t;
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != supervisor {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

fn signal_child(child: &tokio::process::Child, signal: libc::c_int) -> io::Result<()> {
    if let Some(pid) = child.id()
        && unsafe { libc::kill(pid as libc::pid_t, signal) } < 0
    {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}

async fn forward(mut stream: TcpStream, socket: &Path, index: u32) -> io::Result<()> {
    let mut relay = UnixStream::connect(socket).await?;
    relay.write_all(&index.to_be_bytes()).await?;
    tokio::io::copy_bidirectional(&mut stream, &mut relay).await?;
    Ok(())
}

fn configure_loopback(endpoints: &[SocketAddr]) -> io::Result<()> {
    let index = unsafe { libc::if_nametoindex(c"lo".as_ptr()) };
    if index == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut netlink = Netlink::new()?;
    let mut link = vec![libc::AF_UNSPEC as u8, 0, 0, 0];
    link.extend_from_slice(&index.to_ne_bytes());
    link.extend_from_slice(&(libc::IFF_UP as u32).to_ne_bytes());
    link.extend_from_slice(&(libc::IFF_UP as u32).to_ne_bytes());
    netlink.request(libc::RTM_NEWLINK, 0, &link)?;
    let addresses: BTreeSet<_> = endpoints.iter().map(SocketAddr::ip).collect();
    for address in addresses {
        let payload = address_payload(index, address);
        match netlink.request(
            libc::RTM_NEWADDR,
            libc::NLM_F_CREATE | libc::NLM_F_EXCL,
            &payload,
        ) {
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {}
            result => result?,
        }
    }
    Ok(())
}

fn address_payload(index: u32, address: IpAddr) -> Vec<u8> {
    let (family, prefix, octets) = match address {
        IpAddr::V4(ip) => (libc::AF_INET, 32, ip.octets().to_vec()),
        IpAddr::V6(ip) => (libc::AF_INET6, 128, ip.octets().to_vec()),
    };
    let mut payload = vec![
        family as u8,
        prefix,
        libc::IFA_F_NODAD as u8,
        libc::RT_SCOPE_HOST,
    ];
    payload.extend_from_slice(&index.to_ne_bytes());
    for kind in [libc::IFA_LOCAL, libc::IFA_ADDRESS] {
        let length = (4 + octets.len()) as u16;
        payload.extend_from_slice(&length.to_ne_bytes());
        payload.extend_from_slice(&kind.to_ne_bytes());
        payload.extend_from_slice(&octets);
    }
    payload
}

struct Netlink {
    socket: std::fs::File,
    sequence: u32,
}

impl Netlink {
    fn new() -> io::Result<Self> {
        let raw = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::NETLINK_ROUTE,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let socket = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        address.nl_family = libc::AF_NETLINK as u16;
        let result = unsafe {
            libc::connect(
                socket.as_raw_fd(),
                (&address as *const libc::sockaddr_nl).cast(),
                std::mem::size_of_val(&address) as libc::socklen_t,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            socket: socket.into(),
            sequence: 0,
        })
    }

    fn request(&mut self, kind: u16, flags: i32, payload: &[u8]) -> io::Result<()> {
        self.sequence += 1;
        let mut message = Vec::with_capacity(16 + payload.len());
        message.extend_from_slice(&((16 + payload.len()) as u32).to_ne_bytes());
        message.extend_from_slice(&kind.to_ne_bytes());
        message.extend_from_slice(
            &((libc::NLM_F_REQUEST | libc::NLM_F_ACK | flags) as u16).to_ne_bytes(),
        );
        message.extend_from_slice(&self.sequence.to_ne_bytes());
        message.extend_from_slice(&0u32.to_ne_bytes());
        message.extend_from_slice(payload);
        self.socket.write_all(&message)?;
        let mut response = [0u8; 4096];
        loop {
            let received = self.socket.read(&mut response)?;
            if received < 20 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "truncated netlink acknowledgment",
                ));
            }
            let length =
                u32::from_ne_bytes(response[0..4].try_into().map_err(io::Error::other)?) as usize;
            let kind = u16::from_ne_bytes(response[4..6].try_into().map_err(io::Error::other)?);
            let sequence =
                u32::from_ne_bytes(response[8..12].try_into().map_err(io::Error::other)?);
            if sequence != self.sequence {
                continue;
            }
            if length < 20 || length > received || kind != libc::NLMSG_ERROR as u16 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid netlink acknowledgment",
                ));
            }
            let error = i32::from_ne_bytes(response[16..20].try_into().map_err(io::Error::other)?);
            return if error == 0 {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(-error))
            };
        }
    }
}

#[repr(C)]
struct CapabilityHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CapabilityData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

fn drop_capabilities() -> io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    for capability in 0..64 {
        let present = unsafe { libc::prctl(libc::PR_CAPBSET_READ, capability, 0, 0, 0) };
        if present < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINVAL) {
                break;
            }
            return Err(error);
        }
        if present != 0 && unsafe { libc::prctl(libc::PR_CAPBSET_DROP, capability, 0, 0, 0) } < 0 {
            let error = io::Error::last_os_error();
            return Err(error);
        }
    }
    let header = CapabilityHeader {
        version: 0x2008_0522,
        pid: 0,
    };
    let data = [CapabilityData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    if unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn endpoint_validation_rejects_wildcards_and_duplicates() {
        for invalid in [
            "0.0.0.0:443",
            "[::]:443",
            "127.0.0.1:0",
            "224.0.0.1:443",
            "255.255.255.255:443",
            "[fe80::1%1]:443",
        ] {
            assert!(validate_endpoints(&[invalid.parse().unwrap()]).is_err());
        }
        let endpoint = "127.0.0.1:443".parse().unwrap();
        assert!(validate_endpoints(&[endpoint, endpoint]).is_err());
        assert!(validate_endpoints(&[endpoint, "[::1]:443".parse().unwrap()]).is_ok());
    }

    #[test]
    fn address_encoding_uses_exact_local_host_routes() {
        for address in ["192.0.2.4", "2001:db8::4"] {
            let address: IpAddr = address.parse().unwrap();
            let payload = address_payload(9, address);
            let prefix = if address.is_ipv4() { 32 } else { 128 };
            assert_eq!(payload[1], prefix);
            assert_eq!(payload[2], libc::IFA_F_NODAD as u8);
            assert_eq!(&payload[4..8], &9u32.to_ne_bytes());
            assert_eq!(payload.len(), if address.is_ipv4() { 24 } else { 48 });
        }
    }

    #[test]
    fn workload_arguments_preserve_non_utf8() {
        let command = workload_command(&[b"/bin/echo".to_vec(), vec![0xff]]).unwrap();
        assert_eq!(
            command.get_args().next().unwrap(),
            OsString::from_vec(vec![0xff])
        );
        assert!(workload_command(&[]).is_err());
        assert!(workload_command(&[b"echo".to_vec(), vec![0]]).is_err());
    }

    #[test]
    fn supervisor_exit_probe() {
        if std::env::var_os("MUSH_HELPER_EXIT_PROBE").is_some() {
            std::process::exit(23);
        }
    }

    #[tokio::test]
    async fn supervisor_returns_workload_exit_with_no_endpoints() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "net_helper::tests::supervisor_exit_probe"]);
        command.env("MUSH_HELPER_EXIT_PROBE", "1");
        command.stdout(std::process::Stdio::null());
        let status = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            supervise(command, Vec::new(), Path::new("/unused")),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(status, 23);
    }

    #[tokio::test]
    async fn forward_sends_endpoint_index_and_preserves_half_closes() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("relay");
        let relay_listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut client = TcpStream::connect(address).await.unwrap();
            client.write_all(b"request").await.unwrap();
            client.shutdown().await.unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            assert_eq!(response, b"response");
        });
        let relay = tokio::spawn(async move {
            let (mut relay, _) = relay_listener.accept().await.unwrap();
            let mut index = [0; 4];
            relay.read_exact(&mut index).await.unwrap();
            assert_eq!(u32::from_be_bytes(index), 0x0102_0304);
            let mut request = Vec::new();
            relay.read_to_end(&mut request).await.unwrap();
            assert_eq!(request, b"request");
            relay.write_all(b"response").await.unwrap();
            relay.shutdown().await.unwrap();
        });
        let (stream, _) = listener.accept().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            forward(stream, &socket, 0x0102_0304).await.unwrap();
            client.await.unwrap();
            relay.await.unwrap();
        })
        .await
        .unwrap();
    }
}
