#![no_main]
use libfuzzer_sys::fuzz_target;
use mush_sandbox::{Narrow, Profile};

fn subset(child: &Profile, base: &Profile) {
    assert!(child.read().iter().all(|p| base.read().iter().chain(base.write()).any(|b| p.starts_with(b))));
    assert!(child.write().iter().all(|p| base.write().iter().any(|b| p.starts_with(b))));
    assert!(child.secrets().iter().all(|p| base.secrets().contains(p)));
    assert!(!child.websearch() || base.websearch());
    assert!(child.network().iter().all(|p| base.network().contains(p)));
    assert!(child.allowed_host_requests().iter().all(|p| base.allowed_host_requests().contains(p)));
}

fuzz_target!(|data: &[u8]| {
    let root = tempfile::tempdir().unwrap();
    let child = root.path().join("界");
    std::fs::create_dir(&child).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret");
    std::fs::write(&secret, "secret").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
    let base: Profile = serde_json::from_value(serde_json::json!({
        "read": [child], "write": [root.path()], "secrets": [secret], "websearch": true,
        "network": ["example.com:443", "[::1]:65535"], "allowed_host_requests": ["attach_image"]
    })).unwrap();
    let mut current = base.canonicalized().unwrap();
    if let Ok(narrow) = serde_json::from_slice::<Narrow>(data) {
        if let Ok(next) = current.narrow(&narrow) { subset(&next, &current); current = next; }
    }
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(narrow) = toml::from_str::<Narrow>(text) {
            if let Ok(next) = current.narrow(&narrow) { subset(&next, &current); current = next; }
        }
    }
    for byte in data.iter().take(64) {
        let path = match byte % 8 {
            0 => child.clone(), 1 => root.path().to_path_buf(), 2 => outside.path().to_path_buf(),
            3 => child.join(".."), 4 => root.path().join("link"), 5 => secret.clone(),
            6 => "relative".into(), _ => format!("{}//界/", root.path().display()).into(),
        };
        let narrow = Narrow {
            read: (byte & 1 != 0).then(|| vec![path.clone()]), write: (byte & 2 != 0).then(|| vec![path.clone()]),
            secrets: (byte & 4 != 0).then(|| vec![path]), websearch: (byte & 8 != 0).then_some(byte & 16 != 0),
            network: (byte & 32 != 0).then(|| if byte & 64 != 0 { vec!["[::1]:65535".into()] } else { vec![] }),
            allowed_host_requests: (byte & 128 != 0).then(|| vec!["attach_image".into()]),
        };
        if let Ok(next) = current.narrow(&narrow) { subset(&next, &current); current = next; }
    }
});
