//! logging setup - file-based tracing with ring buffer for TUI viewing

use std::collections::VecDeque;
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
    entries: VecDeque<String>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LogBufferInner {
                entries: VecDeque::with_capacity(capacity),
                capacity,
            })),
        }
    }

    /// get last N entries
    pub fn tail(&self, n: usize) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        let start = inner.entries.len().saturating_sub(n);
        inner.entries.iter().skip(start).cloned().collect()
    }
}

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(s) = std::str::from_utf8(buf) {
            // each write is a complete log line (tracing-subscriber writes one at a time)
            let trimmed = s.trim_end();
            if !trimmed.is_empty() {
                let mut inner = self.inner.lock().unwrap();
                if inner.capacity == 0 {
                    return Ok(buf.len());
                }
                if inner.entries.len() >= inner.capacity {
                    inner.entries.pop_front();
                }
                inner.entries.push_back(trimmed.to_string());
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

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    #[test]
    fn zero_capacity_ignores_writes() {
        let mut buffer = LogBuffer::new(0);

        buffer.write_all(b"hello\n").unwrap();
        buffer.write_all(b"world\n").unwrap();

        assert!(buffer.tail(10).is_empty());
    }

    #[test]
    fn overflow_keeps_most_recent_entries() {
        let mut buffer = LogBuffer::new(2);

        buffer.write_all(b"one\n").unwrap();
        buffer.write_all(b"two\n").unwrap();
        buffer.write_all(b"three\n").unwrap();

        assert_eq!(
            buffer.tail(10),
            vec!["two".to_string(), "three".to_string()]
        );
    }
}
