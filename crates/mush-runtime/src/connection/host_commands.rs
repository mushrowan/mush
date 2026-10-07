use std::collections::VecDeque;
use std::sync::nonpoison::Mutex;

use super::ClientCommand;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextUsage {
    pub tokens: Option<u64>,
    pub context_window: Option<u64>,
}

impl ContextUsage {
    #[must_use]
    pub fn percent(&self) -> Option<f64> {
        let (tokens, window) = (self.tokens?, self.context_window?);
        (window > 0).then(|| tokens as f64 * 100.0 / window as f64)
    }
}

#[derive(Default)]
pub struct HostCommands {
    queue: Mutex<VecDeque<ClientCommand>>,
    context: Mutex<ContextUsage>,
}

impl HostCommands {
    pub fn record_context(&self, usage: ContextUsage) {
        *self.context.lock() = usage;
    }

    #[must_use]
    pub fn context_usage(&self) -> ContextUsage {
        *self.context.lock()
    }

    #[must_use]
    pub fn compaction_queued(&self) -> bool {
        self.queue
            .lock()
            .iter()
            .any(|command| matches!(command, ClientCommand::Compact { .. }))
    }

    pub fn push(&self, command: ClientCommand) {
        self.queue.lock().push_back(command);
    }

    pub(super) fn drain(&self) -> Vec<ClientCommand> {
        self.queue.lock().drain(..).collect()
    }

    pub fn replace_refine(&self, request: mush_session::refine::RefineRequest) {
        let mut queue = self.queue.lock();
        queue.retain(|command| !matches!(command, ClientCommand::Refine(_)));
        queue.push_back(ClientCommand::Refine(request));
    }

    #[must_use]
    pub fn pending_refine(&self) -> bool {
        self.queue
            .lock()
            .iter()
            .any(|command| matches!(command, ClientCommand::Refine(_)))
    }

    #[must_use]
    pub fn pending(&self) -> usize {
        self.queue.lock().len()
    }
}
