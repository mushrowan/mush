#[cfg(target_os = "linux")]
fn main() {
    let mut arguments = std::env::args_os().skip(1);
    let Some(manifest) = arguments.next() else {
        eprintln!("usage: mush-sandbox-net <manifest>");
        std::process::exit(2);
    };
    let extra = arguments.next();
    if manifest == "--check" && extra.is_none() {
        return;
    }
    if extra.is_some() {
        eprintln!("usage: mush-sandbox-net <manifest>");
        std::process::exit(2);
    }
    match mush_sandbox::run_network_helper(std::path::Path::new(&manifest)) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("mush-sandbox-net: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("mush-sandbox-net requires linux");
    std::process::exit(1);
}
