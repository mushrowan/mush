//! shared setup helpers for both print and TUI modes

use std::sync::Arc;

use color_eyre::eyre::{Result, eyre};
use mush_ai::providers;
use mush_ai::registry::ApiRegistry;
use mush_ai::types::*;
use mush_ext::loader;
use mush_session::compact;

use crate::config;

/// shared state built from CLI args + config, used by both print and TUI modes
pub struct AppSetup {
    pub cfg: config::Config,
    pub model: Model,
    /// separate model + options for compaction (None = use active model)
    pub compaction_model: Option<(Model, StreamOptions)>,
    pub registry: ApiRegistry,
    pub cwd: std::path::PathBuf,
    pub system_prompt: String,
    pub options: StreamOptions,
    pub thinking_prefs: mush_tui::ThinkingPrefs,
    pub tools: mush_agent::tool::ToolRegistry,
    pub debug_cache: bool,
    pub max_turns: usize,
    pub lifecycle_hooks: mush_agent::LifecycleHooks,
    /// file-triggered rule injection callback
    pub file_rules: Option<mush_agent::FileRuleCallback>,
    /// LSP diagnostic injection callback
    pub lsp_diagnostics: Option<mush_agent::DiagnosticCallback>,
    /// shared http client for all network calls
    pub http_client: mush_ai::http::HttpClient,
}

/// CLI args needed for shared setup (avoids depending on clap struct)
pub struct SetupArgs {
    pub model: Option<String>,
    pub thinking: bool,
    pub max_tokens: Option<u64>,
    pub max_turns: Option<usize>,
    pub system: Option<String>,
    pub no_tools: bool,
    pub debug_cache: bool,
    pub output_sink: Option<mush_tools::bash::OutputSink>,
    pub timer: Option<crate::timing::PhaseTimer>,
}

impl AppSetup {
    /// build shared state from CLI args
    #[tracing::instrument(name = "startup", skip_all)]
    pub async fn init(args: SetupArgs) -> Result<(Self, Option<crate::timing::StartupReport>)> {
        let mut timer = args.timer;

        let cfg = config::load_config();
        let debug_cache = args.debug_cache || cfg.debug_cache;
        if let Some(t) = &mut timer {
            t.phase("config");
        }

        // canonicalise so the same project resolves to the same directory
        // key across symlinks and relative paths. falls through to the raw
        // cwd on filesystems that refuse the lookup
        let cwd = mush_tui::canonical_dir(&std::env::current_dir()?);

        let model_id = args.model.unwrap_or_else(|| default_model_id(&cfg, &cwd));
        let model = mush_ai::discovery::resolve_model_by_id(&model_id).ok_or_else(|| {
            eyre!(
                "unknown model: {model_id}\n\navailable models:\n{}",
                list_models_short()
            )
        })?;

        #[expect(
            clippy::expect_used,
            reason = "reqwest client with default TLS should never fail to build"
        )]
        let http_client = mush_ai::http::wrap(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::limited(10))
                // give the network 60s to establish a tcp + tls connection.
                // reqwest defaults to no connect timeout, which lets stalled
                // proxies hang the agent indefinitely. retry/backoff lives in
                // the agent loop, so a per-attempt cap is enough here. no
                // overall request timeout: streaming responses can take many
                // minutes
                .connect_timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("failed to build http client"),
        );

        let mut registry = ApiRegistry::new();
        providers::register_builtins(&mut registry, http_client.clone());
        if let Some(t) = &mut timer {
            t.phase("model + http");
        }

        let mut tools = init_builtin_tools(
            &model,
            &cwd,
            args.output_sink,
            args.no_tools,
            &http_client,
            mush_tools::forbidden_chars::ForbiddenChars::new(cfg.forbidden_chars.iter().copied()),
            cfg.max_image_mb,
        );
        if let Some(t) = &mut timer {
            t.phase("builtin tools");
        }

        init_mcp(&cfg, args.no_tools, &mut tools).await;
        if let Some(t) = &mut timer {
            t.phase("mcp");
        }

        let project_context = loader::discover_project_context(&cwd);
        init_skill_tools(&project_context, args.no_tools, &mut tools);
        if let Some(t) = &mut timer {
            t.phase("project context");
        }

        let system_prompt = args
            .system
            .or(cfg.system_prompt.clone())
            .unwrap_or_else(|| build_system_prompt_from_context(&project_context, &cwd));
        if let Some(t) = &mut timer {
            t.phase("system prompt");
        }

        let thinking_prefs = config::load_thinking_prefs();
        let thinking_level = resolve_thinking(args.thinking, &model, &thinking_prefs, &cwd, &cfg);
        let mut options = stream_options_from_config(&cfg, thinking_level, args.max_tokens);
        resolve_api_key(&mut options, &model, &cfg).await;

        let compaction_model = init_compaction_model(&cfg, &model, &options).await;
        if let Some(t) = &mut timer {
            t.phase("api keys");
        }

        let max_turns = args
            .max_turns
            .or(cfg.max_turns)
            .unwrap_or(mush_agent::DEFAULT_MAX_TURNS);
        let lifecycle_hooks = cfg.lifecycle_hooks();

        init_repo_map(&cfg, &cwd, args.no_tools, &mut tools);
        if let Some(t) = &mut timer {
            t.phase("repo map");
        }

        let file_rules = build_file_rules(&cwd);
        let lsp_diagnostics = init_lsp(&cfg, &cwd, args.no_tools, &mut tools);
        if let Some(t) = &mut timer {
            t.phase("lsp + rules");
        }

        if !args.no_tools && model.supports_native_parallel_calls() {
            // skip batch tool
        } else if !args.no_tools {
            mush_tools::add_batch_tool(&mut tools);
        }

        let report = timer.map(|t| t.finish());

        Ok((
            Self {
                cfg,
                model,
                compaction_model,
                registry,
                cwd,
                system_prompt,
                options,
                thinking_prefs,
                tools,
                debug_cache,
                max_turns,
                lifecycle_hooks,
                file_rules,
                lsp_diagnostics,
                http_client,
            },
            report,
        ))
    }
}

fn init_builtin_tools(
    model: &Model,
    cwd: &std::path::Path,
    output_sink: Option<mush_tools::bash::OutputSink>,
    no_tools: bool,
    http_client: &mush_ai::http::HttpClient,
    forbidden_chars: mush_tools::forbidden_chars::ForbiddenChars,
    max_image_mb: u32,
) -> mush_agent::tool::ToolRegistry {
    if no_tools {
        return mush_agent::tool::ToolRegistry::new();
    }
    mush_tools::builtin_tools_with_options(
        Arc::from(cwd),
        output_sink,
        model.uses_patch_tool(),
        http_client.clone(),
        forbidden_chars,
        Some(max_image_mb as usize * 1024 * 1024),
    )
}

async fn init_mcp(
    cfg: &config::Config,
    no_tools: bool,
    tools: &mut mush_agent::tool::ToolRegistry,
) {
    if no_tools || cfg.mcp.is_empty() {
        return;
    }

    let (mcp_manager, mcp_tools) = mush_mcp::McpManager::connect_all(&cfg.mcp).await;
    if cfg.dynamic_mcp {
        let conns = mcp_manager.connections();
        if !conns.is_empty() {
            let dynamic = mush_mcp::dynamic::dynamic_mcp_tools(&conns);
            tools.extend_shared(dynamic.iter().cloned());
        }
    } else {
        tools.extend_shared(mcp_tools.iter().cloned());
    }
}

fn init_skill_tools(
    project_context: &loader::ProjectContext,
    no_tools: bool,
    tools: &mut mush_agent::tool::ToolRegistry,
) {
    if no_tools || project_context.skills.is_empty() {
        return;
    }
    let skill_infos: Vec<mush_tools::skills::SkillInfo> = project_context
        .skills
        .iter()
        .map(|s| mush_tools::skills::SkillInfo {
            name: s.name.clone(),
            description: s.description.clone(),
            path: s.path.clone(),
        })
        .collect();
    let skill_tools = mush_tools::skills::skill_tools(skill_infos);
    tools.extend_shared(skill_tools.iter().cloned());
}

async fn init_compaction_model(
    cfg: &config::Config,
    model: &Model,
    options: &StreamOptions,
) -> Option<(Model, StreamOptions)> {
    let id = cfg.compaction_model.as_ref()?;
    let m = mush_ai::discovery::resolve_model_by_id(id).unwrap_or_else(|| {
        eprintln!(
            "\x1b[33mwarning: unknown compaction_model '{id}', falling back to active model\x1b[0m"
        );
        model.clone()
    });
    let mut compact_opts = options.clone();
    if m.provider != model.provider {
        compact_opts.api_key = None;
        compact_opts.account_id = None;
    }
    resolve_api_key(&mut compact_opts, &m, cfg).await;
    Some((m, compact_opts))
}

fn init_repo_map(
    cfg: &config::Config,
    cwd: &std::path::Path,
    no_tools: bool,
    tools: &mut mush_agent::tool::ToolRegistry,
) {
    if !cfg.repo_map.enabled || no_tools || !mush_treesitter::is_inside_repo(cwd) {
        return;
    }

    let shared = mush_tools::repo_map::RepoMapShared::lazy(cwd.to_path_buf(), cfg.repo_map.budget);
    tools.register_shared(std::sync::Arc::new(mush_tools::repo_map::RepoMapTool::new(
        shared,
    )));
}

fn init_lsp(
    cfg: &config::Config,
    cwd: &std::path::Path,
    no_tools: bool,
    tools: &mut mush_agent::tool::ToolRegistry,
) -> Option<mush_agent::DiagnosticCallback> {
    let (lsp_diagnostics, lsp_reg) = build_lsp_diagnostics(cwd, cfg);
    if let Some(reg) = &lsp_reg
        && !no_tools
    {
        let lsp_tool_list = mush_lsp::lsp_tools(reg.clone(), cwd.to_path_buf());
        for tool in lsp_tool_list {
            tools.register_shared(std::sync::Arc::from(tool));
        }
    }
    lsp_diagnostics
}

/// expand /template_name args... into template content
pub fn expand_template(prompt: &str) -> String {
    if !prompt.starts_with('/') {
        return prompt.to_string();
    }

    let cwd = std::env::current_dir().unwrap_or_default();
    let templates = mush_ext::discover_templates(&cwd);

    let parts: Vec<&str> = prompt[1..].splitn(2, ' ').collect();
    let name = parts[0];
    let args_str = parts.get(1).unwrap_or(&"");

    if let Some(tmpl) = mush_ext::find_template(&templates, name) {
        let args: Vec<&str> = if args_str.is_empty() {
            vec![]
        } else {
            args_str.split_whitespace().collect()
        };
        mush_ext::substitute_args(&tmpl.content, &args)
    } else {
        prompt.to_string()
    }
}

/// enrich error messages with actionable suggestions
pub fn format_error(error: &str) -> String {
    if error.contains("missing api key") {
        format!(
            "{error}\n\n\
             hint: set ANTHROPIC_API_KEY, OPENROUTER_API_KEY, or OPENAI_API_KEY, or run:\n  \
             mush login anthropic\n  \
             mush login openai-codex\n  \
             mush config  (to add api_keys in config.toml)"
        )
    } else if error.contains("no provider registered") {
        format!("{error}\n\nhint: the model's api type has no registered provider")
    } else {
        error.to_string()
    }
}

fn format_available_skills(skills: &[loader::Skill]) -> String {
    let mut skills = skills.to_vec();
    skills.sort_by(|left, right| left.name.cmp(&right.name));

    let mut out = String::from("<available_skills>\n");
    for skill in skills {
        out.push_str("  <skill>\n");
        out.push_str(&format!("    <name>{}</name>\n", skill.name));
        out.push_str(&format!(
            "    <description>{}</description>\n",
            skill.description
        ));
        out.push_str(&format!(
            "    <location>{}</location>\n",
            skill.path.display()
        ));
        out.push_str("  </skill>\n");
    }
    out.push_str("</available_skills>");
    out
}

pub fn build_system_prompt_from_context(
    context: &loader::ProjectContext,
    cwd: &std::path::Path,
) -> String {
    let cwd_str = cwd.display();
    let mut prompt = format!(
        "You are running inside mush, a minimal coding agent harness. \
         You help users by reading files, executing commands, \
         editing code, and writing new files.\n\n\
         Current working directory: {cwd_str}\n\n\
         Guidelines:\n\
         - Use bash for file operations like ls, grep, find\n\
         - Use read to examine files before editing\n\
         - Use edit for precise changes (old text must match exactly)\n\
         - Use write only for new files or complete rewrites\n\
         - Be concise in your responses\n\
         - Your final answers are plain text that will be styled by the terminal TUI. \
         Use Markdown-compatible basics for readability: short paragraphs, simple lists, \
         inline backticks for paths and commands, and fenced code blocks for snippets. \
         Avoid HTML, ANSI codes, and complex nested formatting\n\
         - Explore with targeted tools first: prefer grep, find, or glob to locate \
         relevant code before reading. When reading large files, use offset/limit, \
         start_line/end_line, or around_line to fetch only the sections you need \
         instead of dumping the whole file\n\
         - Batch independent operations: when you need to read, edit, or operate on \
         multiple files that don't depend on each other, use the Batch tool to do them \
         all in a single call. This saves round-trips and helps you reason about \
         related changes holistically before making them"
    );

    for agents in &context.agents_md {
        prompt.push_str("\n\n# Project Context\n\n");
        prompt.push_str(&agents.content);
    }

    if !context.skills.is_empty() {
        prompt.push_str(
            "\n\nSkills provide specialised instructions for specific tasks. \
             Use the skill tool to load a skill when a task matches its description. \
             If the user explicitly names a skill with `$skill-name`, load that skill before answering.\n\n",
        );
        prompt.push_str(&format_available_skills(&context.skills));
    }

    prompt
}

fn format_nested_agents(cwd: &std::path::Path, agents: &[loader::AgentsMd]) -> Option<String> {
    if agents.is_empty() {
        return None;
    }

    let text: Vec<_> = agents
        .iter()
        .map(|agents| {
            let path = agents.path.strip_prefix(cwd).unwrap_or(&agents.path);
            format!("## {}\n{}", path.display(), agents.content.trim())
        })
        .collect();
    Some(text.join("\n\n"))
}

/// build file context callback from nested AGENTS.md and `.mush/rules/`
fn build_file_rules(cwd: &std::path::Path) -> Option<mush_agent::FileRuleCallback> {
    let rules = mush_ext::rules::discover_rules(cwd);
    let rule_index = if rules.is_empty() {
        None
    } else {
        let count = rules.len();
        eprintln!("\x1b[2mfound {count} rule files in .mush/rules/\x1b[0m");
        Some(std::sync::Arc::new(mush_ext::rules::RuleIndex::new(rules)))
    };

    let cwd = cwd.to_path_buf();
    Some(std::sync::Arc::new(move |path: &std::path::Path| {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };

        let nested_agents = loader::find_nested_agents_md(&cwd, &resolved);
        let mut parts = Vec::new();

        if let Some(text) = format_nested_agents(&cwd, &nested_agents) {
            parts.push(text);
        }

        if let Some(index) = &rule_index {
            let matched = index.match_file(path);
            if !matched.is_empty() {
                let text: Vec<_> = matched
                    .iter()
                    .map(|rule| format!("## {} rules\n{}", rule.name, rule.content))
                    .collect();
                parts.push(text.join("\n\n"));
            }
        }

        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    }))
}

/// build LSP diagnostic callback and shared registry if enabled in config
fn build_lsp_diagnostics(
    cwd: &std::path::Path,
    cfg: &config::Config,
) -> (
    Option<mush_agent::DiagnosticCallback>,
    Option<std::sync::Arc<mush_lsp::LspRegistry>>,
) {
    if !cfg.lsp.diagnostics {
        return (None, None);
    }

    let mut registry = mush_lsp::LspRegistry::new(cwd.to_path_buf());

    // apply user-configured server overrides
    for (lang_name, entry) in &cfg.lsp.servers {
        if let Some(language) = parse_language_name(lang_name) {
            registry.add_override(mush_lsp::ServerConfig {
                language,
                command: entry.command.clone(),
                args: entry.args.clone(),
            });
        }
    }

    let registry = std::sync::Arc::new(registry);

    eprintln!("\x1b[2mLSP diagnostics enabled\x1b[0m");

    let diag_registry = registry.clone();
    let callback: mush_agent::DiagnosticCallback =
        std::sync::Arc::new(move |path: &std::path::Path| {
            let registry = diag_registry.clone();
            let path = path.to_path_buf();
            Box::pin(async move {
                let text = tokio::fs::read_to_string(&path).await.unwrap_or_default();
                match registry.notify_and_diagnose(&path, &text).await {
                    Ok(diags) if !diags.is_empty() => {
                        Some(mush_lsp::format_diagnostics(&path, &diags))
                    }
                    _ => None,
                }
            })
        });

    (Some(callback), Some(registry))
}

/// map config language name to mush-treesitter Language
fn parse_language_name(name: &str) -> Option<mush_treesitter::Language> {
    match name.to_lowercase().as_str() {
        "rust" => Some(mush_treesitter::Language::Rust),
        "python" => Some(mush_treesitter::Language::Python),
        "javascript" | "js" => Some(mush_treesitter::Language::JavaScript),
        "typescript" | "ts" => Some(mush_treesitter::Language::TypeScript),
        "tsx" => Some(mush_treesitter::Language::Tsx),
        "go" => Some(mush_treesitter::Language::Go),
        "c" => Some(mush_treesitter::Language::C),
        "cpp" | "c++" => Some(mush_treesitter::Language::Cpp),
        "java" => Some(mush_treesitter::Language::Java),
        "bash" | "shell" | "sh" => Some(mush_treesitter::Language::Bash),
        "nix" => Some(mush_treesitter::Language::Nix),
        _ => None,
    }
}

/// resolve the thinking level from CLI flags, saved per-dir+model prefs,
/// the per-model fallback, the model's own default, and the config-file
/// fallback.
///
/// precedence (highest first):
/// 1. `--think` CLI flag → `High`
/// 2. saved preference for this `cwd + model.id`
/// 3. saved preference for `model.id` in any other directory (most
///    recent). lets a model the user has touched elsewhere carry their
///    last pick instead of snapping back to upstream defaults
/// 4. `model.default_thinking_level` (codex's recommended starting level)
/// 5. `cfg.thinking` (global config default)
/// 6. nothing → `None`
pub fn resolve_thinking(
    cli_thinking: bool,
    model: &Model,
    thinking_prefs: &mush_tui::ThinkingPrefs,
    cwd: &std::path::Path,
    cfg: &config::Config,
) -> Option<ThinkingLevel> {
    if cli_thinking {
        return Some(ThinkingLevel::High);
    }
    thinking_prefs
        .get(cwd, model.id.as_ref())
        .or_else(|| thinking_prefs.last_for_model(model.id.as_ref()))
        .or_else(|| model.default_thinking_level.clone())
        .or_else(|| cfg.thinking.clone())
        .map(|level| level.normalize_visible())
}

pub fn stream_options_from_config(
    cfg: &config::Config,
    thinking: Option<ThinkingLevel>,
    max_tokens_override: Option<u64>,
) -> StreamOptions {
    StreamOptions {
        thinking,
        effort: cfg.effort.clone(),
        max_tokens: max_tokens_override.or(cfg.max_tokens).map(TokenCount::new),
        cache_retention: cfg.cache_retention,
        anthropic_betas: Some(cfg.settings.anthropic_betas.clone()),
        force_budget_thinking: cfg.force_budget_thinking,
        ..Default::default()
    }
}

/// resolve API key for a model: env > config > stored credentials > oauth
pub async fn resolve_api_key(options: &mut StreamOptions, model: &Model, cfg: &config::Config) {
    if options.api_key.is_some() {
        return;
    }

    let resolved = mush_ai::auth::resolve_auth(
        &model.provider,
        &cfg.api_keys.to_map(),
        mush_ai::credentials::default_store().as_ref(),
    )
    .await;
    options.api_key = resolved.api_key;
    options.account_id = resolved.account_id;
}

/// pick the default model based on per-cwd memory then available auth
pub fn default_model_id(cfg: &config::Config, cwd: &std::path::Path) -> String {
    if let Some(model) = &cfg.model {
        return model.clone();
    }

    let last_models = config::load_last_models();
    if let Some(model) = last_models.get(cwd)
        && mush_ai::discovery::resolve_model_by_id(model).is_some()
    {
        return model.to_string();
    }

    let oauth_store = mush_ai::oauth::load_credentials().unwrap_or_default();

    let has_anthropic_auth = cfg.api_keys.anthropic.is_some()
        || std::env::var("ANTHROPIC_API_KEY").is_ok()
        || std::env::var("ANTHROPIC_OAUTH_TOKEN").is_ok()
        || oauth_store.providers.contains_key("anthropic");

    let has_openai_codex_auth = oauth_store.providers.contains_key("openai-codex");

    if has_anthropic_auth {
        return "claude-opus-4-7".into();
    }

    if has_openai_codex_auth {
        return "gpt-5.4".into();
    }

    "claude-opus-4-7".into()
}

/// short model list for error messages
pub fn list_models_short() -> String {
    mush_ai::discovery::merged_models()
        .iter()
        .map(|m| format!("  {} ({})", m.id, m.provider))
        .collect::<Vec<_>>()
        .join("\n")
}

/// compact messages when approaching context limit.
/// uses escalating strategy: observation masking first, then LLM summarisation.
#[allow(clippy::too_many_arguments)]
pub async fn auto_compact(
    messages: Vec<Message>,
    context_tokens: TokenCount,
    context_window: TokenCount,
    registry: &ApiRegistry,
    model: &Model,
    options: &StreamOptions,
    lifecycle_hooks: Option<&mush_agent::LifecycleHooks>,
    cwd: Option<&std::path::Path>,
) -> Vec<Message> {
    let result = compact::auto_compact(
        messages,
        context_tokens,
        context_window,
        registry,
        model,
        options,
    )
    .await;

    if result.masked_count > 0 {
        eprintln!(
            "\x1b[2m(masked {} old tool outputs, ~{} tokens saved)\x1b[0m",
            result.masked_count, result.mask_tokens_saved
        );
    }
    if result.summarised_count > 0 {
        eprintln!(
            "\x1b[2m(compacted: {} messages summarised, {} kept)\x1b[0m",
            result.summarised_count,
            result.messages.len()
        );
    }

    let mut messages = result.messages;

    // post-compaction hooks (needs mush-agent types, so handled here)
    if let Some(hooks) = lifecycle_hooks
        && !hooks
            .for_point(mush_agent::HookPoint::PostCompaction)
            .is_empty()
    {
        let results = hooks
            .run_all(mush_agent::HookPoint::PostCompaction, cwd)
            .await;
        let output: String = results
            .iter()
            .filter(|r| !r.output.is_empty())
            .map(|r| r.output.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !output.is_empty() {
            eprintln!("\x1b[2m(post-compaction hook output injected)\x1b[0m");
            messages.push(Message::User(UserMessage {
                content: UserContent::Text(format!("[post-compaction hook output]\n{output}")),
                timestamp_ms: Timestamp::now(),
            }));
        }
        for r in &results {
            if !r.success {
                eprintln!(
                    "\x1b[33mwarning: post-compaction hook failed: {}\x1b[0m",
                    r.command
                );
            }
        }
    }

    messages
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::{LazyLock, Mutex};

    use super::*;
    use mush_ext::loader::{ProjectContext, Skill};

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct EnvGuard {
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvGuard {
        fn set(vars: &[(&'static str, Option<&str>)]) -> Self {
            let mut saved = Vec::new();
            unsafe {
                for (key, value) in vars {
                    saved.push((*key, std::env::var_os(key)));
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                for (key, value) in self.saved.drain(..) {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    #[test]
    fn default_prompt_encourages_targeted_exploration() {
        let context = ProjectContext {
            agents_md: vec![],
            skills: vec![],
        };
        let prompt = build_system_prompt_from_context(&context, std::path::Path::new("/repo"));
        assert!(
            prompt.contains("grep"),
            "expected guidance to mention grep for exploration"
        );
        assert!(
            prompt.contains("targeted"),
            "expected prompt to mention targeted reads"
        );
    }

    #[test]
    fn default_prompt_mentions_terminal_markdown_rendering() {
        let context = ProjectContext {
            agents_md: vec![],
            skills: vec![],
        };
        let prompt = build_system_prompt_from_context(&context, std::path::Path::new("/repo"));

        assert!(prompt.contains("plain text that will be styled by the terminal TUI"));
        assert!(prompt.contains("Markdown-compatible basics"));
        assert!(prompt.contains("terminal TUI"));
        assert!(prompt.contains("fenced code blocks"));
        assert!(prompt.contains("Avoid HTML, ANSI codes"));
    }

    #[test]
    fn summaries_prompt_lists_available_skills() {
        let context = ProjectContext {
            agents_md: vec![],
            skills: vec![
                Skill {
                    name: "zeta".into(),
                    description: "zeta skill".into(),
                    path: "/tmp/zeta/SKILL.md".into(),
                },
                Skill {
                    name: "alpha".into(),
                    description: "alpha skill".into(),
                    path: "/tmp/alpha/SKILL.md".into(),
                },
            ],
        };

        let prompt = build_system_prompt_from_context(&context, std::path::Path::new("/repo"));

        let alpha = prompt.find("<name>alpha</name>").unwrap();
        let zeta = prompt.find("<name>zeta</name>").unwrap();

        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("<description>alpha skill</description>"));
        assert!(prompt.contains("<location>/tmp/alpha/SKILL.md</location>"));
        assert!(prompt.contains("Use the skill tool to load a skill"));
        assert!(prompt.contains("$skill-name"));
        assert!(!prompt.contains("load_skill"));
        assert!(alpha < zeta);
    }

    #[test]
    fn file_context_includes_nested_agents_without_rules() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("repo");
        let nested = cwd.join("src/lib");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            cwd.join("src/AGENTS.md"),
            "# nested agents\nuse the local workflow\n",
        )
        .unwrap();

        let callback = build_file_rules(&cwd).unwrap();
        let text = callback(std::path::Path::new("src/lib/main.rs")).unwrap();

        assert!(text.contains("nested agents"));
        assert!(text.contains("use the local workflow"));
    }

    #[test]
    fn anthropic_auth_defaults_to_opus_4_7() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        let _guard = EnvGuard::set(&[
            ("MUSH_CONFIG_DIR", Some(path)),
            ("MUSH_DATA_DIR", Some(path)),
            ("ANTHROPIC_API_KEY", Some("sk-ant-test")),
            ("ANTHROPIC_OAUTH_TOKEN", None),
        ]);

        let cfg: config::Config = toml::from_str("").unwrap();

        assert_eq!(default_model_id(&cfg, dir.path()), "claude-opus-4-7");
    }

    #[test]
    fn no_auth_defaults_to_opus_4_7() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        let _guard = EnvGuard::set(&[
            ("MUSH_CONFIG_DIR", Some(path)),
            ("MUSH_DATA_DIR", Some(path)),
            ("ANTHROPIC_API_KEY", None),
            ("ANTHROPIC_OAUTH_TOKEN", None),
        ]);

        let cfg: config::Config = toml::from_str("").unwrap();

        assert_eq!(default_model_id(&cfg, dir.path()), "claude-opus-4-7");
    }

    fn dummy_model_with_default(level: Option<ThinkingLevel>) -> Model {
        let mut m = mush_ai::models::all_models_with_user()
            .into_iter()
            .next()
            .unwrap();
        m.default_thinking_level = level;
        m
    }

    #[test]
    fn resolve_thinking_prefers_per_dir_override_over_model_default() {
        // user said "i want medium for this project"; that wins over the
        // model's curated recommendation
        let dir = tempfile::tempdir().unwrap();
        let mut prefs = mush_tui::ThinkingPrefs::default();
        let model = dummy_model_with_default(Some(ThinkingLevel::High));
        prefs.set(
            dir.path().to_path_buf(),
            model.id.to_string(),
            ThinkingLevel::Medium,
        );
        let cfg: config::Config = toml::from_str("").unwrap();
        assert_eq!(
            resolve_thinking(false, &model, &prefs, dir.path(), &cfg),
            Some(ThinkingLevel::Medium)
        );
    }

    #[test]
    fn resolve_thinking_falls_back_to_model_default_when_no_override() {
        // codex hints `medium`; with no per-dir override and no config
        // setting, the model's default applies
        let dir = tempfile::tempdir().unwrap();
        let prefs = mush_tui::ThinkingPrefs::default();
        let model = dummy_model_with_default(Some(ThinkingLevel::Medium));
        let cfg: config::Config = toml::from_str("").unwrap();
        assert_eq!(
            resolve_thinking(false, &model, &prefs, dir.path(), &cfg),
            Some(ThinkingLevel::Medium)
        );
    }

    #[test]
    fn resolve_thinking_model_default_outranks_config_thinking() {
        // a model-specific default is more targeted than the global
        // config default, so it should win when both are set
        let dir = tempfile::tempdir().unwrap();
        let prefs = mush_tui::ThinkingPrefs::default();
        let model = dummy_model_with_default(Some(ThinkingLevel::High));
        let cfg: config::Config = toml::from_str("thinking = \"low\"").unwrap();
        assert_eq!(
            resolve_thinking(false, &model, &prefs, dir.path(), &cfg),
            Some(ThinkingLevel::High)
        );
    }

    #[test]
    fn resolve_thinking_falls_through_to_config_when_no_model_default() {
        // uncurated model with no default; cfg.thinking takes over
        let dir = tempfile::tempdir().unwrap();
        let prefs = mush_tui::ThinkingPrefs::default();
        let model = dummy_model_with_default(None);
        let cfg: config::Config = toml::from_str("thinking = \"low\"").unwrap();
        assert_eq!(
            resolve_thinking(false, &model, &prefs, dir.path(), &cfg),
            Some(ThinkingLevel::Low)
        );
    }

    #[test]
    fn config_effort_is_independent_from_config_thinking() {
        let cfg: config::Config = toml::from_str(
            r#"
thinking = "high"
effort = "low"
"#,
        )
        .unwrap();
        let options = stream_options_from_config(&cfg, Some(ThinkingLevel::High), None);

        assert_eq!(options.effort, Some(mush_ai::types::EffortLevel::Low));
        assert_eq!(options.thinking, Some(ThinkingLevel::High));
    }

    #[test]
    fn resolve_thinking_falls_back_to_per_model_pref_from_other_dir() {
        // user picked Medium for this model in a different project; when
        // they switch back into a fresh dir without a per-dir entry, the
        // per-model fallback should hand back Medium instead of the
        // upstream default
        let other_dir = tempfile::tempdir().unwrap();
        let active_dir = tempfile::tempdir().unwrap();
        let mut prefs = mush_tui::ThinkingPrefs::default();
        let model = dummy_model_with_default(Some(ThinkingLevel::High));
        prefs.set(
            other_dir.path().to_path_buf(),
            model.id.to_string(),
            ThinkingLevel::Medium,
        );
        let cfg: config::Config = toml::from_str("").unwrap();
        // active_dir has no per-dir entry but the per-model fallback
        // returns Medium from the other dir's pick
        assert_eq!(
            resolve_thinking(false, &model, &prefs, active_dir.path(), &cfg),
            Some(ThinkingLevel::Medium),
            "per-model fallback should outrank model.default_thinking_level"
        );
    }

    #[tokio::test]
    #[expect(
        clippy::await_holding_lock,
        reason = "env mutation tests need serial access while async auth resolves env vars"
    )]
    async fn init_compaction_model_resolves_auth_for_compaction_provider() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        let active_model = mush_ai::discovery::resolve_model_by_id("claude-opus-4-7").unwrap();
        let active_options = StreamOptions {
            api_key: Some(ApiKey::new("sk-ant-oat-active-token").unwrap()),
            account_id: Some("anthropic-account".into()),
            ..StreamOptions::default()
        };

        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::set(&[
            ("MUSH_CONFIG_DIR", Some(path)),
            ("MUSH_DATA_DIR", Some(path)),
            ("ANTHROPIC_API_KEY", None),
            ("ANTHROPIC_OAUTH_TOKEN", Some("sk-ant-oat-active-token")),
            ("OPENAI_CODEX_API_KEY", Some("codex-compaction-token")),
        ]);
        let cfg: config::Config = toml::from_str(r#"compaction_model = "gpt-5.4""#).unwrap();
        let (_, compact_options) = init_compaction_model(&cfg, &active_model, &active_options)
            .await
            .unwrap();

        assert_eq!(
            compact_options.api_key,
            Some(ApiKey::new("codex-compaction-token").unwrap())
        );
        assert_eq!(compact_options.account_id, None);
    }

    #[test]
    fn forbidden_chars_parses_from_toml() {
        // each entry must be exactly one char (Unicode scalar), serde
        // accepts a single-char string into a `char`. multi-char
        // strings should fail to parse rather than silently truncate
        let cfg: config::Config = toml::from_str(r#"forbidden_chars = ["—", ";", "–"]"#).unwrap();
        assert_eq!(cfg.forbidden_chars, vec!['—', ';', '–']);
    }

    #[test]
    fn forbidden_chars_defaults_to_empty() {
        let cfg: config::Config = toml::from_str("").unwrap();
        assert!(cfg.forbidden_chars.is_empty());
    }

    #[test]
    fn forbidden_chars_rejects_multi_char_strings() {
        // safety: a model couldn't sensibly comply with a "forbidden
        // string" of length > 1 via the per-char validator, so reject
        // the config rather than silently misbehave
        let result: Result<config::Config, _> = toml::from_str(r#"forbidden_chars = ["nope"]"#);
        assert!(
            result.is_err(),
            "multi-char string should fail to parse as char"
        );
    }
}
