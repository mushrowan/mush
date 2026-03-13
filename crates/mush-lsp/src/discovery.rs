//! auto-detect installed LSP servers
//!
//! checks `$PATH` for common language server binaries and maps
//! them to languages via mush-treesitter's `Language` enum.

use std::path::PathBuf;

use mush_treesitter::Language;

/// an LSP server that can be spawned
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// language this server handles
    pub language: Language,
    /// command to run (resolved from PATH or absolute)
    pub command: String,
    /// command line arguments
    pub args: Vec<String>,
}

/// well-known LSP server binaries and their languages
const KNOWN_SERVERS: &[(&str, Language, &[&str])] = &[
    ("rust-analyzer", Language::Rust, &[]),
    ("pyright-langserver", Language::Python, &["--stdio"]),
    ("pylsp", Language::Python, &[]),
    ("typescript-language-server", Language::TypeScript, &["--stdio"]),
    ("tsserver", Language::TypeScript, &[]),
    ("gopls", Language::Go, &[]),
    ("clangd", Language::C, &[]),
    ("nil", Language::Nix, &[]),
    ("nixd", Language::Nix, &[]),
    ("jdtls", Language::Java, &[]),
    ("bash-language-server", Language::Bash, &["start"]),
];

/// discover available LSP servers by searching PATH
pub fn discover() -> Vec<ServerConfig> {
    let dirs = path_dirs();
    let mut found: Vec<ServerConfig> = Vec::new();
    let mut seen_languages = std::collections::HashSet::new();

    for &(binary, language, args) in KNOWN_SERVERS {
        if seen_languages.contains(&language) {
            continue;
        }
        if is_in_path(binary, &dirs) {
            seen_languages.insert(language);
            found.push(ServerConfig::from_known(binary, language, args));
        }
    }

    found
}

/// discover a server for a specific language
pub fn discover_for_language(language: Language) -> Option<ServerConfig> {
    let dirs = path_dirs();

    KNOWN_SERVERS
        .iter()
        .find(|&&(binary, lang, _)| lang == language && is_in_path(binary, &dirs))
        .map(|&(binary, lang, args)| ServerConfig::from_known(binary, lang, args))
}

impl ServerConfig {
    fn from_known(binary: &str, language: Language, args: &[&str]) -> Self {
        Self {
            language,
            command: binary.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }
}

fn path_dirs() -> Vec<PathBuf> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    std::env::split_paths(&path_var).collect()
}

fn is_in_path(binary: &str, dirs: &[PathBuf]) -> bool {
    dirs.iter().any(|dir| dir.join(binary).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_servers_first_per_language_wins() {
        // rust-analyzer should come before any other rust server
        let rust_entries: Vec<_> = KNOWN_SERVERS
            .iter()
            .filter(|(_, lang, _)| *lang == Language::Rust)
            .collect();
        assert_eq!(rust_entries[0].0, "rust-analyzer");
    }

    #[test]
    fn discover_returns_at_most_one_per_language() {
        let servers = discover();
        let mut seen = std::collections::HashSet::new();
        for s in &servers {
            assert!(
                seen.insert(s.language),
                "duplicate language: {:?}",
                s.language
            );
        }
    }

    #[test]
    fn is_in_path_works() {
        let dirs = vec![PathBuf::from("/usr/bin")];
        // can't assume which binaries exist, but shouldn't panic
        let _ = is_in_path("sh", &dirs);
    }
}
