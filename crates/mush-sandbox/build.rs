#[cfg(target_os = "linux")]
use std::path::PathBuf;

#[cfg(target_os = "linux")]
fn executable(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|path| path.is_absolute() && path.is_file())
}

#[cfg(target_os = "linux")]
fn network_supported() -> bool {
    let bwrap = std::env::var_os("MUSH_BWRAP")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_file())
        .or_else(|| executable("bwrap"));
    let Some((bwrap, probe)) = bwrap.zip(executable("true")) else {
        return false;
    };
    std::process::Command::new(bwrap)
        .args([
            "--unshare-user",
            "--unshare-net",
            "--ro-bind",
            "/",
            "/",
            "--",
        ])
        .arg(probe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn main() {
    println!("cargo:rustc-check-cfg=cfg(mush_landlock_supported)");
    println!("cargo:rustc-check-cfg=cfg(mush_network_supported)");
    println!("cargo:rerun-if-env-changed=MUSH_BWRAP");
    println!("cargo:rerun-if-env-changed=PATH");
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(target_os = "linux")]
    if std::env::var_os("HOST") == std::env::var_os("TARGET") {
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<libc::c_void>(),
                0usize,
                1u32,
            )
        };
        if abi >= 9 {
            println!("cargo:rustc-cfg=mush_landlock_supported");
            if network_supported() {
                println!("cargo:rustc-cfg=mush_network_supported");
            }
        }
    }
}
