#![no_main]
use libfuzzer_sys::fuzz_target;
use mush_sandbox::Config;
use std::path::Path;

fuzz_target!(|data: &[u8]| {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), data).unwrap();
    let first = Config::load(file.path());
    let second = Config::load(file.path());
    assert_eq!(format!("{first:?}"), format!("{second:?}"));
    if let Ok(Some(config)) = first {
        for repo in ["/", "/tmp", "/tmp/界", "/tmp//nested/", "/tmp/../", "relative"] {
            for name in [None, Some("test"), Some("")] {
                assert_eq!(format!("{:?}", config.resolve(Path::new(repo), name)), format!("{:?}", config.resolve(Path::new(repo), name)));
            }
        }
    }
    if let Some(byte) = data.first() {
        let text = format!("[profile.test]\nread = [\"/tmp\"]\nwrite = [\"/tmp/界\"]\nwebsearch = {}\n[project.\"/tmp\"]\ndefault = \"test\"\n[project.\"/tmp/界\".profile.test]\nnetwork = [\"[::1]:65535\"]\n", byte & 1 != 0);
        std::fs::write(file.path(), &text).unwrap();
        let config = Config::load(file.path()).unwrap().unwrap();
        assert_eq!(config.resolve(Path::new("/tmp/界/sub"), None).unwrap(), config.resolve(Path::new("/tmp/界/sub"), Some("test")).unwrap());
        assert!(config.resolve(Path::new("/tmp/界"), Some("unknown")).is_err());
    }
    if let Ok(text) = std::str::from_utf8(data) {
        let text = format!("[profile.test]\nnetwork = [{}]", serde_json::to_string(&text).unwrap());
        std::fs::write(file.path(), &text).unwrap();
        let _ = Config::load(file.path());
        for key in ["read", "write", "secrets", "allowed_host_requests"] {
            let text = format!("[profile.test]\n{key} = [{}]", serde_json::to_string(&text).unwrap());
            std::fs::write(file.path(), &text).unwrap();
            let _ = Config::load(file.path());
        }
    }
});
