//! logging setup - file-based tracing with ring buffer for TUI viewing

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// shared ring buffer for recent log entries
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<LogBufferInner>>,
}

struct LogBufferInner {
    entries: Vec<String>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LogBufferInner {
                entries: Vec::with_capacity(capacity),
                capacity,
            })),
        }
    }

    /// get recent log entries
    pub fn entries(&self) -> Vec<String> {
        self.inner.lock().unwrap().entries.clone()
    }

    /// get last N entries
    pub fn tail(&self, n: usize) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        let start = inner.entries.len().saturating_sub(n);
        inner.entries[start..].to_vec()
    }
}

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(s) = std::str::from_utf8(buf) {
            // each write is a complete log line (tracing-subscriber writes one at a time)
            let trimmed = s.trim_end();
            if !trimmed.is_empty() {
                let mut inner = self.inner.lock().unwrap();
                if inner.entries.len() >= inner.capacity {
                    inner.entries.remove(0);
                }
                inner.entries.push(trimmed.to_string());
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// log file path: ~/.local/share/mush/mush.log
pub fn log_file_path() -> PathBuf {
    let dir = mush_session::data_dir();
    std::fs::create_dir_all(&dir).ok();
    dir.join("mush.log")
}

/// initialise tracing with file output + ring buffer
/// returns the guard (must be held alive) and the log buffer
pub fn init_logging(config_filter: Option<&str>) -> (WorkerGuard, LogBuffer) {
    let log_path = log_file_path();
    let log_dir = log_path.parent().unwrap();
    let log_name = log_path.file_name().unwrap();

    let file_appender = tracing_appender::rolling::never(log_dir, log_name);
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    // ring buffer for TUI /logs command
    let log_buffer = LogBuffer::new(500);
    let (buf_writer, _buf_guard) = tracing_appender::non_blocking(log_buffer.clone());
    // leak the buffer guard so it lives as long as the process
    std::mem::forget(_buf_guard);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let default = config_filter.unwrap_or("warn");
        EnvFilter::new(default)
    });

    tracing_subscriber::registry()
        .with(filter)
        .with(
            fmt::layer()
                .with_writer(file_writer)
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(false)
                .compact(),
        )
        .with(
            fmt::layer()
                .with_writer(buf_writer)
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(false)
                .compact(),
        )
        .init();

    (guard, log_buffer)
}
