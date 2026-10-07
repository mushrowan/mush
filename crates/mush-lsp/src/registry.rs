use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};

use lsp_types::Diagnostic;
use mush_treesitter::Language;
use tokio::sync::Mutex;

use crate::client::LspClient;
use crate::discovery::{self, ServerConfig};
use crate::error::LspError;

const DIAGNOSTIC_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

pub struct LspRegistry {
    root: PathBuf,
    clients: Mutex<HashMap<Language, LspClient>>,
    overrides: HashMap<Language, ServerConfig>,
}

impl LspRegistry {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            clients: Mutex::new(HashMap::new()),
            overrides: HashMap::new(),
        }
    }

    pub fn add_override(&mut self, config: ServerConfig) {
        self.overrides.insert(config.language, config);
    }

    pub async fn diagnostics_for_file(&self, path: &Path) -> Result<Vec<Diagnostic>, LspError> {
        let language = Language::detect(path)
            .ok_or_else(|| LspError::NoServer(format!("unknown language: {}", path.display())))?;

        let text = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| LspError::Transport(format!("can't read {}: {e}", path.display())))?;

        let mut clients = self.clients.lock().await;
        let client = self.ensure_client_locked(&mut clients, language).await?;

        let send_result = client.did_open(path, &text).await;
        evict_if_exited(&mut clients, language, send_result)?;
        drop(clients);

        self.wait_and_get_diagnostics(language, path).await
    }

    pub async fn notify_and_diagnose(
        &self,
        path: &Path,
        text: &str,
    ) -> Result<Vec<Diagnostic>, LspError> {
        let Some(language) = Language::detect(path) else {
            return Ok(vec![]);
        };

        let mut clients = self.clients.lock().await;
        let Some(client) = clients.get(&language) else {
            return Ok(vec![]);
        };

        let send_result = client.did_change(path, text, 2).await;
        evict_if_exited(&mut clients, language, send_result)?;
        drop(clients);

        self.wait_and_get_diagnostics(language, path).await
    }

    pub async fn has_server_for(&self, language: Language) -> bool {
        self.clients.lock().await.contains_key(&language)
    }

    pub async fn shutdown_all(self) {
        for (lang, client) in self.clients.into_inner() {
            if let Err(e) = client.shutdown().await {
                tracing::warn!("failed to shut down {lang:?} LSP server: {e}");
            }
        }
    }

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

    async fn ensure_client_locked<'a>(
        &self,
        clients: &'a mut HashMap<Language, LspClient>,
        language: Language,
    ) -> Result<&'a LspClient, LspError> {
        match clients.entry(language) {
            Entry::Occupied(entry) => Ok(entry.into_mut()),
            Entry::Vacant(entry) => {
                let config = self
                    .overrides
                    .get(&language)
                    .cloned()
                    .or_else(|| discovery::discover_for_language(language))
                    .ok_or_else(|| LspError::NoServer(format!("{language:?}")))?;

                tracing::info!(language = ?language, command = %config.command, "starting LSP server");
                let client = LspClient::start(&config, &self.root).await?;
                Ok(entry.insert(client))
            }
        }
    }
}

fn evict_if_exited(
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
