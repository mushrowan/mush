use std::path::PathBuf;

use mush_agent::ipc::{
    IpcMessage, IpcMessageKind,
    control::{Event, Operation, Reply, Request, Target, new_id},
    listener::exchange,
};

#[derive(Debug, clap::Args)]
pub struct ControlArgs {
    #[arg(long)]
    socket: Option<PathBuf>,
    #[arg(long)]
    instance: Option<String>,
    #[arg(long)]
    session: Option<String>,
    #[arg(long)]
    activation: Option<String>,
    #[arg(long, default_value = "local-cli")]
    source: String,
    #[arg(long)]
    request_id: Option<String>,
    #[command(subcommand)]
    command: ControlCommand,
}

#[derive(Debug, clap::Subcommand)]
enum ControlCommand {
    Send {
        text: String,
        #[arg(long)]
        worker: Option<String>,
    },
    #[command(name = "wake", visible_alias = "schedule")]
    Schedule {
        text: String,
        #[arg(long)]
        delay_ms: u64,
        #[arg(long, default_value_t = 60_000)]
        ttl_ms: u64,
    },
    Cancel {
        id: String,
    },
    Status {
        id: Option<String>,
    },
}

pub async fn run(args: ControlArgs) -> color_eyre::Result<()> {
    use color_eyre::eyre::eyre;
    let (socket, instance, session, activation) = if let Some(socket) = args.socket {
        (socket, args.instance, args.session, args.activation)
    } else {
        if args.instance.is_some() || args.session.is_some() || args.activation.is_some() {
            return Err(eyre!("provide --socket with explicit target fields"));
        }
        (
            std::env::var_os("MUSH_CONTROL_SOCKET")
                .map(PathBuf::from)
                .ok_or_else(|| eyre!("provide --socket or use the tool environment"))?,
            std::env::var("MUSH_CONTROL_INSTANCE").ok(),
            std::env::var("MUSH_CONTROL_SESSION").ok(),
            std::env::var("MUSH_CONTROL_ACTIVATION").ok(),
        )
    };
    let target = match (instance, session, activation) {
        (Some(instance), Some(session), Some(activation)) => Target {
            instance,
            session,
            activation,
        },
        (None, None, None) if matches!(args.command, ControlCommand::Status { .. }) => {
            let response = exchange(
                &socket,
                &IpcMessage {
                    id: new_id(),
                    from: args.source.clone(),
                    kind: IpcMessageKind::GetTarget,
                },
            )
            .await?;
            let IpcMessageKind::Target(target) = response.kind else {
                return Err(eyre!("control target unavailable"));
            };
            target
        }
        _ => {
            return Err(eyre!(
                "provide --instance, --session and --activation, or use the tool environment"
            ));
        }
    };
    let operation = match args.command {
        ControlCommand::Send { text, worker } => Operation::Send {
            event: match worker {
                Some(worker) => Event::WorkerCompletion { worker, text },
                None => Event::Message { text },
            },
        },
        ControlCommand::Schedule {
            text,
            delay_ms,
            ttl_ms,
        } => Operation::Schedule {
            event: Event::Wake { text },
            delay_ms,
            ttl_ms,
        },
        ControlCommand::Cancel { id } => Operation::Cancel { id },
        ControlCommand::Status { id } => Operation::Status { id },
    };
    let response = exchange(
        &socket,
        &IpcMessage {
            id: args.request_id.unwrap_or_else(new_id),
            from: args.source,
            kind: IpcMessageKind::Control(Request { target, operation }),
        },
    )
    .await?;
    println!("{}", serde_json::to_string(&response)?);
    match response.kind {
        IpcMessageKind::ControlReply(Reply::Error { code }) => Err(eyre!(code)),
        IpcMessageKind::ControlReply(_) => Ok(()),
        _ => Err(eyre!("unexpected control response")),
    }
}

pub fn follow_up(control: mush_agent::ipc::control::Control) -> mush_agent::SteeringFn {
    Box::new(move || {
        let control = control.clone();
        Box::pin(async move {
            control.finish(mush_agent::ipc::control::Outcome::Completed);
            let Some(delivery) = control.take_next(&control.target()) else {
                return Vec::new();
            };
            match delivery.message() {
                Ok(message) => vec![message],
                Err(_) => {
                    control.finish(mush_agent::ipc::control::Outcome::Failed);
                    Vec::new()
                }
            }
        })
    })
}
