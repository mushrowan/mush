fn main() {
    println!("cargo:rustc-check-cfg=cfg(mush_landlock_supported)");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var_os("HOST") == std::env::var_os("TARGET")
        && mush_sandbox::Sandbox::check_support().is_ok()
    {
        println!("cargo:rustc-cfg=mush_landlock_supported");
    }
}
