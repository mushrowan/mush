use super::*;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

#[test]
fn endpoint_syntax_requires_explicit_nonzero_port() {
    for invalid in [
        "example.com",
        "example.com:0",
        "http://example.com:80",
        "a b:80",
        "[::1]",
        ":80",
    ] {
        assert!(network::validate_entry(invalid).is_err(), "{invalid}");
    }
    for valid in ["db.example:5432", "127.0.0.1:5432", "[::1]:5432"] {
        network::validate_entry(valid).unwrap();
    }
}

#[test]
fn relay_refuses_unlisted_destination_indices() {
    assert_eq!(
        network::endpoint_at(&[SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5432)], 0)
            .unwrap()
            .port(),
        5432
    );
    assert!(network::endpoint_at(&[], 0).is_err());
    assert!(network::endpoint_at(&["127.0.0.1:5432".parse().unwrap()], 1).is_err());
}

#[test]
fn malformed_endpoint_grammar_is_rejected() {
    for entry in [
        "[example.com]:443",
        "[127.0.0.1]:443",
        "::1:443",
        ".:443",
        "-:443",
        "a..b:443",
        "-a:443",
        "a-:443",
    ] {
        assert!(network::validate_entry(entry).is_err(), "{entry}");
    }
}

use proptest::prelude::*;
use proptest::test_runner::{Config as TestConfig, RngSeed};

proptest! {
    #![proptest_config(TestConfig { cases: 512, rng_seed: RngSeed::Fixed(0x619dd0), failure_persistence: None, ..TestConfig::default() })]
    #[test]
    fn endpoint_indices_are_exact(endpoints in prop::collection::vec((any::<[u8; 4]>(), any::<u16>()), 0..64), index in any::<u32>()) {
        let endpoints: Vec<_> = endpoints.into_iter().map(|(ip, port)| SocketAddr::new(IpAddr::V4(ip.into()), port)).collect();
        prop_assert_eq!(network::endpoint_at(&endpoints, index).ok(), endpoints.get(index as usize).copied());
        for (index, expected) in endpoints.iter().enumerate() { prop_assert_eq!(network::endpoint_at(&endpoints, index as u32).unwrap(), *expected); }
        prop_assert!(network::endpoint_at(&endpoints, endpoints.len() as u32).is_err());
    }

    #[test]
    fn generated_hosts_contain_only_allowed_names(labels in prop::collection::vec("[a-zA-Z][a-zA-Z0-9]{0,30}", 1..5), addresses in prop::collection::vec(any::<[u8; 16]>(), 1..16), port in 1u16..=65535) {
        let name = labels.join(".");
        network::validate_entry(&format!("{name}:{port}")).unwrap();
        let mut hosts = String::new();
        for bytes in &addresses {
            let ip = IpAddr::V6((*bytes).into());
            network::append_host(&mut hosts, &name, ip);
            network::append_host(&mut hosts, &ip.to_string(), ip);
        }
        prop_assert_eq!(hosts.lines().count(), addresses.len());
        for (line, bytes) in hosts.lines().zip(&addresses) {
            let fields: Vec<_> = line.split_whitespace().collect();
            prop_assert_eq!(fields.len(), 2);
            prop_assert_eq!(fields[0].parse::<IpAddr>().unwrap(), IpAddr::V6((*bytes).into()));
            prop_assert_eq!(fields[1], name.as_str());
        }
    }

    #[test]
    fn valid_endpoints_and_invalid_combinations(label in "[a-zA-Z][a-zA-Z0-9-]{0,30}[a-zA-Z0-9]", v4 in any::<[u8; 4]>(), v6 in any::<[u8; 16]>(), port in 1u16..=65535, trailing in any::<bool>()) {
        let name = format!("{label}.example{}", if trailing { "." } else { "" });
        let v4 = std::net::Ipv4Addr::from(v4);
        let v6 = std::net::Ipv6Addr::from(v6);
        for entry in [format!("{name}:{port}"), format!("{v4}:{port}"), format!("[{v6}]:{port}")] {
            prop_assert!(network::validate_entry(&entry).is_ok(), "{}", entry);
        }
        for entry in [format!("[{name}]:{port}"), format!("{v6}:{port}"), format!("{name}:0"), format!("-{name}:65536"), format!("{label}..example:{port}")] {
            prop_assert!(network::validate_entry(&entry).is_err(), "{}", entry);
        }
    }

    #[test]
    fn combined_network_grammar_boundaries(host in ".{0,300}", port in any::<u32>(), bracket in any::<bool>()) {
        let entry = if bracket { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
        if network::validate_entry(&entry).is_ok() {
            prop_assert!((1..=65535).contains(&port));
            if bracket { prop_assert!(host.parse::<std::net::Ipv6Addr>().is_ok()); }
            else {
                prop_assert!(!host.contains(':'));
                if host.parse::<std::net::Ipv4Addr>().is_err() {
                    let host = host.strip_suffix('.').unwrap_or(&host);
                    prop_assert!(host.len() <= 253);
                    for label in host.split('.') {
                        prop_assert!((1..=63).contains(&label.len()));
                        prop_assert!(label.as_bytes()[0].is_ascii_alphanumeric());
                        prop_assert!(label.as_bytes()[label.len()-1].is_ascii_alphanumeric());
                        prop_assert!(label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
                    }
                }
            }
        }
    }
}
