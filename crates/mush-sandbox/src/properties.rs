use super::*;
use proptest::prelude::*;
use proptest::test_runner::{Config as TestConfig, RngSeed};

fn settings() -> TestConfig {
    TestConfig {
        cases: 256,
        rng_seed: RngSeed::Fixed(0x5a11db0),
        failure_persistence: None,
        ..TestConfig::default()
    }
}

fn tokens() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        Just("../".into()),
        Just("//".into()),
        Just("界é\u{301}".into()),
        ".{0,128}",
        "[a-zA-Z0-9_.:-]{0,260}"
    ]
}

proptest! {
    #![proptest_config(settings())]
    #[test]
    fn config_parsing_is_deterministic(text in tokens(), key in "[a-z]{1,24}", nested in 0u8..4) {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), &text).unwrap();
        let first = Config::load(file.path());
        let second = Config::load(file.path());
        prop_assert_eq!(format!("{first:?}"), format!("{second:?}"));
        let unknown = format!("unknown_{key} = []");
        let prefix = match nested { 0 => "", 1 => "[profile.test]\n", 2 => "[project.\"/tmp\"]\n", _ => "[project.\"/tmp\".profile.test]\n" };
        std::fs::write(file.path(), format!("{prefix}{unknown}")).unwrap();
        prop_assert!(Config::load(file.path()).is_err());
    }

    #[test]
    fn project_resolution_uses_longest_component_prefix(name in "[a-z]{1,20}", web in any::<bool>(), trailing in any::<bool>(), extra in 0usize..5) {
        let mut config = Config::default();
        config.profile.insert(name.clone(), Grants { read: vec!["/shared".into()], websearch: Some(!web), ..Grants::default() });
        config.project.insert("/repo".into(), Project { default: Some(name.clone()), profile: BTreeMap::from([(name.clone(), Grants { write: vec!["/parent".into()], ..Grants::default() })]) });
        config.project.insert("/repo/界".into(), Project { default: Some(name.clone()), profile: BTreeMap::from([(name.clone(), Grants { write: vec!["/child".into()], websearch: Some(web), ..Grants::default() })]) });
        let repo = PathBuf::from(format!("/repo/界{}{}", "/nested".repeat(extra), if trailing { "/" } else { "" }));
        let expected = Profile { read: vec!["/shared".into()], write: vec!["/child".into()], websearch: web, ..Profile::default() };
        prop_assert_eq!(config.resolve(&repo, None).unwrap(), expected.clone());
        prop_assert_eq!(config.resolve(&repo, Some(&name)).unwrap(), expected);
        prop_assert_eq!(config.resolve(Path::new("/repository/界"), None).unwrap(), Profile::default());
    }

    #[test]
    fn accepted_descendants_never_widen(bits in prop::collection::vec(any::<u8>(), 1..32), host in tokens()) {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let nested = root.path().join("界");
        std::fs::create_dir(&nested).unwrap();
        let secret = outside.path().join("secret");
        std::fs::write(&secret, "secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let parent = Profile { read: vec![nested.clone()], write: vec![root.path().to_path_buf()], secrets: vec![secret.clone()], websearch: bits[0] & 1 != 0, network: vec!["example.com:443".into(), "[::1]:65535".into()], allowed_host_requests: vec!["attach_image".into(), host], };
        let parent = parent.canonicalized().unwrap();
        let mut current = parent.clone();
        for byte in bits {
            let candidate = match byte % 8 {
                0 => nested.clone(), 1 => root.path().to_path_buf(), 2 => outside.path().to_path_buf(),
                3 => nested.join(".."), 4 => PathBuf::from(format!("{}//界/", root.path().display())),
                5 => root.path().join("link"), 6 => PathBuf::from("relative"), _ => secret.clone(),
            };
            let narrow = Narrow {
                read: (byte & 1 != 0).then(|| vec![candidate.clone()]),
                write: (byte & 2 != 0).then(|| vec![candidate.clone()]),
                secrets: (byte & 4 != 0).then(|| vec![candidate]),
                websearch: (byte & 8 != 0).then_some(byte & 16 != 0),
                network: (byte & 32 != 0).then(|| if byte & 64 != 0 { vec!["[::1]:65535".into()] } else { vec![] }),
                allowed_host_requests: (byte & 128 != 0).then(|| vec!["attach_image".into()]),
            };
            if let Ok(child) = current.narrow(&narrow) {
                for path in child.read() { prop_assert!(current.read.iter().chain(&current.write).any(|p| path.starts_with(p))); }
                for path in child.write() { prop_assert!(current.write.iter().any(|p| path.starts_with(p))); }
                for path in child.secrets() { prop_assert!(current.secrets.contains(path)); }
                prop_assert!(!child.websearch || current.websearch);
                prop_assert!(child.network.iter().all(|s| current.network.contains(s)));
                prop_assert!(child.allowed_host_requests.iter().all(|s| current.allowed_host_requests.contains(s)));
                prop_assert_eq!(child.narrow(&Narrow::default()).unwrap(), child.clone());
                current = child;
            }
        }
    }
}

proptest! {
    #![proptest_config(settings())]
    #[test]
    fn valid_config_validation_and_request_policy(name in "[a-z]{1,16}", mode in 0u8..8, web in any::<bool>(), kind in tokens()) {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = if mode == 1 { "relative" } else { "/read" };
        let network = if mode == 2 { "[example.com]:443" } else { "[::1]:65535" };
        let project = if mode == 3 { "relative" } else { "/repo" };
        let default = if mode == 4 { "missing" } else { &name };
        let text = format!("[profile.{name}]\nread = [\"{path}\"]\nwrite = [\"/write\"]\nsecrets = [\"/secret\"]\nnetwork = [\"{network}\"]\nwebsearch = {web}\nallowed_host_requests = [\"attach_image\"]\n[project.\"{project}\"]\ndefault = \"{default}\"\n[project.\"{project}\".profile.{name}]\nread = [\"/extra\"]\n");
        std::fs::write(file.path(), text).unwrap();
        let config = Config::load(file.path());
        if (1..=4).contains(&mode) { prop_assert!(config.is_err()); }
        else {
            let config = config.unwrap().unwrap();
            let resolved = config.resolve(Path::new("/repo/child"), None).unwrap();
            prop_assert_eq!(resolved.read(), &[PathBuf::from("/read"), PathBuf::from("/extra")]);
            prop_assert!(config.resolve(Path::new("relative"), None).is_err());
            prop_assert!(config.resolve(Path::new("/repo"), Some("missing")).is_err());
            let builtin = matches!(kind.as_str(), "edit" | "bash.start" | "bash.wait" | "bash.poll" | "bash.running" | "bash.cancel" | "bash.kill");
            prop_assert_eq!(resolved.allows_host_request(&kind), if kind == "websearch" { web } else { builtin || kind == "attach_image" });
            for request in ["edit", "bash.start", "bash.wait", "bash.poll", "bash.running", "bash.cancel", "bash.kill", "attach_image"] { prop_assert!(resolved.allows_host_request(request)); }
            prop_assert_eq!(resolved.allows_host_request("websearch"), web);
        }
        prop_assert!(Config::load(&file.path().join("child")).is_err());
        let absent = file.path().with_extension("missing");
        prop_assert!(Config::load(&absent).unwrap().is_none());
    }
}
