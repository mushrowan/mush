#![cfg(target_os = "linux")]

#[cfg(test)]
mod tests {
    use mush_sandbox::{Config, Sandbox};
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn echo_server(address: &str) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(address).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut message = [0; 4];
                stream.read_exact(&mut message).await.unwrap();
                stream.write_all(&message).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        (address, task)
    }

    fn install_helper() {
        let executable = std::env::current_exe().unwrap();
        let helper = executable.parent().unwrap().join("mush-sandbox-net");
        match std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_mush-sandbox-net"), helper) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => panic!("installing network test helper: {error}"),
        }
    }

    async fn run_probe(probe: &str) {
        install_helper();
        let workspace = tempfile::tempdir().unwrap();
        let executable = std::env::current_exe().unwrap();
        let (hostname, hostname_server) = echo_server("[::]:0").await;
        let (ipv4, ipv4_server) = echo_server("127.0.0.1:0").await;
        let (ipv6, ipv6_server) = echo_server("[::1]:0").await;
        let (blocked_port, blocked_port_server) = echo_server("127.0.0.1:0").await;
        let (blocked_host, blocked_host_server) = echo_server("127.0.0.2:0").await;
        for address in [blocked_port, blocked_host] {
            let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
            stream.write_all(b"ping").await.unwrap();
            let mut response = [0; 4];
            stream.read_exact(&mut response).await.unwrap();
            assert_eq!(&response, b"ping");
        }
        let config: Config = toml::from_str(&format!(
            "[profile.networked]\nnetwork = [\"localhost:{}\", \"{ipv4}\", \"{ipv6}\"]\n",
            hostname.port()
        ))
        .unwrap();
        let profile = config.resolve(workspace.path(), Some("networked")).unwrap();
        let sandbox = Sandbox::new(
            profile,
            workspace.path(),
            std::slice::from_ref(&executable),
            &[],
        )
        .unwrap();
        let mut command = tokio::process::Command::new(&executable);
        command.args(["--exact", &format!("tests::{probe}"), "--nocapture"]);
        let mut command = sandbox.prepare_command(command).unwrap();
        command.env("MUSH_NETWORK_PROBE", "1");
        command.env(
            "MUSH_NETWORK_HOSTNAME",
            format!("localhost:{}", hostname.port()),
        );
        command.env("MUSH_NETWORK_IPV4", ipv4.to_string());
        command.env("MUSH_NETWORK_IPV6", ipv6.to_string());
        command.env("MUSH_NETWORK_BLOCKED_PORT", blocked_port.to_string());
        command.env("MUSH_NETWORK_BLOCKED_HOST", blocked_host.to_string());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        let output = tokio::time::timeout(Duration::from_secs(20), command.output())
            .await
            .unwrap()
            .unwrap();
        for server in [
            hostname_server,
            ipv4_server,
            ipv6_server,
            blocked_port_server,
            blocked_host_server,
        ] {
            server.abort();
        }
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn echo(address: impl ToSocketAddrs) {
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(b"ping").unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut message = Vec::new();
        stream.read_to_end(&mut message).unwrap();
        assert_eq!(message, b"ping");
    }

    #[test]
    fn allowed_tcp_probe() {
        if std::env::var_os("MUSH_NETWORK_PROBE").is_none() {
            return;
        }
        for variable in [
            "MUSH_NETWORK_HOSTNAME",
            "MUSH_NETWORK_IPV4",
            "MUSH_NETWORK_IPV6",
        ] {
            echo(std::env::var(variable).unwrap());
        }
    }

    #[test]
    fn resolver_probe() {
        if std::env::var_os("MUSH_NETWORK_PROBE").is_none() {
            return;
        }
        assert!("not-granted.invalid:443".to_socket_addrs().is_err());
        assert_eq!(std::fs::read_to_string("/etc/resolv.conf").unwrap(), "");
        assert_eq!(
            std::fs::read_to_string("/etc/nsswitch.conf").unwrap(),
            "hosts: files\n"
        );
        let hosts = std::fs::read_to_string("/etc/hosts").unwrap();
        assert!(hosts.contains("localhost"));
        assert!(!hosts.contains("not-granted"));
    }

    #[test]
    fn blocked_live_endpoints_probe() {
        if std::env::var_os("MUSH_NETWORK_PROBE").is_none() {
            return;
        }
        for variable in ["MUSH_NETWORK_BLOCKED_PORT", "MUSH_NETWORK_BLOCKED_HOST"] {
            let blocked = std::env::var(variable).unwrap();
            assert!(
                TcpStream::connect_timeout(&blocked.parse().unwrap(), Duration::from_millis(200))
                    .is_err(),
                "reachable disallowed endpoint {blocked}"
            );
        }
    }

    #[test]
    fn denied_socket_probe() {
        if std::env::var_os("MUSH_NETWORK_PROBE").is_none() {
            return;
        }
        for address in ["0.0.0.0:0", "[::]:0"] {
            let error = UdpSocket::bind(address).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EPERM));
        }
        for (family, protocol) in [
            (libc::AF_INET, libc::IPPROTO_ICMP),
            (libc::AF_INET6, libc::IPPROTO_ICMPV6),
        ] {
            assert_eq!(
                unsafe { libc::socket(family, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, protocol) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            );
        }
        for family in [libc::AF_NETLINK, libc::AF_PACKET] {
            assert_eq!(
                unsafe { libc::socket(family, libc::SOCK_RAW | libc::SOCK_CLOEXEC, 0) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            );
        }
        for family in [libc::AF_INET, libc::AF_INET6] {
            assert_eq!(
                unsafe {
                    libc::socket(
                        family,
                        libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                        libc::IPPROTO_TCP,
                    )
                },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            );
        }
    }

    #[test]
    fn confinement_probe() {
        if std::env::var_os("MUSH_NETWORK_PROBE").is_none() {
            return;
        }
        assert_eq!(
            unsafe { libc::unshare(libc::CLONE_NEWNET | libc::CLONE_NEWUSER) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
        assert_eq!(
            unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) },
            1
        );
        for capability in [12, 8, 10] {
            assert_eq!(
                unsafe { libc::prctl(libc::PR_CAPBSET_READ, capability, 0, 0, 0) },
                0
            );
        }
    }

    #[tokio::test]
    #[cfg_attr(
        not(mush_network_supported),
        ignore = "requires bubblewrap user namespaces and Landlock ABI 9"
    )]
    async fn ordinary_tcp_clients_use_allowed_hostname_ipv4_ipv6() {
        run_probe("allowed_tcp_probe").await;
    }

    #[tokio::test]
    #[cfg_attr(
        not(mush_network_supported),
        ignore = "requires bubblewrap user namespaces and Landlock ABI 9"
    )]
    async fn unlisted_live_host_and_port_are_blocked() {
        run_probe("blocked_live_endpoints_probe").await;
    }

    #[tokio::test]
    #[cfg_attr(
        not(mush_network_supported),
        ignore = "requires bubblewrap user namespaces and Landlock ABI 9"
    )]
    async fn dns_resolution_is_grant_scoped() {
        run_probe("resolver_probe").await;
    }

    #[tokio::test]
    #[cfg_attr(
        not(mush_network_supported),
        ignore = "requires bubblewrap user namespaces and Landlock ABI 9"
    )]
    async fn udp_raw_and_netlink_are_denied() {
        run_probe("denied_socket_probe").await;
    }

    #[tokio::test]
    #[cfg_attr(
        not(mush_network_supported),
        ignore = "requires bubblewrap user namespaces and Landlock ABI 9"
    )]
    async fn workload_cannot_regain_network_authority() {
        run_probe("confinement_probe").await;
    }

    #[tokio::test]
    #[cfg_attr(
        not(mush_network_supported),
        ignore = "requires bubblewrap user namespaces and Landlock ABI 9"
    )]
    async fn broad_tmp_write_cannot_cover_trusted_network_state() {
        install_helper();
        let workspace = tempfile::tempdir().unwrap();
        let config: Config = toml::from_str(
            "[profile.networked]\nnetwork = [\"127.0.0.1:54321\"]\nwrite = [\"/tmp\"]\n",
        )
        .unwrap();
        let profile = config.resolve(workspace.path(), Some("networked")).unwrap();
        let error = Sandbox::new(profile, workspace.path(), &[], &[]).unwrap_err();
        assert!(matches!(error, mush_sandbox::Error::WritableOverlap { .. }));
    }

    #[tokio::test]
    #[cfg_attr(
        not(mush_network_supported),
        ignore = "requires bubblewrap user namespaces and Landlock ABI 9"
    )]
    async fn extra_paths_cannot_cover_trusted_network_state() {
        install_helper();
        let workspace = tempfile::tempdir().unwrap();
        let config: Config =
            toml::from_str("[profile.networked]\nnetwork = [\"127.0.0.1:54321\"]\n").unwrap();
        let profile = config.resolve(workspace.path(), Some("networked")).unwrap();
        let sandbox = Sandbox::new(profile, workspace.path(), &[], &[]).unwrap();
        let error = sandbox.with_paths(&[], &["/tmp".into()]).unwrap_err();
        assert!(matches!(error, mush_sandbox::Error::WritableOverlap { .. }));
    }

    fn executable(name: &str) -> std::path::PathBuf {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
            .map(|directory| directory.join(name))
            .find(|path| path.is_file())
            .unwrap_or_else(|| panic!("test executable not found: {name}"))
            .canonicalize()
            .unwrap()
    }

    #[tokio::test]
    #[cfg_attr(
        not(mush_network_supported),
        ignore = "requires bubblewrap user namespaces and Landlock ABI 9"
    )]
    async fn ordinary_tls_client_verifies_ca_and_hostname_without_proxy_settings() {
        use tokio::io::AsyncBufReadExt;
        install_helper();
        let workspace = tempfile::tempdir().unwrap();
        let certificate = workspace.path().join("localhost.pem");
        let key = workspace.path().join("localhost.key");
        let generated = tokio::process::Command::new(executable("openssl"))
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-noenc",
                "-days",
                "1",
                "-subj",
                "/CN=localhost",
                "-addext",
                "subjectAltName=DNS:localhost",
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&certificate)
            .output()
            .await
            .unwrap();
        assert!(
            generated.status.success(),
            "{}",
            String::from_utf8_lossy(&generated.stderr)
        );
        let python = executable("python3");
        let mut server = tokio::process::Command::new(&python)
            .args([
                "-I",
                "-c",
                r#"
import socket, ssl, sys
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(sys.argv[1], sys.argv[2])
with socket.socket(socket.AF_INET6, socket.SOCK_STREAM) as listener:
    listener.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0)
    listener.bind(('::', 0))
    listener.listen(4)
    listener.settimeout(15)
    print(listener.getsockname()[1], flush=True)
    for _ in range(3):
        connection, _ = listener.accept()
        try:
            with context.wrap_socket(connection, server_side=True) as stream:
                stream.sendall(stream.recv(4))
        except ssl.SSLError:
            connection.close()
"#,
            ])
            .arg(&certificate)
            .arg(&key)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut readiness = String::new();
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio::io::BufReader::new(server.stdout.take().unwrap()).read_line(&mut readiness),
        )
        .await
        .unwrap()
        .unwrap();
        let port = readiness.trim().parse::<u16>().unwrap();
        let config: Config = toml::from_str(&format!(
            "[profile.tls]\nnetwork = [\"localhost:{port}\"]\n"
        ))
        .unwrap();
        let profile = config.resolve(workspace.path(), Some("tls")).unwrap();
        let sandbox = Sandbox::new(profile, workspace.path(), &[], &[]).unwrap();
        let mut command = tokio::process::Command::new(&python);
        command
            .args([
                "-I",
                "-c",
                r#"
import os, socket, ssl, sys
assert not any(name.lower().endswith('_proxy') for name in os.environ)
context = ssl.create_default_context(cafile=sys.argv[1])
assert context.verify_mode == ssl.CERT_REQUIRED and context.check_hostname
port = int(sys.argv[2])
for settings, hostname in [(context, 'wrong.invalid'), (ssl.create_default_context(), 'localhost')]:
    try:
        with socket.create_connection(('localhost', port), timeout=5) as raw:
            with settings.wrap_socket(raw, server_hostname=hostname):
                raise AssertionError('unverified TLS peer accepted')
    except ssl.SSLCertVerificationError:
        pass
with socket.create_connection(('localhost', port), timeout=5) as raw:
    with context.wrap_socket(raw, server_hostname='localhost') as stream:
        stream.sendall(b'ping')
        assert stream.recv(4) == b'ping'
"#,
            ])
            .arg(&certificate)
            .arg(port.to_string());
        let mut command = sandbox.prepare_command(command).unwrap();
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        command.kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(20), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let output = tokio::time::timeout(Duration::from_secs(10), server.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
