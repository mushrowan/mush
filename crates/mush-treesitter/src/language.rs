//! language detection from file extensions and names

use std::path::Path;

/// languages supported by the tree-sitter integration
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    C,
    Cpp,
    Java,
    Bash,
    Json,
    Toml,
    Yaml,
    Markdown,
    Html,
    Css,
    Nix,
}

/// all supported languages
pub const ALL: &[Language] = &[
    Language::Rust,
    Language::Python,
    Language::JavaScript,
    Language::TypeScript,
    Language::Tsx,
    Language::Go,
    Language::C,
    Language::Cpp,
    Language::Java,
    Language::Bash,
    Language::Json,
    Language::Toml,
    Language::Yaml,
    Language::Markdown,
    Language::Html,
    Language::Css,
    Language::Nix,
];

/// languages with meaningful symbol extraction (not data/markup formats)
pub const CODE: &[Language] = &[
    Language::Rust,
    Language::Python,
    Language::JavaScript,
    Language::TypeScript,
    Language::Tsx,
    Language::Go,
    Language::C,
    Language::Cpp,
    Language::Java,
    Language::Bash,
    Language::Nix,
];

/// data and markup formats (no symbol extraction)
pub const DATA: &[Language] = &[
    Language::Json,
    Language::Toml,
    Language::Yaml,
    Language::Markdown,
    Language::Html,
    Language::Css,
];

impl Language {
    /// detect language from a file path (extension + filename heuristics)
    pub fn detect(path: &Path) -> Option<Self> {
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && let Some(lang) = Self::from_filename(name)
        {
            return Some(lang);
        }

        let ext = path.extension()?.to_str()?;
        Self::from_extension(ext)
    }

    /// map a file extension to a language
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "py" | "pyi" | "pyw" => Some(Self::Python),
            "js" | "mjs" | "cjs" | "jsx" => Some(Self::JavaScript),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "go" => Some(Self::Go),
            "c" | "h" => Some(Self::C),
            "cc" | "cpp" | "cxx" | "hpp" | "hxx" | "hh" | "h++" | "c++" => Some(Self::Cpp),
            "java" => Some(Self::Java),
            "sh" | "bash" | "zsh" => Some(Self::Bash),
            "json" | "jsonc" | "json5" => Some(Self::Json),
            "toml" => Some(Self::Toml),
            "yml" | "yaml" => Some(Self::Yaml),
            "md" | "markdown" | "mdx" => Some(Self::Markdown),
            "html" | "htm" | "xhtml" => Some(Self::Html),
            "css" | "scss" => Some(Self::Css),
            "nix" => Some(Self::Nix),
            _ => None,
        }
    }

    /// map well-known filenames to a language
    fn from_filename(name: &str) -> Option<Self> {
        match name {
            "Makefile" | "makefile" | "GNUmakefile" => Some(Self::Bash),
            "Dockerfile" => Some(Self::Bash),
            "Justfile" | "justfile" => Some(Self::Bash),
            ".bashrc" | ".bash_profile" | ".bash_logout" | ".profile" | ".zshrc" | ".zshenv"
            | ".zprofile" => Some(Self::Bash),
            "flake.lock" => Some(Self::Json),
            _ => None,
        }
    }

    /// get the tree-sitter language function, if the grammar is compiled in
    pub fn tree_sitter_language(self) -> Option<tree_sitter::Language> {
        match self {
            #[cfg(feature = "lang-rust")]
            Self::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
            #[cfg(feature = "lang-python")]
            Self::Python => Some(tree_sitter_python::LANGUAGE.into()),
            #[cfg(feature = "lang-javascript")]
            Self::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
            #[cfg(feature = "lang-typescript")]
            Self::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
            #[cfg(feature = "lang-typescript")]
            Self::Tsx => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
            #[cfg(feature = "lang-go")]
            Self::Go => Some(tree_sitter_go::LANGUAGE.into()),
            #[cfg(feature = "lang-c")]
            Self::C => Some(tree_sitter_c::LANGUAGE.into()),
            #[cfg(feature = "lang-cpp")]
            Self::Cpp => Some(tree_sitter_cpp::LANGUAGE.into()),
            #[cfg(feature = "lang-java")]
            Self::Java => Some(tree_sitter_java::LANGUAGE.into()),
            #[cfg(feature = "lang-bash")]
            Self::Bash => Some(tree_sitter_bash::LANGUAGE.into()),
            #[cfg(feature = "lang-json")]
            Self::Json => Some(tree_sitter_json::LANGUAGE.into()),
            #[cfg(feature = "lang-toml")]
            Self::Toml => Some(tree_sitter_toml_ng::LANGUAGE.into()),
            #[cfg(feature = "lang-yaml")]
            Self::Yaml => Some(tree_sitter_yaml::LANGUAGE.into()),
            #[cfg(feature = "lang-markdown")]
            Self::Markdown => Some(tree_sitter_md::LANGUAGE.into()),
            #[cfg(feature = "lang-html")]
            Self::Html => Some(tree_sitter_html::LANGUAGE.into()),
            #[cfg(feature = "lang-css")]
            Self::Css => Some(tree_sitter_css::LANGUAGE.into()),
            #[cfg(feature = "lang-nix")]
            Self::Nix => Some(tree_sitter_nix::LANGUAGE.into()),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// human-readable name
    pub fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::Go => "go",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Java => "java",
            Self::Bash => "bash",
            Self::Json => "json",
            Self::Toml => "toml",
            Self::Yaml => "yaml",
            Self::Markdown => "markdown",
            Self::Html => "html",
            Self::Css => "css",
            Self::Nix => "nix",
        }
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_from_extension() {
        assert_eq!(Language::detect(Path::new("main.rs")), Some(Language::Rust));
        assert_eq!(
            Language::detect(Path::new("app.py")),
            Some(Language::Python)
        );
        assert_eq!(
            Language::detect(Path::new("index.tsx")),
            Some(Language::Tsx)
        );
        assert_eq!(
            Language::detect(Path::new("index.ts")),
            Some(Language::TypeScript)
        );
        assert_eq!(Language::detect(Path::new("main.go")), Some(Language::Go));
        assert_eq!(
            Language::detect(Path::new("config.toml")),
            Some(Language::Toml)
        );
        assert_eq!(
            Language::detect(Path::new("flake.nix")),
            Some(Language::Nix)
        );
    }

    #[test]
    fn detect_from_filename() {
        assert_eq!(
            Language::detect(Path::new("Makefile")),
            Some(Language::Bash)
        );
        assert_eq!(Language::detect(Path::new(".bashrc")), Some(Language::Bash));
        assert_eq!(
            Language::detect(Path::new("flake.lock")),
            Some(Language::Json)
        );
    }

    #[test]
    fn detect_returns_none_for_unknown() {
        assert_eq!(Language::detect(Path::new("photo.png")), None);
        assert_eq!(Language::detect(Path::new("no_extension")), None);
    }

    #[test]
    fn c_vs_cpp_header_disambiguation() {
        // .h defaults to C
        assert_eq!(Language::detect(Path::new("foo.h")), Some(Language::C));
        assert_eq!(Language::detect(Path::new("foo.hpp")), Some(Language::Cpp));
    }

    #[test]
    fn all_languages_have_names() {
        for lang in ALL {
            assert!(!lang.name().is_empty());
            assert!(!lang.to_string().is_empty());
        }
    }

    #[cfg(feature = "all-languages")]
    #[test]
    fn all_languages_have_grammars() {
        for lang in ALL {
            assert!(
                lang.tree_sitter_language().is_some(),
                "{lang} grammar missing"
            );
        }
    }
}
