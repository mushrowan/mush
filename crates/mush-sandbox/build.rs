fn main() {
    println!("cargo:rustc-check-cfg=cfg(mush_landlock_supported)");
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
        }
    }
}
