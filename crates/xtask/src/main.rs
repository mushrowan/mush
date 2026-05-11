use std::process::{Command, ExitCode};

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
        let client = reqwest::Client::new();
        let summary = mush_ai::discovery::refresh_and_save(client).await;
        println!("{}", summary.one_line());

        for result in &summary.results {
            match &result.outcome {
                mush_ai::discovery::DiscoveryOutcome::Success { models, .. } => {
                    println!("  {}: {models} models", result.provider);
                }
                mush_ai::discovery::DiscoveryOutcome::Skipped => {
                    println!("  {}: skipped", result.provider);
                }
                mush_ai::discovery::DiscoveryOutcome::Failed(error) => {
                    println!("  {}: error: {error}", result.provider);
                }
            }
        }

        let merged = mush_ai::discovery::merged_catalogue();
        let openai = merged
            .iter()
            .filter(|entry| entry.model.provider == mush_ai::Provider::Custom("openai".into()))
            .count();
        let codex = merged
            .iter()
            .filter(|entry| {
                entry.model.provider == mush_ai::Provider::Custom("openai-codex".into())
            })
            .count();

        println!("merged openai models: {openai}");
        println!("merged codex models: {codex}");
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
