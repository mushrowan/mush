use std::process::{Command, ExitCode};

use mush_ai::Provider;
use mush_ai::discovery::{self, DiscoveryOutcome};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    match args.as_slice() {
        ["check", "fast"] => run("cargo", &["nextest", "run", "--profile", "fast"]),
        ["check", "full"] => run("cargo", &["nextest", "run", "--profile", "full"]),
        ["check", "all"] => run("nix", &["flake", "check", "--quiet"]),
        ["discovery", "probe"] => discovery_probe(),
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

fn discovery_probe() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("failed to build tokio runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(async {
        let summary = discovery::refresh_and_save(mush_ai::http::client()).await;
        println!("{}", summary.one_line());

        for result in &summary.results {
            match &result.outcome {
                DiscoveryOutcome::Success { models, .. } => {
                    println!("  {}: {models} models", result.provider);
                }
                DiscoveryOutcome::Skipped => println!("  {}: skipped", result.provider),
                DiscoveryOutcome::Failed(error) => {
                    println!("  {}: error: {error}", result.provider);
                }
            }
        }

        let merged = discovery::merged_catalogue();
        let count = |name: &str| {
            let provider = Provider::Custom(name.into());
            merged
                .iter()
                .filter(|entry| entry.model.provider == provider)
                .count()
        };

        println!("merged openai models: {}", count("openai"));
        println!("merged codex models: {}", count("openai-codex"));
        ExitCode::SUCCESS
    })
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
  discovery probe
                run live model discovery using the current user env and config
  deny          run cargo-deny checks
  fmt           format the workspace
  upgrade       upgrade deps to latest versions"
    );
}
