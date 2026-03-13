//! LSP client for communicating with a single language server
//!
//! handles spawning the server process, the initialize handshake,
//! sending requests and notifications, and receiving responses.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use lsp_types::{Diagnostic, DiagnosticSeverity, PublishDiagnosticsParams, Uri};
use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, oneshot};

use crate::discovery::ServerConfig;
use crate::error::LspError;
use crate::transport;

/// timeout for LSP requests (seconds)
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// an active LSP server connection
pub struct LspClient {
    /// server process handle
    _process: Child,
    /// writer to server stdin (behind mutex for shared access)
    writer: Arc<Mutex<ChildStdin>>,
    /// monotonically increasing request id
    next_id: AtomicI64,
    /// pending requests waiting for responses
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    /// latest diagnostics per file URI
    diagnostics: Arc<Mutex<HashMap<Uri, Vec<Diagnostic>>>>,
    /// workspace root
    root_uri: Uri,
    /// background reader task handle
    _reader_task: tokio::task::JoinHandle<()>,
}

impl LspClient {
    /// spawn a language server and perform the initialize handshake
    pub async fn start(config: &ServerConfig, root: &Path) -> Result<Self, LspError> {
        let root_uri = path_to_uri(root)?;

        let mut process = Command::new(&config.command)
            .args(&config.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| LspError::SpawnFailed(format!("{}: {e}", config.command)))?;

        let stdin = process.stdin.take().ok_or_else(|| {
            LspError::SpawnFailed("failed to capture server stdin".into())
        })?;
        let stdout = process.stdout.take().ok_or_else(|| {
            LspError::SpawnFailed("failed to capture server stdout".into())
        })?;

        let writer = Arc::new(Mutex::new(stdin));
        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let diagnostics: Arc<Mutex<HashMap<Uri, Vec<Diagnostic>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let reader_task = spawn_reader(stdout, Arc::clone(&pending), Arc::clone(&diagnostics));

        let mut client = Self {
            _process: process,
            writer,
            next_id: AtomicI64::new(1),
            pending,
            diagnostics,
            root_uri,
            _reader_task: reader_task,
        };

        client.initialize().await?;

        Ok(client)
    }

    /// send a JSON-RPC request and wait for the response
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        {
            let mut writer = self.writer.lock().await;
            transport::write_message(&mut writer, &message).await?;
        }

        let response = tokio::time::timeout(REQUEST_TIMEOUT, rx)
            .await
            .map_err(|_| LspError::Timeout)?
            .map_err(|_| LspError::ServerExited)?;

        if let Some(error) = response.get("error") {
            return Err(LspError::Request {
                code: error["code"].as_i64().unwrap_or(-1),
                message: error["message"]
                    .as_str()
                    .unwrap_or("unknown error")
                    .to_string(),
            });
        }

        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// send a JSON-RPC notification (no response expected)
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut writer = self.writer.lock().await;
        transport::write_message(&mut writer, &message).await
    }

    /// perform the initialize/initialized handshake
    async fn initialize(&mut self) -> Result<(), LspError> {
        let params = json!({
            "processId": std::process::id(),
            "rootUri": self.root_uri.as_str(),
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": {
                        "relatedInformation": true
                    },
                    "synchronization": {
                        "willSave": false,
                        "willSaveWaitUntil": false
                    }
                }
            },
            "workspaceFolders": [{
                "uri": self.root_uri.as_str(),
                "name": "workspace"
            }]
        });

        let _result = self.request("initialize", params).await?;
        self.notify("initialized", json!({})).await?;
        Ok(())
    }

    /// notify the server about a file being opened
    pub async fn did_open(&self, path: &Path, text: &str) -> Result<(), LspError> {
        let uri = path_to_uri(path)?;
        let language_id = language_id_for_path(path);
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri.as_str(),
                    "languageId": language_id,
                    "version": 1,
                    "text": text
                }
            }),
        )
        .await
    }

    /// notify the server about a file changing (full sync)
    pub async fn did_change(&self, path: &Path, text: &str, version: i32) -> Result<(), LspError> {
        let uri = path_to_uri(path)?;
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {
                    "uri": uri.as_str(),
                    "version": version
                },
                "contentChanges": [{ "text": text }]
            }),
        )
        .await
    }

    /// get the latest published diagnostics for a file
    pub async fn get_diagnostics(&self, path: &Path) -> Result<Vec<Diagnostic>, LspError> {
        let uri = path_to_uri(path)?;
        let diags = self.diagnostics.lock().await;
        Ok(diags.get(&uri).cloned().unwrap_or_default())
    }

    /// send shutdown request and exit notification
    pub async fn shutdown(self) -> Result<(), LspError> {
        let _ = self.request("shutdown", Value::Null).await;
        let _ = self.notify("exit", Value::Null).await;
        Ok(())
    }
}

/// format diagnostics as a human-readable string
pub fn format_diagnostics(path: &Path, diagnostics: &[Diagnostic]) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    for d in diagnostics {
        let severity = match d.severity {
            Some(DiagnosticSeverity::ERROR) => "error",
            Some(DiagnosticSeverity::WARNING) => "warning",
            Some(DiagnosticSeverity::INFORMATION) => "info",
            Some(DiagnosticSeverity::HINT) => "hint",
            _ => "diagnostic",
        };
        let line = d.range.start.line + 1;
        let col = d.range.start.character + 1;
        let _ = writeln!(out, "{}:{}:{}: {}: {}", path.display(), line, col, severity, d.message);
    }
    out
}

/// spawn a background task that reads server messages and dispatches them
fn spawn_reader(
    stdout: ChildStdout,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    diagnostics: Arc<Mutex<HashMap<Uri, Vec<Diagnostic>>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            match transport::read_message(&mut reader).await {
                Ok(msg) => dispatch_message(msg, &pending, &diagnostics).await,
                Err(LspError::ServerExited) => break,
                Err(e) => {
                    tracing::warn!("LSP read error: {e}");
                    break;
                }
            }
        }
        // cancel any pending requests
        let mut guard = pending.lock().await;
        guard.clear();
    })
}

/// route an incoming message to the right handler
async fn dispatch_message(
    msg: Value,
    pending: &Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    diagnostics: &Mutex<HashMap<Uri, Vec<Diagnostic>>>,
) {
    // response to a request (has "id" but no "method")
    if let Some(id) = msg.get("id").and_then(|v| v.as_i64())
        && msg.get("method").is_none()
    {
        let mut guard = pending.lock().await;
        if let Some(tx) = guard.remove(&id) {
            let _ = tx.send(msg);
        }
        return;
    }

    // notification from the server
    if let Some(method) = msg.get("method").and_then(|v| v.as_str()) {
        match method {
            "textDocument/publishDiagnostics" => {
                if let Some(params) = msg.get("params")
                    && let Ok(diag_params) =
                        serde_json::from_value::<PublishDiagnosticsParams>(params.clone())
                {
                    let mut guard = diagnostics.lock().await;
                    guard.insert(diag_params.uri, diag_params.diagnostics);
                }
            }
            "window/logMessage" | "window/showMessage" => {
                // log but don't act on these
                if let Some(msg_text) = msg
                    .get("params")
                    .and_then(|p| p.get("message"))
                    .and_then(|m| m.as_str())
                {
                    tracing::debug!("LSP: {msg_text}");
                }
            }
            _ => {
                tracing::trace!("unhandled LSP notification: {method}");
            }
        }
    }
}

fn path_to_uri(path: &Path) -> Result<Uri, LspError> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| LspError::Transport(format!("can't resolve path: {e}")))?
            .join(path)
    };
    let uri_str = format!("file://{}", abs.display());
    Uri::from_str(&uri_str)
        .map_err(|e| LspError::Transport(format!("invalid file URI {uri_str}: {e}")))
}

fn language_id_for_path(path: &Path) -> &'static str {
    match mush_treesitter::Language::detect(path) {
        Some(mush_treesitter::Language::Rust) => "rust",
        Some(mush_treesitter::Language::Python) => "python",
        Some(mush_treesitter::Language::JavaScript) => "javascript",
        Some(mush_treesitter::Language::TypeScript | mush_treesitter::Language::Tsx) => {
            "typescript"
        }
        Some(mush_treesitter::Language::Go) => "go",
        Some(mush_treesitter::Language::C) => "c",
        Some(mush_treesitter::Language::Cpp) => "cpp",
        Some(mush_treesitter::Language::Java) => "java",
        Some(mush_treesitter::Language::Bash) => "shellscript",
        Some(mush_treesitter::Language::Nix) => "nix",
        Some(mush_treesitter::Language::Json) => "json",
        Some(mush_treesitter::Language::Toml) => "toml",
        Some(mush_treesitter::Language::Yaml) => "yaml",
        Some(mush_treesitter::Language::Markdown) => "markdown",
        Some(mush_treesitter::Language::Html) => "html",
        Some(mush_treesitter::Language::Css) => "css",
        None => "plaintext",
    }
}

#[cfg(test)]
mod tests {
    use lsp_types::{Position, Range};

    use super::*;

    #[test]
    fn format_diagnostics_empty() {
        assert!(format_diagnostics(Path::new("test.rs"), &[]).is_empty());
    }

    #[test]
    fn format_diagnostics_shows_location_and_severity() {
        let diags = vec![Diagnostic {
            range: Range {
                start: Position {
                    line: 4,
                    character: 10,
                },
                end: Position {
                    line: 4,
                    character: 15,
                },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            message: "expected `;`".into(),
            ..Default::default()
        }];

        let output = format_diagnostics(Path::new("src/main.rs"), &diags);
        assert!(output.contains("src/main.rs:5:11: error: expected `;`"));
    }

    #[test]
    fn format_diagnostics_multiple() {
        let diags = vec![
            Diagnostic {
                range: Range::default(),
                severity: Some(DiagnosticSeverity::ERROR),
                message: "first".into(),
                ..Default::default()
            },
            Diagnostic {
                range: Range::default(),
                severity: Some(DiagnosticSeverity::WARNING),
                message: "second".into(),
                ..Default::default()
            },
        ];

        let output = format_diagnostics(Path::new("lib.rs"), &diags);
        assert!(output.contains("error: first"));
        assert!(output.contains("warning: second"));
    }

    #[test]
    fn language_id_detection() {
        assert_eq!(language_id_for_path(Path::new("main.rs")), "rust");
        assert_eq!(language_id_for_path(Path::new("app.py")), "python");
        assert_eq!(language_id_for_path(Path::new("index.ts")), "typescript");
        assert_eq!(language_id_for_path(Path::new("unknown.xyz")), "plaintext");
    }

    #[test]
    fn path_to_uri_absolute() {
        let uri = path_to_uri(Path::new("/tmp/test.rs")).unwrap();
        assert!(uri.as_str().starts_with("file://"));
        assert!(uri.as_str().contains("/tmp/test.rs"));
    }

    #[tokio::test]
    async fn dispatch_diagnostics_notification() {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let diagnostics = Arc::new(Mutex::new(HashMap::new()));

        let msg = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///tmp/test.rs",
                "diagnostics": [{
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 5}
                    },
                    "severity": 1,
                    "message": "type error"
                }]
            }
        });

        dispatch_message(msg, &pending, &diagnostics).await;

        let guard = diagnostics.lock().await;
        let uri: Uri = "file:///tmp/test.rs".parse().unwrap();
        let diags = guard.get(&uri).unwrap();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].message, "type error");
    }

    #[tokio::test]
    async fn dispatch_response_resolves_pending() {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let diagnostics = Arc::new(Mutex::new(HashMap::new()));

        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert(42, tx);

        let msg = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {"capabilities": {}}
        });

        dispatch_message(msg, &pending, &diagnostics).await;

        let response = rx.await.unwrap();
        assert!(response.get("result").is_some());
    }
}
