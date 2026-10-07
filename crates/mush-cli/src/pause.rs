use color_eyre::eyre::{Result, eyre};
use mush_daemon::WorkerDescriptor;
use mush_runtime::{AgentConnection, ClientCommand};
use mush_session::Pause;

use crate::agents;
use crate::output::{Style, short_id};

async fn targets(selector: Option<&str>, all: bool) -> Result<Vec<WorkerDescriptor>> {
    let workers = agents::live().await?;
    match (selector, all) {
        (Some(_), true) => Err(eyre!("pass an agent or --all, not both")),
        (None, false) => Err(eyre!("name an agent, or pass --all")),
        (None, true) => Ok(workers),
        (Some(selector), false) => Ok(vec![agents::sole_match(workers, selector)?]),
    }
}

async fn apply(workers: Vec<WorkerDescriptor>, pause: Option<Pause>) -> Result<()> {
    if workers.is_empty() {
        println!("no live agents");
        return Ok(());
    }
    let verb = if pause.is_some() { "paused" } else { "resumed" };
    let mut failures = 0;
    for worker in &workers {
        let result = async {
            let mut connection = agents::connect(worker).await?;
            connection
                .send(ClientCommand::SetPaused(pause.clone()))
                .await?;
            Ok::<_, color_eyre::Report>(())
        }
        .await;
        match result {
            Ok(()) => println!("{verb} {}", short_id(&worker.session_id)),
            Err(error) => {
                failures += 1;
                eprintln!(
                    "{}",
                    Style::Dim.stderr(format_args!("{}: {error}", short_id(&worker.session_id)))
                );
            }
        }
    }
    if failures > 0 {
        return Err(eyre!("{failures} of {} did not answer", workers.len()));
    }
    Ok(())
}

pub async fn pause(selector: Option<&str>, all: bool, reason: Option<&str>) -> Result<()> {
    let pause = Pause {
        at: mush_ai::types::Timestamp::now(),
        reason: reason.map(str::to_string),
        by: Some("cli".to_string()),
    };
    apply(targets(selector, all).await?, Some(pause)).await
}

pub async fn resume(selector: Option<&str>, all: bool) -> Result<()> {
    apply(targets(selector, all).await?, None).await
}
