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
