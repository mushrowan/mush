use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    match args.as_slice() {
        ["check", "fast"] => run("cargo", &["nextest", "run", "--profile", "fast"]),
        ["check", "full"] => run("cargo", &["nextest", "run", "--profile", "full"]),
        ["check", "all"] => run("nix", &["flake", "check", "--quiet"]),
        ["deny"] => run("cargo", &["deny", "check"]),
        ["fmt"] => run("cargo", &["fmt"]),
        ["upgrade"] => {
            let r = run("cargo", &["upgrade", "--incompatible", "allow"]);
            if r != ExitCode::SUCCESS {
                return r;
            }
            run("cargo", &["update"])
        }
        _ => {
            usage();
            ExitCode::FAILURE
        }
    }
}

fn run(program: &str, args: &[&str]) -> ExitCode {
    eprintln!("  → {program} {}", args.join(" "));
    match Command::new(program).args(args).status() {
        Ok(status) => {
            if status.success() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(status.code().unwrap_or(1) as u8)
            }
        }
        Err(e) => {
            eprintln!("failed to run {program}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    eprintln!(
        "usage: cargo x <command>

commands:
  check fast    run tests with the fast nextest profile
  check full    run tests with the full nextest profile
  check all     run nix flake check
  deny          run cargo-deny checks
  fmt           format the workspace
  upgrade       upgrade deps to latest versions"
    );
}
