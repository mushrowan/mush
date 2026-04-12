//! LSP server registry: one client per language, lazily spawned
//!
//! the registry maps languages to LSP clients. servers are only
//! started when a file of that language is first touched.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_types::Diagnostic;
use mush_treesitter::Language;
use tokio::sync::Mutex;

use crate::client::LspClient;
use crate::discovery::{self, ServerConfig};
use crate::error::LspError;

/// how long to wait for the server to publish diagnostics after a change
const DIAGNOSTIC_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// manages LSP server lifecycles, one per language
pub struct LspRegistry {
    /// workspace root for all servers
    root: PathBuf,
    /// active clients keyed by language
    clients: Arc<Mutex<HashMap<Language, LspClient>>>,
    /// user-provided server config overrides (language -> config)
    overrides: HashMap<Language, ServerConfig>,
}

impl LspRegistry {
    /// create a new registry rooted at the given workspace directory
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            clients: Arc::new(Mutex::new(HashMap::new())),
            overrides: HashMap::new(),
        }
    }

    /// add a user-configured server override for a language
    pub fn add_override(&mut self, config: ServerConfig) {
        self.overrides.insert(config.language, config);
    }

    /// get diagnostics for a file, starting the LSP server if needed.
    /// opens the file in the server, waits briefly for diagnostics,
    /// then returns them. restarts the server on crash.
    pub async fn diagnostics_for_file(&self, path: &Path) -> Result<Vec<Diagnostic>, LspError> {
        let language = Language::detect(path)
            .ok_or_else(|| LspError::NoServer(format!("unknown language: {}", path.display())))?;

        let text = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| LspError::Transport(format!("can't read {}: {e}", path.display())))?;

        let mut clients = self.clients.lock().await;
        let client = self.ensure_client_locked(&mut clients, language).await?;

        let send_result = client.did_open(path, &text).await;
        self.send_or_evict(&mut clients, language, send_result)?;
        drop(clients);

        self.wait_and_get_diagnostics(language, path).await
    }

    /// notify a file changed and return updated diagnostics.
    /// if no server is running for this language, does nothing.
    /// removes crashed servers so they restart on next use.
    pub async fn notify_and_diagnose(
        &self,
        path: &Path,
        text: &str,
    ) -> Result<Vec<Diagnostic>, LspError> {
        let language = match Language::detect(path) {
            Some(l) => l,
            None => return Ok(vec![]),
        };

        let mut clients = self.clients.lock().await;
        let Some(client) = clients.get(&language) else {
            return Ok(vec![]);
        };

        let send_result = client.did_change(path, text, 2).await;
        self.send_or_evict(&mut clients, language, send_result)?;
        drop(clients);

        self.wait_and_get_diagnostics(language, path).await
    }

    /// check if a server is running for a language
    pub async fn has_server_for(&self, language: Language) -> bool {
        self.clients.lock().await.contains_key(&language)
    }

    /// shut down all active LSP servers
    pub async fn shutdown_all(self) {
        let mut clients = self.clients.lock().await;
        for (lang, client) in clients.drain() {
            if let Err(e) = client.shutdown().await {
                tracing::warn!("failed to shut down {lang:?} LSP server: {e}");
            }
        }
    }

    /// wait for diagnostics to be published, then retrieve them
    async fn wait_and_get_diagnostics(
        &self,
        language: Language,
        path: &Path,
    ) -> Result<Vec<Diagnostic>, LspError> {
        tokio::time::sleep(DIAGNOSTIC_WAIT).await;

        let clients = self.clients.lock().await;
        match clients.get(&language) {
            Some(client) => client.get_diagnostics(path).await,
            None => Ok(vec![]),
        }
    }

    /// propagate send result, evicting crashed servers
    fn send_or_evict(
        &self,
        clients: &mut HashMap<Language, LspClient>,
        language: Language,
        result: Result<(), LspError>,
    ) -> Result<(), LspError> {
        if let Err(ref e @ LspError::ServerExited) = result {
            tracing::warn!(language = ?language, error = %e, "LSP server crashed, removing");
            clients.remove(&language);
        }
        result
    }

    /// ensure an LSP client exists for the given language.
    /// caller must hold the mutex lock.
    async fn ensure_client_locked<'a>(
        &self,
        clients: &'a mut HashMap<Language, LspClient>,
        language: Language,
    ) -> Result<&'a LspClient, LspError> {
        use std::collections::hash_map::Entry;

        if let Entry::Vacant(entry) = clients.entry(language) {
            let config = self
                .overrides
                .get(&language)
                .cloned()
                .or_else(|| discovery::discover_for_language(language))
                .ok_or_else(|| LspError::NoServer(format!("{language:?}")))?;

            tracing::info!(language = ?language, command = %config.command, "starting LSP server");
            let client = LspClient::start(&config, &self.root).await?;
            entry.insert(client);
        }

        #[expect(clippy::expect_used, reason = "entry was just inserted above")]
        Ok(clients.get(&language).expect("just ensured"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_new() {
        let reg = LspRegistry::new(PathBuf::from("/tmp"));
        assert_eq!(reg.root, PathBuf::from("/tmp"));
    }

    #[test]
    fn add_override() {
        let mut reg = LspRegistry::new(PathBuf::from("/tmp"));
        reg.add_override(ServerConfig {
            language: Language::Rust,
            command: "my-rust-analyzer".into(),
            args: vec![],
        });
        assert!(reg.overrides.contains_key(&Language::Rust));
    }

    #[tokio::test]
    async fn has_server_for_initially_false() {
        let reg = LspRegistry::new(PathBuf::from("/tmp"));
        assert!(!reg.has_server_for(Language::Rust).await);
    }
}
