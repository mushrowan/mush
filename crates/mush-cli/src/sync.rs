use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use color_eyre::eyre::{Result, WrapErr, eyre};
use mush_sync::{CheckoutError, Direction, Engine, Keys, Opened, Report, Scope};

use crate::config::{self, SyncConfig};
use crate::output::{Style, short_id};

#[derive(clap::Subcommand, Debug, Clone)]
pub enum SyncCommand {
    Status,
    Watch,
    Serve {
        #[arg(long, default_value = "127.0.0.1:7347")]
        listen: String,
        #[arg(long)]
        root: PathBuf,
        #[arg(long, env = "MUSH_SYNC_TOKEN_HASH")]
        token_hash: String,
    },
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
    Take {
        session: String,
    },
    Import {
        dir: PathBuf,
    },
}

impl SyncCommand {
    #[must_use]
    pub fn runs_as_service(&self) -> bool {
        matches!(self, Self::Serve { .. } | Self::Watch)
    }
}

pub fn log_to_stderr() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,mush_sync=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_target(false)
        .try_init();
}

#[derive(clap::Subcommand, Debug, Clone, Copy)]
pub enum KeyAction {
    Generate,
    Show,
    TokenHash,
}

fn off() -> color_eyre::Report {
    eyre!(
        "sync is off, set [sync] server in {} (for example server = \"http://val:7347\")",
        config::config_path().display()
    )
}

fn engine(sync: &SyncConfig) -> Result<Arc<Engine>> {
    let settings = sync.settings().ok_or_else(off)?;
    settings.engine().map_err(|error| eyre!("{error}"))
}

fn key_path(sync: &SyncConfig) -> PathBuf {
    sync.settings().map_or_else(
        || config::config_dir().join("sync.key"),
        |settings| settings.key_file,
    )
}

async fn stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

pub async fn run(command: Option<SyncCommand>, sync: &SyncConfig) -> Result<()> {
    match command {
        None => once(sync).await,
        Some(SyncCommand::Status) => status(sync).await,
        Some(SyncCommand::Watch) => watch(sync).await,
        Some(SyncCommand::Serve {
            listen,
            root,
            token_hash,
        }) => serve(&listen, root, token_hash).await,
        Some(SyncCommand::Key { action }) => key(sync, action),
        Some(SyncCommand::Take { session }) => take(sync, &session).await,
        Some(SyncCommand::Import { dir }) => import(sync, &dir),
    }
}

fn import(sync: &SyncConfig, dir: &Path) -> Result<()> {
    let layout = mush_sync::Layout::new(&mush_session::data_dir());
    let report = mush_sync::import::import(&layout, dir)
        .wrap_err_with(|| format!("could not import {}", dir.display()))?;
    let counts = [
        ("added", report.added.len()),
        ("advanced", report.advanced.len()),
        ("already here", report.unchanged.len()),
        ("kept as forks", report.forked.len()),
        ("open here, skipped", report.busy.len()),
    ];
    println!(
        "{}",
        counts
            .iter()
            .map(|(label, count)| format!("{label} {count}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    for (session, fork) in &report.forked {
        println!(
            "{}",
            Style::Yellow.stdout(format_args!(
                "{} differs from the copy here, the backup's version is now {}",
                short_id(session),
                short_id(fork)
            ))
        );
    }
    if !report.kept_beside.is_empty() {
        println!(
            "{}",
            Style::Dim.stdout(format_args!(
                "{} older or competing session headers were kept beside their sessions as .sync-conflict files",
                report.kept_beside.len()
            ))
        );
    }
    if report.harness {
        println!("merged the backed up harness state");
    }
    if sync.settings().is_some() {
        println!(
            "{}",
            Style::Dim.stdout("mush sync shares it with your other machines")
        );
    }
    Ok(())
}

fn print_report(report: &Report) {
    let counts = [
        ("pushed", report.pushed.len()),
        ("pulled", report.pulled.len()),
        ("deleted", report.deleted.len()),
        ("forked", report.forked.len()),
        ("waiting", report.waiting.len()),
        ("failed", report.failed.len()),
    ];
    let summary = counts
        .iter()
        .filter(|(_, count)| *count > 0)
        .map(|(label, count)| format!("{label} {count}"))
        .collect::<Vec<_>>()
        .join(", ");
    if summary.is_empty() {
        println!("{}", Style::Dim.stdout("everything is in sync"));
    } else {
        println!("{summary}");
    }
    for (session, fork) in &report.forked {
        println!(
            "{}",
            Style::Yellow.stdout(format_args!(
                "{} diverged, this machine's side is kept as {}",
                short_id(session),
                short_id(fork)
            ))
        );
    }
    for (session, key) in &report.sidecars {
        println!(
            "{}",
            Style::Yellow.stdout(format_args!(
                "{} {key} changed on both sides, this machine's copy is kept beside it",
                short_id(session)
            ))
        );
    }
    for (session, why) in &report.waiting {
        println!(
            "{}",
            Style::Dim.stdout(format_args!("{} waits: {why}", short_id(session)))
        );
    }
    for (session, error) in &report.failed {
        println!(
            "{}",
            Style::Red.stdout(format_args!("{} failed: {error}", short_id(session)))
        );
    }
}

async fn once(sync: &SyncConfig) -> Result<()> {
    let settings = sync.settings().ok_or_else(off)?;
    let engine = if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        settings.engine_with_progress(Arc::new(|done, total| {
            if total > 20 {
                eprint!("\r{done}/{total} sessions");
                if done == total {
                    eprintln!();
                }
            }
        }))
    } else {
        settings.engine()
    }
    .map_err(|error| eyre!("{error}"))?;
    let report = engine
        .pass(Scope::All, Direction::Both, None, Duration::from_secs(120))
        .await
        .map_err(|error| eyre!("{error}"))?;
    print_report(&report);
    if report.failed.is_empty() {
        Ok(())
    } else {
        Err(eyre!("{} sessions did not sync", report.failed.len()))
    }
}

async fn status(sync: &SyncConfig) -> Result<()> {
    let settings = sync.settings().ok_or_else(off)?;
    println!("{}\n", Style::Bold.stdout("mush sync"));
    println!("  server:  {}", settings.server);
    println!("  machine: {}", settings.machine);
    println!("  key:     {}", settings.key_file.display());
    let engine = settings.engine().map_err(|error| eyre!("{error}"))?;
    let local = engine.local_summary()?;
    println!(
        "  here:    {} sessions, {} synced, {} waiting",
        local.sessions, local.synced, local.waiting
    );
    match engine.overview().await {
        Ok(overview) => {
            println!("  remote:  {} sessions, reachable", overview.sessions);
            if !overview.elsewhere.is_empty() {
                println!("\n{}\n", Style::Bold.stdout("open on other machines"));
                for open in &overview.elsewhere {
                    println!(
                        "  {}  {}  on {}, lease {}s left",
                        short_id(&open.session),
                        open.title.as_deref().unwrap_or("(untitled)"),
                        open.holder,
                        open.left.as_secs()
                    );
                }
            }
        }
        Err(error) => println!(
            "  remote:  {}",
            Style::Yellow.stdout(format_args!("unreachable ({error})"))
        ),
    }
    Ok(())
}

async fn watch(sync: &SyncConfig) -> Result<()> {
    let engine = engine(sync)?;
    eprintln!(
        "{}",
        Style::Dim.stderr(format_args!("syncing sessions as {}", engine.machine()))
    );
    mush_sync::watch::watch(engine, stop_signal())
        .await
        .map_err(|error| eyre!("{error}"))
}

pub async fn serve(listen: &str, root: PathBuf, token_hash: String) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .wrap_err_with(|| format!("could not listen on {listen}"))?;
    eprintln!("mush sync server listening on {}", listener.local_addr()?);
    let config = mush_sync::server::ServerConfig { root, token_hash };
    tokio::select! {
        served = mush_sync::server::serve(listener, config) => served?,
        () = stop_signal() => {}
    }
    Ok(())
}

fn key(sync: &SyncConfig, action: KeyAction) -> Result<()> {
    let path = key_path(sync);
    match action {
        KeyAction::Generate => generate(&path),
        KeyAction::Show => {
            Keys::load(&path).map_err(|error| eyre!("{error}"))?;
            println!("{}", std::fs::read_to_string(&path)?.trim());
            Ok(())
        }
        KeyAction::TokenHash => {
            let keys = Keys::load(&path).map_err(|error| eyre!("{error}"))?;
            println!("{}", keys.token_hash());
            Ok(())
        }
    }
}

fn generate(path: &Path) -> Result<()> {
    if path.exists() {
        return Err(eyre!(
            "{} already exists, every machine needs the same key so it is never replaced",
            path.display()
        ));
    }
    let master = Keys::generate_master()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    mush_ai::private_io::write_private(path, format!("{master}\n"))?;
    let keys = Keys::parse(&master).ok_or_else(|| eyre!("the new key did not parse"))?;
    println!("wrote {}", path.display());
    println!("copy it to every machine that syncs (or into your secrets)");
    println!("the server only needs this hash: {}", keys.token_hash());
    Ok(())
}

async fn take(sync: &SyncConfig, session: &str) -> Result<()> {
    let settings = sync.settings().ok_or_else(off)?;
    let engine = settings.engine().map_err(|error| eyre!("{error}"))?;
    let store = mush_session::SessionStore::new(mush_session::SessionStore::default_dir());
    let id = match store.resolve(session) {
        Ok(meta) => meta.id.to_string(),
        Err(mush_session::StoreError::NotFound(_))
            if mush_sync::protocol::valid_session(session) =>
        {
            session.to_owned()
        }
        Err(error) => return Err(eyre!("{error}")),
    };
    let previous = engine
        .take(&id, settings.lease_ttl)
        .await
        .map_err(|error| eyre!("{error}"))?;
    let from = previous
        .map(|holder| format!(" from {holder}"))
        .unwrap_or_default();
    println!(
        "took {}{from}, open it here within {}s (mush --resume {})",
        short_id(&id),
        settings.lease_ttl.as_secs(),
        short_id(&id)
    );
    Ok(())
}

pub async fn checkout(
    sync: &SyncConfig,
    lease: &mush_session::SessionLease,
) -> Result<Option<mush_sync::Checkout>> {
    let Some(settings) = sync.settings() else {
        return Ok(None);
    };
    let engine = match settings.engine() {
        Ok(engine) => engine,
        Err(error) => {
            tracing::warn!(%error, "sync is configured but cannot start");
            return Ok(None);
        }
    };
    match mush_sync::checkout(engine, lease, settings.lease_ttl, false).await {
        Ok(Opened::Claimed(checkout)) => Ok(Some(checkout)),
        Ok(Opened::Offline(reason)) => {
            tracing::info!(
                reason,
                "sync server unreachable, opening the session offline"
            );
            Ok(None)
        }
        Err(error @ CheckoutError::Held { .. }) => Err(eyre!("{error}")),
        Err(error) => {
            tracing::warn!(%error, "could not check the session out of sync");
            Ok(None)
        }
    }
}
