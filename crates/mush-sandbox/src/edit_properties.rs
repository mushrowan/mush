use crate::{Error, Profile, Sandbox};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn config(cases: u32, seed: u64) -> Config {
    Config {
        cases,
        rng_seed: RngSeed::Fixed(seed),
        failure_persistence: None,
        ..Config::default()
    }
}

fn sandbox(root: &Path) -> Sandbox {
    Sandbox {
        profile: Profile::default(),
        workspace: root.to_path_buf(),
        temporary: Arc::new(tempfile::tempdir_in("/tmp").unwrap()),
        read: vec![],
        write: vec![root.to_path_buf()],
        secrets: vec![],
        protected_read: vec![],
        network: None,
    }
}

fn filename() -> impl Strategy<Value = OsString> {
    prop_oneof![
        proptest::collection::vec(1u8..=255, 1..40),
        proptest::collection::vec(1u8..=255, 240..256),
        Just(vec![b'x', 0xff, 0xfe]),
    ]
    .prop_map(|mut bytes| {
        for byte in &mut bytes {
            if *byte == b'/' || *byte == b'.' {
                *byte = b'_';
            }
        }
        OsString::from_vec(bytes)
    })
}

proptest! {
    #![proptest_config(config(256, 0x645d_700b))]

    #[test]
    fn trees_reject_escape_at_every_depth(
        names in proptest::collection::vec(filename(), 1..8),
        leaf in filename(),
        value in ".{0,128}",
    ) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("writable");
        let outside = temporary.path().join("writable-sibling");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        let broker = sandbox(&root);
        let marker = format!("before<{value}>after");
        let external = outside.join(&leaf);
        fs::write(&external, &marker).unwrap();
        let mut directory = root.clone();
        for (depth, name) in names.iter().enumerate() {
            let link = directory.join(format!("escape-{depth}"));
            symlink(&outside, &link).unwrap();
            prop_assert!(broker.edit_file(&link.join(&leaf), "before<", "changed<").is_err());
            fs::remove_file(&link).unwrap();
            let hardlink = directory.join(format!("hardlink-{depth}"));
            fs::hard_link(&external, &hardlink).unwrap();
            prop_assert!(matches!(broker.edit_file(&hardlink, "before<", "changed<"), Err(Error::EditFileType)));
            let next = directory.join(name);
            fs::create_dir(&next).unwrap();
            let decoy = directory.join(format!("decoy-{depth}"));
            fs::create_dir(&decoy).unwrap();
            fs::write(decoy.join("untouched"), &marker).unwrap();
            directory = next;
        }
        let file = directory.join(&leaf);
        fs::write(&file, &marker).unwrap();
        broker.edit_file(&file, &marker, "changed").unwrap();
        prop_assert_eq!(fs::read_to_string(&file).unwrap(), "changed");
        let link = directory.join("leaf-symlink");
        symlink(&external, &link).unwrap();
        prop_assert!(broker.edit_file(&link, "before<", "changed<").is_err());
        let hardlink = directory.join("leaf-hardlink");
        fs::hard_link(&external, &hardlink).unwrap();
        prop_assert!(matches!(broker.edit_file(&hardlink, "before<", "changed<"), Err(Error::EditFileType)));
        prop_assert!(broker.edit_file(&external, "before<", "changed<").is_err());
        prop_assert_eq!(fs::read_to_string(&external).unwrap(), marker);
        let traversal = directory.join("..").join("unreachable");
        prop_assert!(matches!(broker.edit_file(&traversal, "before<", "changed<"), Err(Error::EditTraversal(_))));
        prop_assert!(matches!(broker.edit_file(Path::new("relative"), "a", "b"), Err(Error::EditDenied(_))));
        let nul = directory.join(OsString::from_vec(vec![b'x', 0, b'y']));
        prop_assert!(broker.edit_file(&nul, "a", "b").is_err());
        prop_assert_eq!(fs::read_to_string(&hardlink).unwrap(), fs::read_to_string(&external).unwrap());
    }
}

proptest! {
    #![proptest_config(config(512, 0x740c_3c49))]

    #[test]
    fn text_replacement_matches_independent_model(
        prefix in ".{0,256}",
        old in ".{0,32}",
        new in ".{0,512}",
        copies in 0usize..5,
        grant_file in any::<bool>(),
    ) {
        let temporary = tempfile::tempdir().unwrap();
        let file = temporary.path().join("file");
        let text = format!("{prefix}{}", old.repeat(copies));
        fs::write(&file, &text).unwrap();
        let mut broker = sandbox(temporary.path());
        if grant_file {
            broker.write = vec![file.clone()];
        }
        let occurrences = text.match_indices(&old).count();
        let result = broker.edit_file(&file, &old, &new);
        if old.is_empty() {
            prop_assert!(matches!(result, Err(Error::EditEmpty)));
            prop_assert_eq!(fs::read_to_string(&file).unwrap(), text);
        } else if occurrences != 1 {
            prop_assert!(matches!(result, Err(Error::EditMatches(n)) if n == occurrences));
            prop_assert_eq!(fs::read_to_string(&file).unwrap(), text);
        } else {
            prop_assert!(result.is_ok());
            let offset = text.find(&old).unwrap();
            let expected = format!("{}{}{}", &text[..offset], new, &text[offset + old.len()..]);
            prop_assert_eq!(fs::read_to_string(&file).unwrap(), expected);
        }
    }
}

proptest! {
    #![proptest_config(config(64, 0x13a4_c33d))]

    #[test]
    fn concurrent_symlink_replacement_never_writes_outside(
        depth in 0usize..6,
        names in proptest::collection::vec(filename(), 6),
        swap_directory in any::<bool>(),
    ) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("root");
        let outside = temporary.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        let mut directory = root.clone();
        for name in names.iter().take(depth) {
            directory.push(name);
            fs::create_dir(&directory).unwrap();
        }
        let external = outside.join("file");
        fs::write(&external, "outside-a").unwrap();
        let entry = directory.join("entry");
        let parked = directory.join("parked");
        let path = if swap_directory {
            fs::create_dir(&entry).unwrap();
            fs::write(entry.join("file"), "inside-a").unwrap();
            entry.join("file")
        } else {
            fs::write(&entry, "inside-a").unwrap();
            entry.clone()
        };
        let broker = sandbox(&root);
        let stop = AtomicBool::new(false);
        let ready = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                ready.wait();
                while !stop.load(Ordering::Relaxed) {
                    fs::rename(&entry, &parked).unwrap();
                    if let Err(error) = symlink(if swap_directory { outside.as_path() } else { external.as_path() }, &entry) {
                        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
                    }
                    std::thread::yield_now();
                    fs::remove_file(&entry).unwrap();
                    fs::rename(&parked, &entry).unwrap();
                }
            });
            ready.wait();
            for _ in 0..128 {
                let _ = broker.edit_file(&path, "a", "b");
                let _ = broker.edit_file(&path, "b", "a");
            }
            stop.store(true, Ordering::Relaxed);
            worker.join().unwrap();
        });
        prop_assert_eq!(fs::read_to_string(&external).unwrap(), "outside-a");
    }
}

#[test]
fn special_files_and_invalid_utf8_are_unchanged() {
    let temporary = tempfile::tempdir().unwrap();
    let broker = sandbox(temporary.path());
    let file = temporary.path().join("bytes");
    fs::write(&file, [0xff, b'a']).unwrap();
    assert!(matches!(
        broker.edit_file(&file, "a", "b"),
        Err(Error::EditIo(_))
    ));
    assert_eq!(fs::read(&file).unwrap(), [0xff, b'a']);
    assert!(broker.edit_file(temporary.path(), "a", "b").is_err());
    let socket = temporary.path().join("socket");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    assert!(broker.edit_file(&socket, "a", "b").is_err());
}

#[test]
fn size_boundaries_preserve_rejected_files() {
    const LIMIT: u64 = 16 * 1024 * 1024;
    let temporary = tempfile::tempdir().unwrap();
    let broker = sandbox(temporary.path());
    let file = temporary.path().join("file");
    let oversized = fs::File::create(&file).unwrap();
    oversized.set_len(LIMIT + 1).unwrap();
    assert!(matches!(
        broker.edit_file(&file, "a", "b"),
        Err(Error::EditTooLarge)
    ));
    assert_eq!(fs::metadata(&file).unwrap().len(), LIMIT + 1);
    let mut text = "x".repeat(LIMIT as usize - 1);
    text.push('a');
    fs::write(&file, &text).unwrap();
    assert!(matches!(
        broker.edit_file(&file, "a", "bb"),
        Err(Error::EditTooLarge)
    ));
    assert_eq!(fs::read_to_string(&file).unwrap(), text);
    broker.edit_file(&file, "a", "").unwrap();
    assert_eq!(fs::metadata(&file).unwrap().len(), LIMIT - 1);
}

const ALLOWED_ENV: [&str; 10] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TZ",
    "VIRTUAL_ENV",
];

proptest! {
    #![proptest_config(config(128, 0x900d_1255))]

    #[test]
    fn randomized_environment_is_exactly_whitelisted(
        variables in proptest::collection::btree_map("[A-Z_][A-Z_0-9]{0,31}", "[^\\x00]{0,64}", 0..40),
        allowed in proptest::collection::vec(any::<bool>(), ALLOWED_ENV.len()),
        value in "[a-zA-Z0-9_/: .\\n]{0,64}",
    ) {
        let executable = std::env::current_exe().unwrap();
        let env_binary = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|path| path.join("env"))
            .find(|path| path.is_file())
            .unwrap()
            .canonicalize().unwrap();
        let mut command = std::process::Command::new(executable);
        command.args(["--exact", "edit_properties::environment_child", "--nocapture"]);
        command.env_clear().envs(&variables);
        for (name, selected) in ALLOWED_ENV.iter().zip(&allowed) {
            if *selected {
                command.env(name, &value);
            }
        }
        for name in [
            "OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY", "AWS_SECRET_ACCESS_KEY",
            "MUSH_SOCKET", "MUSH_SESSION", "MUSH_BWRAP", "RLM_DAEMON_SOCKET", "PYTHONPATH",
            "PYTHONSTARTUP", "BASH_ENV", "ENV", "TMPDIR", "TMP", "TEMP", "PYTHONNOUSERSITE",
        ] {
            command.env(name, &value);
        }
        command.env("MUSH_ENV_BINARY", env_binary);
        command.env("MUSH_ENV_CASE", "1");
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        command.env("LC_CTYPE", OsString::from_vec(vec![b'x', 0xff, b'y']));
        let output = command.output().unwrap();
        prop_assert!(output.status.success(), "{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    }
}

#[test]
fn environment_child() {
    if std::env::var_os("MUSH_ENV_CASE").is_none() {
        return;
    }
    Sandbox::check_support().unwrap();
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let mut broker = sandbox(root.path());
    broker.read.push(PathBuf::from("/nix/store"));
    let binary = PathBuf::from(std::env::var_os("MUSH_ENV_BINARY").unwrap());
    broker.read.push(binary.clone());
    let mut expected: BTreeMap<Vec<u8>, Vec<u8>> = ALLOWED_ENV
        .iter()
        .filter_map(|name| {
            std::env::var_os(name)
                .map(|value| (name.as_bytes().to_vec(), value.as_bytes().to_vec()))
        })
        .collect();
    expected.insert(b"PYTHONNOUSERSITE".to_vec(), b"1".to_vec());
    for name in ["TMPDIR", "TMP", "TEMP"] {
        expected.insert(
            name.as_bytes().to_vec(),
            broker.temporary.path().as_os_str().as_bytes().to_vec(),
        );
    }
    let mut command = tokio::process::Command::new(binary);
    command.as_std_mut().arg0("env");
    command.arg("-0");
    command.env("OPENAI_API_KEY", "command-only-secret");
    command.env("MUSH_CONTROL", "command-only-control");
    let mut command = broker.prepare_command(command).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let output = runtime.block_on(async { command.output().await }).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let actual: BTreeMap<Vec<u8>, Vec<u8>> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let equals = entry.iter().position(|byte| *byte == b'=').unwrap();
            (entry[..equals].to_vec(), entry[equals + 1..].to_vec())
        })
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn concurrent_hardlink_creation_keeps_external_unchanged() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("root");
    let outside = temporary.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    let broker = sandbox(&root);
    let file = root.join("file");
    let external = outside.join("linked");
    let mut text = "x".repeat(16 * 1024 * 1024 - 1);
    text.push('a');
    for delay in [0, 20, 50, 100, 200, 400, 800, 1600] {
        fs::write(&file, &text).unwrap();
        let original_inode = fs::metadata(&file).unwrap().ino();
        let ready = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                ready.wait();
                std::thread::sleep(std::time::Duration::from_micros(delay));
                fs::hard_link(&file, &external).unwrap();
            });
            ready.wait();
            let result = broker.edit_file(&file, "a", "b");
            worker.join().unwrap();
            if fs::metadata(&external).unwrap().ino() == original_inode {
                assert!(
                    fs::read_to_string(&external).unwrap() == text,
                    "external content changed at hardlink delay {delay}, edit result {result:?}"
                );
            }
        });
        fs::remove_file(&external).unwrap();
    }
}

proptest! {
    #![proptest_config(config(128, 0x30b9_270f))]

    #[test]
    fn replacing_granted_root_by_symlink_is_denied(
        file_grant in any::<bool>(),
        name in filename(),
        value in ".{0,128}",
    ) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("root");
        let outside = temporary.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        let external = outside.join(&name);
        let file = root.join(&name);
        fs::write(&file, &value).unwrap();
        fs::write(&external, &value).unwrap();
        let mut broker = sandbox(&root);
        if file_grant {
            broker.write = vec![file.clone()];
            fs::remove_file(&file).unwrap();
            symlink(&external, &file).unwrap();
        } else {
            fs::remove_file(&file).unwrap();
            fs::remove_dir(&root).unwrap();
            symlink(&outside, &root).unwrap();
        }
        prop_assert!(broker.edit_file(&file, "a", "b").is_err());
        prop_assert_eq!(fs::read_to_string(&external).unwrap(), value);
    }
}

proptest! {
    #![proptest_config(config(128, 0x17f0_845c))]

    #[test]
    fn replacement_preserves_permissions_and_removes_staging_entries(mode in 0u32..=0o777) {
        let temporary = tempfile::tempdir().unwrap();
        let file = temporary.path().join("file");
        fs::write(&file, "before").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(mode | 0o600)).unwrap();
        let broker = sandbox(temporary.path());
        broker.edit_file(&file, "before", "after").unwrap();
        prop_assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, mode | 0o600);
        prop_assert_eq!(fs::read_to_string(&file).unwrap(), "after");
        prop_assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 1);
    }
}
