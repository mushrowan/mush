use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use color_eyre::eyre::{Result, WrapErr, eyre};
use mush_sync::{CheckoutError, Direction, Engine, Keys, Opened, Report, Scope};

use crate::config::{self, SyncConfig};
use crate::output::{Style, short_id};

pub const ABOUT: &str =
    "sync sessions, their files, saved tool output and the harness between machines";

pub const LONG_ABOUT: &str = "keeps sessions, their artifacts, saved tool output and the shared harness state in step \
between your machines through an end-to-end encrypted server. the server only ever holds ciphertext under \
blinded names.

logins and keys never sync. oauth.json, api-keys.json, the sync key and config.toml live in the config \
directory, which sync never reads, and credential-looking files beside a session (oauth.json, \
api-keys.json, sync.key, .env, *.pem, *.key) are skipped too. log in on each machine separately.

without a subcommand, runs one sync pass and prints what moved.";

#[derive(clap::Subcommand, Debug, Clone)]
pub enum SyncCommand {
    #[command(
        about = "show sync health: last sync, what is queued, open sessions, every machine, and forks or conflicts to review"
    )]
    Status,
    #[command(about = "keep syncing in the background, what the home manager service runs")]
    Watch,
    #[command(about = "run the sync server")]
    Serve {
        #[arg(long, default_value = "127.0.0.1:7347")]
        listen: String,
        #[arg(long)]
        root: PathBuf,
        #[arg(long, env = "MUSH_SYNC_TOKEN_HASH")]
        token_hash: String,
        #[arg(
            long,
            default_value_t = 30,
            help = "days to keep replaced history generations and deleted sessions before pruning them, 0 keeps them forever"
        )]
        keep_days: u64,
    },
    #[command(about = "make, show or hash the shared sync key")]
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
    #[command(about = "move a session that is open on another machine to this one")]
    Take { session: String },
    #[command(
        about = "merge a backup of sessions and harness state in without overwriting newer work"
    )]
    Import { dir: PathBuf },
    #[command(
        about = "bring back a deleted session, or an older generation of one as a copy. with no session, lists what the server can restore"
    )]
    Restore {
        session: Option<String>,
        #[arg(long)]
        generation: Option<u64>,
    },
    #[command(
        about = "prune replaced history and deleted sessions older than the server's retention, never anything a machine still needs"
    )]
    Prune {
        #[arg(long)]
        dry_run: bool,
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
            keep_days,
        }) => serve(&listen, root, token_hash, keep_days).await,
        Some(SyncCommand::Key { action }) => key(sync, action),
        Some(SyncCommand::Take { session }) => take(sync, &session).await,
        Some(SyncCommand::Import { dir }) => import(sync, &dir),
        Some(SyncCommand::Restore {
            session,
            generation,
        }) => restore(sync, session.as_deref(), generation).await,
        Some(SyncCommand::Prune { dry_run }) => prune(sync, dry_run).await,
    }
}

fn ago(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn local_ms() -> u64 {
    mush_ai::types::Timestamp::now().as_ms()
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
        ("identical copies joined", report.matched.len()),
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
    let engine = settings.engine().map_err(|error| eyre!("{error}"))?;
    let local = mush_sync::status::LocalStatus::load(engine.layout());
    let summary = engine.local_summary()?;
    let queue = engine.queue()?;
    let review = mush_sync::status::review(engine.layout())?;
    let started = std::time::Instant::now();
    let reachable = engine
        .remote()
        .changes(0, Duration::ZERO, true)
        .await
        .map(|_| started.elapsed());
    println!("{}\n", Style::Bold.stdout("mush sync"));
    match &reachable {
        Ok(latency) => println!(
            "  server:     {}  reachable, {}ms",
            settings.server,
            latency.as_millis()
        ),
        Err(error) => println!(
            "  server:     {}  {}",
            settings.server,
            Style::Red.stdout(format_args!("unreachable ({error})"))
        ),
    }
    println!("  machine:    {}", settings.machine);
    println!("  key:        {}", settings.key_file.display());
    let now = local_ms();
    match (local.last_ok_ms, &local.offline_since_ms, &local.last_error) {
        (_, Some(since), Some(error)) => println!(
            "  last sync:  {}",
            Style::Yellow.stdout(format_args!(
                "server unreachable for {} ({error})",
                ago(now.saturating_sub(*since))
            ))
        ),
        (Some(at), None, Some(error)) => println!(
            "  last sync:  {} ago, {}",
            ago(now.saturating_sub(at)),
            Style::Yellow.stdout(format_args!("with a problem: {error}"))
        ),
        (Some(at), _, None) => println!("  last sync:  {} ago", ago(now.saturating_sub(at))),
        (None, _, _) => println!("  last sync:  never from this machine"),
    }
    println!(
        "  here:       {} sessions, {} synced, {} to push, {} to pull",
        summary.sessions,
        summary.synced,
        queue.up.len(),
        queue.down.len()
    );
    if reachable.is_ok() {
        if let Ok(machines) = engine.remote().machines(Duration::from_secs(5)).await
            && !machines.machines.is_empty()
        {
            println!("\n{}\n", Style::Bold.stdout("machines"));
            for machine in &machines.machines {
                let this = if machine.name == settings.machine {
                    " (this one)"
                } else {
                    ""
                };
                let problem = machine
                    .report
                    .last_error
                    .as_deref()
                    .map(|error| format!(", {}", Style::Yellow.stdout(error)))
                    .unwrap_or_default();
                println!(
                    "  {}{this}  synced {} ago, {} sessions, {} waiting, {} failed, {} to review{problem}",
                    Style::Bold.stdout(&machine.name),
                    ago(machines.now.saturating_sub(machine.seen_at)),
                    machine.report.sessions,
                    machine.report.waiting,
                    machine.report.failed,
                    machine.report.review
                );
            }
        }
        if let Ok(leases) = engine.remote().leases(Duration::from_secs(5)).await
            && !leases.held.is_empty()
        {
            let names = local_names(&engine);
            println!("\n{}\n", Style::Bold.stdout("open sessions"));
            for (sid, lease) in &leases.held {
                let (id, title) = names
                    .get(sid)
                    .map_or((sid.as_str(), "(not on this machine)"), |(id, title)| {
                        (id.as_str(), title.as_str())
                    });
                println!(
                    "  {}  {title}  on {}, lease {}s left",
                    short_id(id),
                    lease.holder,
                    lease.expires_at.saturating_sub(leases.now) / 1000
                );
            }
        }
    }
    if !local.waiting.is_empty() || !local.failed.is_empty() {
        println!("\n{}\n", Style::Bold.stdout("last pass"));
        for (session, why) in &local.waiting {
            println!(
                "  {}",
                Style::Dim.stdout(format_args!("{} waits: {why}", short_id(session)))
            );
        }
        for (session, error) in &local.failed {
            println!(
                "  {}",
                Style::Red.stdout(format_args!("{} failed: {error}", short_id(session)))
            );
        }
    }
    if !review.is_empty() {
        println!("\n{}\n", Style::Bold.stdout("waiting for review"));
        for (session, fork, why) in &review.forks {
            println!(
                "  fork {} of {} ({why}), mush --resume {} to look, mush delete {} once done",
                short_id(fork),
                short_id(session),
                short_id(fork),
                short_id(fork)
            );
        }
        for (session, path) in &review.sidecars {
            println!(
                "  conflicting copy of {} kept at {}, delete it once reviewed",
                short_id(session),
                path.display()
            );
        }
    }
    Ok(())
}

fn local_names(engine: &Engine) -> std::collections::HashMap<String, (String, String)> {
    let mut names = std::collections::HashMap::new();
    let Ok(ids) = engine.layout().local_ids() else {
        return names;
    };
    for id in ids {
        let title = mush_session::load_meta(&engine.layout().meta(&id))
            .ok()
            .and_then(|meta| meta.title)
            .unwrap_or_else(|| "(untitled)".to_owned());
        names.insert(engine.keys().sid(&id), (id, title));
    }
    names
}

async fn restore(sync: &SyncConfig, session: Option<&str>, generation: Option<u64>) -> Result<()> {
    let engine = engine(sync)?;
    let found = engine
        .restorable()
        .await
        .map_err(|error| eyre!("{error}"))?;
    let Some(wanted) = session else {
        if found.is_empty() {
            println!(
                "{}",
                Style::Dim.stdout(
                    "nothing to restore, the server holds no deleted sessions or older generations"
                )
            );
            return Ok(());
        }
        let now = local_ms();
        for entry in &found {
            let title = entry.title.as_deref().unwrap_or("(untitled)");
            if let Some(at) = entry.deleted_at {
                let state = if entry.pruned {
                    Style::Dim.stdout("pruned, gone for good").to_string()
                } else {
                    format!("mush sync restore {}", short_id(&entry.session))
                };
                println!(
                    "  {}  {title}  deleted {} ago  {state}",
                    short_id(&entry.session),
                    ago(now.saturating_sub(at))
                );
            }
            for kept in &entry.generations {
                println!(
                    "  {}  {}  generation {}, {}, replaced {} ago  mush sync restore {} --generation {}",
                    short_id(&entry.session),
                    kept.title.as_deref().unwrap_or(title),
                    kept.generation,
                    size(kept.len),
                    ago(now.saturating_sub(kept.archived_at)),
                    short_id(&entry.session),
                    kept.generation
                );
            }
        }
        return Ok(());
    };
    let store = mush_session::SessionStore::new(mush_session::SessionStore::default_dir());
    let id = match store.resolve(wanted) {
        Ok(meta) => meta.id.to_string(),
        Err(_) => {
            let matches: Vec<&str> = found
                .iter()
                .map(|entry| entry.session.as_str())
                .filter(|id| id.starts_with(wanted))
                .collect();
            match matches.as_slice() {
                [one] => (*one).to_owned(),
                [] if mush_sync::protocol::valid_session(wanted) => wanted.to_owned(),
                [] => {
                    return Err(eyre!(
                        "no deleted session or kept generation matches {wanted}"
                    ));
                }
                many => {
                    return Err(eyre!(
                        "{wanted} matches {} sessions, give more of the id",
                        many.len()
                    ));
                }
            }
        }
    };
    match engine
        .restore(&id, generation)
        .await
        .map_err(|error| eyre!("{error}"))?
    {
        mush_sync::restore::Restored::Undeleted { session } => println!(
            "restored {}, open it with mush --resume {}",
            short_id(&session),
            short_id(&session)
        ),
        mush_sync::restore::Restored::Copied {
            session,
            copy,
            generation,
            missing,
        } => {
            println!(
                "generation {generation} of {} is back as {}, open it with mush --resume {}",
                short_id(&session),
                short_id(&copy),
                short_id(&copy)
            );
            if missing > 0 {
                println!(
                    "{}",
                    Style::Yellow.stdout(format_args!(
                        "{missing} of its files were no longer on the server"
                    ))
                );
            }
        }
    }
    Ok(())
}

async fn prune(sync: &SyncConfig, dry_run: bool) -> Result<()> {
    let engine = engine(sync)?;
    let report = engine
        .remote()
        .prune(dry_run)
        .await
        .map_err(|error| eyre!("{error}"))?;
    if report.keep_ms == 0 {
        println!(
            "{}",
            Style::Dim.stdout("the server keeps everything, it was started with --keep-days 0")
        );
        return Ok(());
    }
    let names = local_names(&engine);
    let name = |sid: &str| {
        names.get(sid).map_or_else(
            || format!("{} (not on this machine)", &sid[..sid.len().min(8)]),
            |(id, title)| format!("{} {title}", short_id(id)),
        )
    };
    let verb = if dry_run { "would remove" } else { "removed" };
    println!(
        "{verb} {} replaced history generations and {} deleted sessions, {} in all (retention {} days)",
        report.generations.len(),
        report.sessions.len(),
        size(report.freed + report.swept_bytes),
        report.keep_ms / 86_400_000
    );
    for generation in &report.generations {
        println!(
            "  generation {} of {}, {}",
            generation.generation,
            name(&generation.sid),
            size(generation.bytes)
        );
    }
    for session in &report.sessions {
        println!(
            "  deleted session {}, {}",
            name(&session.sid),
            size(session.bytes)
        );
    }
    if !report.blocked.is_empty() {
        println!("\n{}", Style::Bold.stdout("kept for now"));
        for blocked in &report.blocked {
            println!(
                "  {} of {}: {}",
                blocked.what,
                name(&blocked.sid),
                blocked.reason
            );
        }
    }
    if !report.stale_machines.is_empty() {
        println!(
            "{}",
            Style::Dim.stdout(format_args!(
                "not waiting for machines that stopped syncing: {}",
                report.stale_machines.join(", ")
            ))
        );
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

pub async fn serve(listen: &str, root: PathBuf, token_hash: String, keep_days: u64) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .wrap_err_with(|| format!("could not listen on {listen}"))?;
    eprintln!("mush sync server listening on {}", listener.local_addr()?);
    let config = mush_sync::server::ServerConfig {
        root,
        token_hash,
        retention: (keep_days > 0)
            .then(|| Duration::from_secs(keep_days.saturating_mul(24 * 60 * 60))),
    };
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
