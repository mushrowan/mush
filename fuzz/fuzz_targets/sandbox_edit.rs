#![no_main]
use libfuzzer_sys::fuzz_target;
use mush_sandbox::{Profile, Sandbox};

fuzz_target!(|data: &[u8]| {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let marker = outside.path().join("marker");
    std::fs::write(&marker, b"outside").unwrap();
    let sandbox = Sandbox::new(Profile::default(), root.path(), &[], &[]).unwrap();
    let mut directory = root.path().to_path_buf();
    for (depth, byte) in data.iter().take(8).enumerate() {
        let name = format!("界{depth}{}", "x".repeat(*byte as usize));
        directory.push(name);
        if std::fs::create_dir(&directory).is_err() { break; }
        let path = directory.join("file");
        let kind = byte % 4;
        if kind == 0 { std::fs::write(&path, b"outside").unwrap(); }
        #[cfg(unix)]
        if kind == 1 { std::os::unix::fs::symlink(&marker, &path).unwrap(); }
        if kind == 2 { std::fs::hard_link(&marker, &path).unwrap(); }
        #[cfg(unix)]
        if kind == 3 { std::os::unix::fs::symlink(outside.path(), &path).unwrap(); }
        let target = if kind == 3 { path.join("marker") } else { path };
        let replacement = String::from_utf8_lossy(data);
        let _ = sandbox.edit_file(&target, "outside", &replacement);
        let _ = sandbox.edit_file(&directory.join("../file"), "outside", &replacement);
        assert_eq!(std::fs::read(&marker).unwrap(), b"outside");
    }
});
