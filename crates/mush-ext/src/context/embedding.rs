//! embedding model creation and configuration

use std::path::PathBuf;

use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};

use super::ContextError;

/// shared model cache under the mush data dir
fn model_cache_dir() -> PathBuf {
    mush_session::data_dir().join("models")
}

/// which local embedding model to use
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingModelChoice {
    /// nomic CodeRankEmbed-137M, 768-dim, code-specialised
    /// INT8 ONNX from mrsladoje/CodeRankEmbed-onnx-int8 (132MB)
    CodeRankEmbed,
    /// google EmbeddingGemma-300M, 768-dim, general purpose
    /// built-in fastembed model (~274MB)
    Gemma300M,
}

impl Default for EmbeddingModelChoice {
    fn default() -> Self {
        Self::CodeRankEmbed
    }
}

impl EmbeddingModelChoice {
    /// apply model-specific query prefix for search
    pub(super) fn prefix_query(&self, query: &str) -> String {
        match self {
            Self::CodeRankEmbed => {
                format!("Represent this query for searching relevant code: {query}")
            }
            Self::Gemma300M => query.to_string(),
        }
    }
}

/// create a TextEmbedding from the chosen model
pub(super) fn create_embedding_model(
    choice: EmbeddingModelChoice,
) -> Result<TextEmbedding, ContextError> {
    match choice {
        EmbeddingModelChoice::CodeRankEmbed => create_coderank_model(),
        EmbeddingModelChoice::Gemma300M => create_gemma_model(),
    }
}

/// load CodeRankEmbed INT8 ONNX via hf-hub download + UserDefinedEmbeddingModel
fn create_coderank_model() -> Result<TextEmbedding, ContextError> {
    let api = hf_hub::api::sync::ApiBuilder::new()
        .with_cache_dir(model_cache_dir())
        .build()
        .map_err(|e| ContextError::Download(e.to_string()))?;

    let repo = api.model("mrsladoje/CodeRankEmbed-onnx-int8".to_string());
    let fetch = |name: &str| -> Result<Vec<u8>, ContextError> {
        let path = repo
            .get(name)
            .map_err(|e| ContextError::Download(format!("{name}: {e}")))?;
        std::fs::read(&path)
            .map_err(|e| ContextError::Download(format!("read {}: {e}", path.display())))
    };

    let user_model = UserDefinedEmbeddingModel::new(
        fetch("onnx/model.onnx")?,
        TokenizerFiles {
            tokenizer_file: fetch("tokenizer.json")?,
            config_file: fetch("config.json")?,
            special_tokens_map_file: fetch("special_tokens_map.json")?,
            tokenizer_config_file: fetch("tokenizer_config.json")?,
        },
    )
    .with_pooling(Pooling::Mean);

    TextEmbedding::try_new_from_user_defined(user_model, InitOptionsUserDefined::new())
        .map_err(|e| ContextError::ModelInit(e.to_string()))
}

/// load built-in EmbeddingGemma-300M via fastembed
fn create_gemma_model() -> Result<TextEmbedding, ContextError> {
    use fastembed::{EmbeddingModel, InitOptions};

    TextEmbedding::try_new(
        InitOptions::new(EmbeddingModel::EmbeddingGemma300M)
            .with_cache_dir(model_cache_dir())
            .with_show_download_progress(true),
    )
    .map_err(|e| ContextError::ModelInit(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_model_is_coderank() {
        assert_eq!(
            EmbeddingModelChoice::default(),
            EmbeddingModelChoice::CodeRankEmbed
        );
    }

    #[test]
    fn coderank_prefixes_query() {
        let result = EmbeddingModelChoice::CodeRankEmbed.prefix_query("find the auth module");
        assert!(result.starts_with("Represent this query"));
        assert!(result.ends_with("find the auth module"));
    }

    #[test]
    fn gemma_passes_query_through() {
        let result = EmbeddingModelChoice::Gemma300M.prefix_query("find the auth module");
        assert_eq!(result, "find the auth module");
    }
}
