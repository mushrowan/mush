use std::sync::Arc;

use mush_tools::ipython::{BusyKernelChoice, BusyKernelPrompt};
use mush_tools::kernel_jobs::{JobNotice, NoticeSink};
use tokio::sync::{mpsc, oneshot};

pub struct KernelSinks {
    pub output: mush_tools::bash::OutputSink,
    pub notices: NoticeSink,
    pub busy_prompt: BusyKernelPrompt,
}

pub struct KernelSignals {
    pub(crate) output: mpsc::UnboundedReceiver<(String, String)>,
    pub(crate) notices: mpsc::UnboundedReceiver<JobNotice>,
    pub(crate) busy: mpsc::UnboundedReceiver<oneshot::Sender<BusyKernelChoice>>,
}

#[must_use]
pub fn kernel_channels() -> (KernelSinks, KernelSignals) {
    let (output_tx, output) = mpsc::unbounded_channel();
    let (notices_tx, notices) = mpsc::unbounded_channel();
    let (busy_tx, busy) = mpsc::unbounded_channel();
    let sinks = KernelSinks {
        output: Arc::new(move |exec_id: &str, line: &str| {
            let _ = output_tx.send((exec_id.to_string(), line.to_string()));
        }),
        notices: Arc::new(move |notice: JobNotice| {
            let _ = notices_tx.send(notice);
        }),
        busy_prompt: Arc::new(move || {
            let (reply, answer) = oneshot::channel();
            let _ = busy_tx.send(reply);
            Box::pin(async move { answer.await.unwrap_or(BusyKernelChoice::Cancel) })
        }),
    };
    (
        sinks,
        KernelSignals {
            output,
            notices,
            busy,
        },
    )
}
