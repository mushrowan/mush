use std::fmt::Write as _;
use std::sync::Arc;

use color_eyre::eyre::{Result, eyre};
use mush_ai::providers;
use mush_ai::registry::ApiRegistry;
use mush_ai::types::*;
use mush_ext::loader;
use mush_session::compact;

use crate::config;
use crate::output::Style;

pub struct ClientSetup {
    pub cfg: config::Config,
    pub model: Model,
    pub compaction_model: Option<(Model, StreamOptions)>,
    pub cwd: std::path::PathBuf,
    pub options: StreamOptions,
    pub debug_cache: bool,
    pub http_client: mush_ai::http::HttpClient,
    pub project_context: loader::ProjectContext,
    pub rlm: bool,
}

pub struct ClientArgs {
    pub model: Option<String>,
    pub thinking: bool,
    pub max_tokens: Option<u64>,
    pub config_overrides: Vec<String>,
    pub discovery: mush_ext::DiscoveryOptions,
    pub debug_cache: bool,
}

pub struct AppSetup {
    pub client: ClientSetup,
    pub registry: ApiRegistry,
    pub system_prompt: String,
    pub tools: mush_agent::tool::ToolRegistry,
    pub max_turns: usize,
    pub lifecycle_hooks: mush_agent::LifecycleHooks,
    pub file_rules: Option<mush_agent::FileRuleCallback>,
    pub lsp_diagnostics: Option<mush_agent::DiagnosticCallback>,
    pub mcp: Option<std::sync::Arc<mush_mcp::McpManager>>,
    pub kernel_state: Option<Arc<dyn mush_kernel::KernelState>>,
    pub kernel_packages: Vec<String>,
    pub ipython_tool: Option<Arc<mush_tools::ipython::IpythonTool>>,
}

#[derive(Default, Clone)]
pub struct KernelSinks {
    pub output: Option<mush_tools::bash::OutputSink>,
    pub notices: Option<mush_tools::kernel_jobs::NoticeSink>,
    pub busy_prompt: Option<mush_tools::ipython::BusyKernelPrompt>,
}

#[must_use]
pub fn env_depth(name: &str) -> Option<u32> {
    std::env::var(name).ok()?.trim().parse().ok()
}

#[must_use]
pub fn rlm_depth() -> u32 {
    env_depth("RLM_DEPTH").unwrap_or(0)
}

#[must_use]
pub fn rlm_max_depth(cfg: &config::Config) -> u32 {
    cfg.rlm_max_depth
        .or_else(|| env_depth("RLM_MAX_DEPTH"))
        .unwrap_or(mush_runtime::session::DEFAULT_RLM_MAX_DEPTH)
}

pub fn kernel_plan(cfg: &config::Config) -> Result<mush_kernel::PythonPlan> {
    let managed = managed_venv(cfg);
    Ok(mush_kernel::PythonPlan::resolve(
        cfg.kernel.python.as_deref(),
        managed.as_ref(),
    )?)
}

fn managed_venv(cfg: &config::Config) -> Option<mush_kernel::ManagedVenv> {
    cfg.kernel.managed.then(|| {
        let mut venv = mush_kernel::ManagedVenv::at(mush_session::data_dir().join("kernel-venv"));
        venv.packages
            .extend(cfg.kernel.extra_packages.iter().cloned());
        venv
    })
}

pub fn build_ipython_tool(
    cfg: &config::Config,
    cwd: &std::path::Path,
    sinks: KernelSinks,
    orphans: Option<mush_tools::orphans::OrphanJournal>,
) -> mush_tools::ipython::IpythonTool {
    let cwd_arc: Arc<std::path::Path> = Arc::from(cwd);
    let host = mush_tools::ipython::MushHost::new(cwd_arc.clone());
    if let Some(orphans) = orphans {
        host.jobs().set_orphan_journal(orphans);
    }
    if let Some(sink) = sinks.notices {
        host.jobs().set_notice_sink(sink);
    }
    let mut ipython = mush_tools::ipython::IpythonTool::with_mush_host(host, cwd_arc);
    ipython.kernel_config_mut().python = cfg.kernel.python.clone();
    ipython.kernel_config_mut().managed_venv = managed_venv(cfg);
    if let Some(sink) = sinks.output {
        ipython = ipython.with_output_sink(sink);
    }
    if let Some(prompt) = sinks.busy_prompt {
        ipython = ipython.with_busy_prompt(prompt);
    }
    ipython
}

pub struct SetupArgs {
    pub client: ClientArgs,
    pub max_turns: Option<usize>,
    pub system: Option<String>,
    pub no_tools: bool,
    pub tools: Option<Vec<String>>,
    pub append_system: Option<String>,
    pub output_sink: Option<mush_tools::bash::OutputSink>,
    pub kernel_notice_sink: Option<mush_tools::kernel_jobs::NoticeSink>,
    pub kernel_busy_prompt: Option<mush_tools::ipython::BusyKernelPrompt>,
    pub timer: Option<crate::timing::PhaseTimer>,
}

impl ClientSetup {
    pub async fn init(
        args: ClientArgs,
        timer: Option<&mut crate::timing::PhaseTimer>,
    ) -> Result<Self> {
        let mut timer = timer;
        let cfg = config::load_config_with(&args.config_overrides)?;
        let debug_cache = args.debug_cache || cfg.debug_cache;
        if let Some(t) = &mut timer {
            t.phase("config");
        }

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
                .connect_timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("failed to build http client"),
        );
        if let Some(t) = &mut timer {
            t.phase("model + http");
        }

        let project_context = loader::discover_project_context_with(&cwd, &args.discovery);
        for diagnostic in &project_context.skill_diagnostics {
            tracing::warn!("skill: {diagnostic}");
        }
        if let Some(t) = &mut timer {
            t.phase("project context");
        }

        let thinking_level = resolve_thinking(args.thinking, &model, &cfg);
        let mut options = stream_options_from_config(&cfg, Some(thinking_level), args.max_tokens);
        resolve_api_key(&mut options, &model, &cfg).await;
        let compaction_model = init_compaction_model(&cfg, &model, &options).await;
        if let Some(t) = &mut timer {
            t.phase("api keys");
        }

        Ok(Self {
            rlm: cfg.rlm,
            cfg,
            model,
            compaction_model,
            cwd,
            options,
            debug_cache,
            http_client,
            project_context,
        })
    }

    #[must_use]
    pub fn skills(&self) -> mush_tui::SkillCatalogue {
        mush_tui::SkillCatalogue {
            skills: self.project_context.skills.clone(),
            diagnostics: self.project_context.skill_diagnostics.clone(),
        }
    }
}

impl AppSetup {
    #[tracing::instrument(name = "startup", skip_all)]
    pub async fn init(args: SetupArgs) -> Result<(Self, Option<crate::timing::StartupReport>)> {
        let mut timer = args.timer;
        let mut client = ClientSetup::init(args.client, timer.as_mut()).await?;
        apply_prompt_overrides(
            &mut client.project_context,
            args.system
                .as_deref()
                .or(client.cfg.system_prompt.as_deref()),
            args.append_system.as_deref(),
        );
        let ClientSetup {
            cfg,
            model,
            cwd,
            project_context,
            http_client,
            ..
        } = &client;

        let mut registry = ApiRegistry::new();
        providers::register_builtins(&mut registry, http_client.clone());

        let rlm = cfg.rlm;
        if rlm && let Err(error) = crate::bundled_skills::install(&crate::bundled_skills::root()) {
            tracing::warn!("bundled skills: {error}");
        }
        let classic_tools = !args.no_tools && !rlm;
        let mut kernel_state: Option<Arc<dyn mush_kernel::KernelState>> = None;
        let orphans = mush_tools::orphans::OrphanJournal::at(
            mush_session::data_dir().join("orphan-processes.jsonl"),
        );
        let reaped = orphans.reap_abandoned();
        if !reaped.killed.is_empty() {
            tracing::info!(pids = ?reaped.killed, "killed bash jobs left by an earlier mush");
        }
        let mut kernel_packages = Vec::new();
        let mut ipython_tool = None;
        let mut tools = if rlm && !args.no_tools {
            let plan = kernel_plan(cfg)?;
            kernel_packages = kernel_package_labels(&plan, &cfg.kernel.extra_packages).await;
            let ipython = Arc::new(build_ipython_tool(
                cfg,
                cwd,
                KernelSinks {
                    output: args.output_sink.clone(),
                    notices: args.kernel_notice_sink.clone(),
                    busy_prompt: args.kernel_busy_prompt.clone(),
                },
                Some(orphans),
            ));
            kernel_state = Some(ipython.clone());
            ipython_tool = Some(ipython.clone());
            mush_agent::tool::ToolRegistry::from_shared(vec![
                ipython as mush_agent::tool::SharedTool,
            ])
        } else {
            init_builtin_tools(
                model,
                cwd,
                args.output_sink,
                args.no_tools,
                http_client,
                mush_tools::forbidden_chars::ForbiddenChars::with_classes(
                    cfg.forbidden_chars.iter().copied(),
                    cfg.forbidden_char_classes.iter().copied(),
                ),
                cfg.max_image_mb,
                cfg.web_search.clone(),
            )
        };
        if let Some(t) = &mut timer {
            t.phase("builtin tools");
        }

        let mcp = init_mcp(cfg, !classic_tools, &mut tools).await;
        if let Some(t) = &mut timer {
            t.phase("mcp");
        }

        init_skill_tools(project_context, !classic_tools, &mut tools);

        let system_prompt = if rlm {
            build_rlm_system_prompt(
                project_context,
                cwd,
                &kernel_packages,
                &RlmRole::root(rlm_max_depth(cfg)),
            )
        } else {
            build_system_prompt_from_context(project_context, cwd)
        };
        if let Some(t) = &mut timer {
            t.phase("system prompt");
        }

        let max_turns = args
            .max_turns
            .or(cfg.max_turns)
            .unwrap_or(mush_agent::DEFAULT_MAX_TURNS);
        let lifecycle_hooks = cfg.lifecycle_hooks();

        init_repo_map(cfg, cwd, !classic_tools, &mut tools);
        if let Some(t) = &mut timer {
            t.phase("repo map");
        }

        let file_rules = build_file_rules(cwd);
        let lsp_diagnostics = init_lsp(cfg, cwd, !classic_tools, &mut tools);
        if let Some(t) = &mut timer {
            t.phase("lsp + rules");
        }

        if !classic_tools || model.supports_native_parallel_calls() {
        } else {
            mush_tools::add_batch_tool(&mut tools);
        }
        if let Some(allowed) = &args.tools {
            tools.retain_named(allowed);
            let loaded: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
            for name in allowed {
                if !loaded.iter().any(|tool| {
                    mush_agent::tool::ToolKey::new(tool) == mush_agent::tool::ToolKey::new(name)
                }) {
                    tracing::warn!("--tools names {name}, which is not a loaded tool");
                }
            }
        }

        let report = timer.map(|t| t.finish());

        Ok((
            Self {
                client,
                registry,
                system_prompt,
                tools,
                max_turns,
                lifecycle_hooks,
                file_rules,
                lsp_diagnostics,
                mcp,
                kernel_state,
                kernel_packages,
                ipython_tool,
            },
            report,
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn init_builtin_tools(
    model: &Model,
    cwd: &std::path::Path,
    output_sink: Option<mush_tools::bash::OutputSink>,
    no_tools: bool,
    http_client: &mush_ai::http::HttpClient,
    forbidden_chars: mush_tools::forbidden_chars::ForbiddenChars,
    max_image_mb: u32,
    web_search: mush_tools::web_search::WebSearchConfig,
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
        web_search,
    )
}

async fn init_mcp(
    cfg: &config::Config,
    no_tools: bool,
    tools: &mut mush_agent::tool::ToolRegistry,
) -> Option<std::sync::Arc<mush_mcp::McpManager>> {
    if no_tools || cfg.mcp.is_empty() {
        return None;
    }

    let manager = std::sync::Arc::new(mush_mcp::McpManager::connect_all(&cfg.mcp).await);
    if cfg.dynamic_mcp {
        tools.extend_shared(mush_mcp::dynamic::dynamic_mcp_tools(manager.catalogue()));
    } else {
        tools.extend_shared(manager.tool_registry().await.iter().cloned());
    }
    Some(manager)
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
            "{}",
            Style::Yellow.stderr(format_args!(
                "warning: unknown compaction_model '{id}', falling back to active model"
            ))
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
    let mut skills: Vec<&loader::Skill> = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect();
    skills.sort_by(|left, right| left.name.cmp(&right.name));

    let mut out = String::from("<available_skills>\n");
    for skill in skills {
        out.push_str("  <skill>\n");
        out.push_str(&format!("    <name>{}</name>\n", escape_xml(&skill.name)));
        out.push_str(&format!(
            "    <description>{}</description>\n",
            escape_xml(&skill.description)
        ));
        out.push_str(&format!(
            "    <location>{}</location>\n",
            escape_xml(&skill.path.display().to_string())
        ));
        out.push_str("  </skill>\n");
    }
    out.push_str("</available_skills>");
    out
}

fn escape_xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

fn default_system_prompt(cwd_str: &str) -> String {
    format!(
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
         - Continue authorised work within the task scope. Keep a record of decisions, \
         checks and unresolved issues. Stop and request clarification when the next step \
         needs approval or essential information\n\
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
    )
}

async fn kernel_package_labels(plan: &mush_kernel::PythonPlan, extra: &[String]) -> Vec<String> {
    let mut labels: Vec<String> = match plan.python_if_present() {
        Some(python) => {
            let names: Vec<&str> = mush_kernel::DEFAULT_PACKAGES
                .iter()
                .map(|package| package.import_name)
                .collect();
            let found = mush_kernel::venv::available_imports(&python, &names).await;
            mush_kernel::DEFAULT_PACKAGES
                .iter()
                .filter(|package| found.iter().any(|name| name == package.import_name))
                .map(|package| package.label.to_string())
                .collect()
        }
        None if plan.managed() => mush_kernel::DEFAULT_PACKAGES
            .iter()
            .map(|package| package.label.to_string())
            .collect(),
        None => Vec::new(),
    };
    if plan.managed() {
        labels.extend(extra.iter().cloned());
    }
    labels
}

const SUBAGENTS_PROMPT: &str = "\
Subagents:\n\
- `await rlm.spawn(prompt, name=...)` starts a child agent with its own context and kernel \
and returns a handle at once (`rlm_child_id`, `name`, `session_id`, `model`). It never waits \
for the child. Pass `model=` to use another model. Spawn independent pieces of work as \
separate children in one cell, then end your turn.\n\
- A child's answer arrives in your conversation as a message labelled `[agent-message from \
child:<name>]`. Do not poll for it. Files the child writes are also yours to read.\n\
- `await rlm.list_subagents()` lists your children (name, status, latest progress note, last \
answer). `await rlm.collect(targets=None, timeout_ms=0)` returns the same snapshots and, with \
a timeout, waits until they settle. `await rlm.delete_subagent(child)` stops one and drops \
its context.\n\
- `await rlm.pause_subagents(targets=None, reason=...)` stops children taking new turns, \
descendants included, and `await rlm.resume_subagents(targets=None)` lets them go again. \
Omit `targets` for all of them. A turn already running finishes; the pause holds anything \
queued, scheduled or delivered until you resume.\n\
- `await agent_message.send(text, receiver_role=\"child\", receiver_name=...)` sends a child \
more instructions. `receiver_role` is required: `\"parent\"`, `\"sibling\"` or `\"child\"`, with \
`receiver_name` for siblings and children. A busy agent sees the message during its turn; an \
idle one wakes into a new turn in its own context. It returns a receipt whose \
`deliveryStatus` is `delivered` (an idle agent got it) or `queued` (it waits on current \
work). `await agent_message.send(\"all\", text)` broadcasts to your whole family and returns \
`{receipts: [...]}`, with an `error` entry for any member it could not reach.\n\
- Give a child one clear task with the context it needs; it starts from an empty conversation.\n\
- `await rlm.create_session(prompt, name=...)` starts a separate top-level session in its own \
worker for work that should outlive this conversation; it runs on its own and reports \
nothing back. `mush send <id> ...` from bash reaches it.";

const OBSERVE_PROMPT: &str = "\
`await agent_observe.list_agents()` names everyone you can reach (yourself, your parent, your \
siblings, your children) with their status and latest progress note. \
`await agent_observe.recent_messages(target, limit=8, max_chars=800)` reads the tail of a live \
family member's conversation. Both are read-only. Prefer a progress note or a message over \
reading someone else's transcript.";

fn child_doctrine(name: &str, depth: u32) -> String {
    format!(
        "You are a child agent named `{name}` (depth {depth}), spawned by your parent agent. Task \
         prompts from it are labelled `[task from parent]`. When the task calls for an answer, \
         send it with `await agent_message.send(message, receiver_role=\"parent\")`; the parent \
         only sees what you send or write to disk, never your transcript. Not every message \
         needs a reply; finish up and go idle when you are done. For long work, report brief \
         progress with `await rlm.progress_note('...')` (512 characters, one per ten seconds)."
    )
}

fn rlm_base_prompt(cwd_str: &str, packages: &[String]) -> String {
    let packages = if packages.is_empty() {
        "Third-party packages: none are guaranteed. Check with `importlib.util.find_spec` \
         before importing one."
            .to_string()
    } else {
        format!(
            "Pre-installed Python packages: {}. Install more with `await bash('uv pip install \
             --python ' + sys.executable + ' <pkg>')`.",
            packages.join(", ")
        )
    };
    format!(
        "You are running inside mush, a coding agent harness. You work by writing and running \
         Python in a persistent REPL: the `ipython` tool is your only tool. Break a task into \
         steps, run code, read what comes back, and iterate. When the task is done, stop \
         calling tools and give your answer.\n\n\
         Working directory: {cwd_str}\n\
         {packages}\n\n\
         The kernel:\n\
         - State persists across calls: variables, imports, functions and parsed results stay \
         available. Bind anything you will need again to a name instead of recomputing it.\n\
         - Top-level `await` works.\n\
         - `bash(command, timeout=None)` starts a command through bash in the background and \
         returns a handle at once. `await bash(...)` or `await h` waits for the result with \
         `.exit_code`, `.ok` and `.output` (stdout and stderr together). On a running handle, \
         `await h.poll()` is the result or None, `await h.output()` and `await h.tail(n)` show \
         output so far, `await h.kill()` stops it. Each call is its own process; use \
         `os.chdir` and `os.environ` for state that should carry over.\n\
         - For slow work (tests, builds, long scripts) start it, keep the handle in a variable \
         and end your turn. You get a message when it finishes; read the handle then. Do not \
         poll in a loop or sleep to wait.\n\
         - `await edit(path, old, new)` replaces exactly one occurrence of `old` in the file. \
         `old` must match the file text exactly, so read the file first and keep `old` as \
         small as it can be while still unique.\n\
         - Read files with `pathlib.Path(p).read_text()`, search with `re` or \
         `await bash('rg ...')`, create files with `Path.write_text`.\n\
         - `attach_image(path, ...)` puts a png, jpeg, gif or webp in front of you as pixels. \
         Use it to look at a screenshot, chart or diagram; use a library when you need to \
         compute over the image.\n\
         - `await compact.run(instructions=None)` schedules a context compaction for after this \
         turn, with optional instructions on what the summary must keep. Use it when the \
         conversation has grown past what the task needs. `await compact.status()` shows \
         `tokens`, `context_window`, `percent` and whether one is `scheduled`.\n\
         - `await refine.run(instructions=None, global_=False)` schedules a continual harness \
         refinement for after this turn: a review of the trajectory that creates, updates or \
         deletes prompt notes, memories, skill entries and subagent specs. Call it after a \
         repeated failure, when a reusable tactic emerges, or when the user corrects behaviour \
         that should persist. Local by default; `global_=True` for lessons every session should \
         keep. Applied edits reach you as a short refinement notice; the full harness digest \
         is delivered when the session starts, after a compaction and after moving in the \
         tree.\n\
         - Continual harness state is `rlm.harness` (also bare `harness`). \
         `await harness.search(query, kind=None, limit=10)`, `harness.list(kind=None)`, \
         `harness.get(kind, id)` and `harness.overview()` read the entries the digest \
         summarises; the digest is a routing hint, so read the entry when the detail \
         matters. `harness.create_memory(title, content)`, `harness.update_memory(id, \
         title, content)`, `harness.delete_memory(id)` and the matching `*_prompt_note`, \
         `*_skill` and `*_subagent` calls are the single-entry edits, with \
         `harness.create(kind, title, content, ...)` as the generic form. Skill entries \
         need a python `reference` and `arguments`. `refine.run()` is the reviewed pass \
         over the whole trajectory. Ids carry a `local:` or `global:` prefix in the digest \
         and are accepted with it. CRUD is local by default; `global_=True` for stable \
         cross-session lessons, and python reserves `global`, so `global=True` is a syntax \
         error.\n\
         - `await rlm.progress_note(text)` puts one line in the user's status bar; use it \
         during long work so they can see where you are.\n\
         - `await rlm.note(text)` keeps a short note that survives compaction and kernel \
         restarts; they reappear in the conversation whenever they change. `await \
         rlm.notes()` reads them, `await rlm.clear_notes()` drops them. Use them for \
         decisions, paths and state a long task needs later.\n\
         - Use Python for loops, conditionals and parsing. Use `bash()` to run programs, not \
         to write shell programs.\n\
         - Run a project's own commands through its own environment (`cargo test`, `npm run \
         check`, `uv run ...`). Do not install a project's dependencies into the kernel to \
         import it directly.\n\
         - Keep tool output small: slice, filter and summarise in Python before printing. \
         Long results are truncated.\n\n\
         Your final answers are plain text styled by the terminal TUI: short paragraphs, simple \
         lists, inline backticks for paths and commands, fenced code blocks for snippets. No \
         HTML or ANSI codes."
    )
}

#[derive(Debug, Clone, Default)]
pub struct RlmRole {
    pub depth: u32,
    pub max_depth: u32,
    pub child_name: Option<String>,
}

impl RlmRole {
    #[must_use]
    pub fn root(max_depth: u32) -> Self {
        Self {
            depth: 0,
            max_depth,
            child_name: None,
        }
    }

    #[must_use]
    pub fn child(name: &str, depth: u32, max_depth: u32) -> Self {
        Self {
            depth,
            max_depth,
            child_name: Some(name.to_string()),
        }
    }
}

pub fn build_rlm_system_prompt(
    context: &loader::ProjectContext,
    cwd: &std::path::Path,
    packages: &[String],
    role: &RlmRole,
) -> String {
    let cwd_str = cwd.display().to_string();
    let custom = context
        .system_prompt
        .as_ref()
        .map(|file| file.content.trim_end());
    let mut prompt = custom.map_or_else(|| rlm_base_prompt(&cwd_str, packages), str::to_string);
    let mut role_sections = String::new();
    if role.depth < role.max_depth {
        role_sections.push_str("\n\n");
        role_sections.push_str(SUBAGENTS_PROMPT);
    }
    if let Some(name) = &role.child_name {
        role_sections.push_str("\n\n");
        role_sections.push_str(&child_doctrine(name, role.depth));
    }
    if role.depth < role.max_depth || role.child_name.is_some() {
        role_sections.push_str("\n\n");
        role_sections.push_str(OBSERVE_PROMPT);
    }
    if custom.is_none() {
        prompt.push_str(&role_sections);
    }
    push_project_context(&mut prompt, &context.agents_md);
    let skills =
        crate::bundled_skills::with_discovered(&context.skills, &crate::bundled_skills::root());
    if skills.iter().any(|skill| !skill.disable_model_invocation) {
        prompt.push_str(
            "\n\nSkills provide specialised instructions for specific tasks. When a task matches a \
             skill's description, read its SKILL.md with `Path(location).read_text()` before \
             acting, and resolve relative paths in it against the skill's directory. If the user \
             names a skill with `$skill-name`, read it first.\n\n",
        );
        prompt.push_str(&format_available_skills(&skills));
    }
    if custom.is_some() {
        push_date_and_cwd(&mut prompt, &cwd_str);
        prompt.push_str(&role_sections);
    }
    push_append(&mut prompt, context);
    prompt
}

pub fn build_system_prompt_from_context(
    context: &loader::ProjectContext,
    cwd: &std::path::Path,
) -> String {
    let cwd_str = cwd.display().to_string();
    let mut prompt = match &context.system_prompt {
        Some(file) => file.content.trim_end().to_string(),
        None => default_system_prompt(&cwd_str),
    };

    push_project_context(&mut prompt, &context.agents_md);

    if context
        .skills
        .iter()
        .any(|skill| !skill.disable_model_invocation)
    {
        prompt.push_str(
            "\n\nSkills provide specialised instructions for specific tasks. \
             Use the skill tool to load a skill when a task matches its description. \
             If the user explicitly names a skill with `$skill-name`, load that skill before answering.\n\n",
        );
        prompt.push_str(&format_available_skills(&context.skills));
    }
    if context.system_prompt.is_some() {
        push_date_and_cwd(&mut prompt, &cwd_str);
    }
    push_append(&mut prompt, context);
    prompt
}

fn push_date_and_cwd(prompt: &mut String, cwd: &str) {
    let _ = write!(
        prompt,
        "\n\nCurrent date: {}\nCurrent working directory: {cwd}",
        chrono::Local::now().format("%Y-%m-%d")
    );
}

fn push_append(prompt: &mut String, context: &loader::ProjectContext) {
    if let Some(extra) = &context.append_system_prompt {
        prompt.push_str("\n\n");
        prompt.push_str(extra.content.trim_end());
    }
}

fn prompt_input(value: &str) -> mush_ext::PromptFile {
    let path = std::path::Path::new(value);
    if path.is_file() {
        match std::fs::read_to_string(path) {
            Ok(content) => {
                return mush_ext::PromptFile {
                    path: path.to_path_buf(),
                    content,
                };
            }
            Err(error) => {
                tracing::warn!(%error, path = value, "could not read the prompt file, using the text itself");
            }
        }
    }
    mush_ext::PromptFile {
        path: std::path::PathBuf::new(),
        content: value.to_string(),
    }
}

pub fn apply_prompt_overrides(
    context: &mut loader::ProjectContext,
    system: Option<&str>,
    append: Option<&str>,
) {
    if let Some(system) = system.filter(|text| !text.trim().is_empty()) {
        context.system_prompt = Some(prompt_input(system));
    }
    if let Some(append) = append.filter(|text| !text.trim().is_empty()) {
        context.append_system_prompt = Some(prompt_input(append));
    }
}

fn push_project_context(prompt: &mut String, agents: &[loader::AgentsMd]) {
    if agents.is_empty() {
        return;
    }
    prompt.push_str("\n\n# Project Context\n\nProject-specific instructions and guidelines:");
    for agents in agents {
        let _ = write!(
            prompt,
            "\n\n## {}\n\n{}",
            agents.path.display(),
            agents.content.trim_end()
        );
    }
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

fn build_file_rules(cwd: &std::path::Path) -> Option<mush_agent::FileRuleCallback> {
    let rules = mush_ext::rules::discover_rules(cwd);
    let rule_index = if rules.is_empty() {
        None
    } else {
        let count = rules.len();
        eprintln!(
            "{}",
            Style::Dim.stderr(format_args!("found {count} rule files in .mush/rules/"))
        );
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

    eprintln!("{}", Style::Dim.stderr("LSP diagnostics enabled"));

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

pub const DEFAULT_THINKING_LEVEL: ThinkingLevel = ThinkingLevel::Medium;

pub fn resolve_thinking(cli_thinking: bool, model: &Model, cfg: &config::Config) -> ThinkingLevel {
    let level = if cli_thinking {
        ThinkingLevel::High
    } else {
        cfg.thinking.clone().unwrap_or(DEFAULT_THINKING_LEVEL)
    };
    model.clamp_thinking(&level).normalize_visible()
}

pub fn session_thinking(
    cli_thinking: bool,
    model: &Model,
    cfg: &config::Config,
    conversation: &mush_session::ConversationState,
) -> ThinkingLevel {
    match conversation.thinking_level() {
        Some(level) if !cli_thinking => model.clamp_thinking(&level).normalize_visible(),
        _ => resolve_thinking(cli_thinking, model, cfg),
    }
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

    if let Some(model) = config::load_recent_models()
        .iter()
        .find(|id| mush_ai::discovery::resolve_model_by_id(id).is_some())
    {
        return model.clone();
    }

    let oauth_store = mush_ai::oauth::load_credentials().unwrap_or_default();

    let has_anthropic_auth = cfg.api_keys.anthropic.is_some()
        || std::env::var("ANTHROPIC_API_KEY").is_ok()
        || std::env::var("ANTHROPIC_OAUTH_TOKEN").is_ok()
        || oauth_store.providers.contains_key("anthropic");

    let has_openai_codex_auth = oauth_store.providers.contains_key("openai-codex");

    if has_anthropic_auth {
        return "claude-opus-5-5".into();
    }

    if has_openai_codex_auth {
        return "gpt-5.4".into();
    }

    "claude-opus-5-5".into()
}

pub fn list_models_short() -> String {
    mush_ai::discovery::merged_models()
        .iter()
        .map(|m| format!("  {} ({})", m.id, m.provider))
        .collect::<Vec<_>>()
        .join("\n")
}

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
    live: Option<&compact::LiveContext>,
    settings: &compact::CompactionSettings,
) -> compact::AutoCompactResult {
    let result = compact::auto_compact(
        messages,
        context_tokens,
        context_window,
        registry,
        model,
        options,
        live,
        settings,
    )
    .await;

    if result.summarised_count > 0 {
        eprintln!(
            "{}",
            Style::Dim.stderr(format_args!(
                "(compacted: {} messages summarised, {} kept)",
                result.summarised_count,
                result.messages.len()
            ))
        );
    }

    if let Some(hooks) = lifecycle_hooks
        && !hooks
            .for_point(mush_agent::HookPoint::PostCompaction)
            .is_empty()
    {
        let results = hooks
            .run_all(mush_agent::HookPoint::PostCompaction, cwd)
            .await;
        for r in &results {
            if !r.success {
                eprintln!(
                    "{}",
                    Style::Yellow.stderr(format_args!(
                        "warning: post-compaction hook failed: {}",
                        r.command
                    ))
                );
            }
        }
    }

    result
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
    fn the_rlm_prompt_names_the_one_tool_and_the_kernel_helpers() {
        let context = ProjectContext {
            agents_md: vec![mush_ext::AgentsMd {
                path: "/repo/AGENTS.md".into(),
                content: "project rules".into(),
            }],
            skills: vec![Skill {
                name: "nix".into(),
                description: "nix things".into(),
                path: "/home/u/.config/mush/skills/nix/SKILL.md".into(),
                source: mush_ext::SkillSource::User,
                disable_model_invocation: false,
            }],
            skill_diagnostics: vec![],
            system_prompt: None,
            append_system_prompt: None,
        };

        let prompt = build_rlm_system_prompt(
            &context,
            std::path::Path::new("/repo"),
            &["requests".into(), "numpy".into()],
            &RlmRole::root(2),
        );

        assert!(prompt.contains("the `ipython` tool is your only tool"));
        assert!(prompt.contains("await rlm.spawn(prompt, name=...)"));
        assert!(!prompt.contains("You are a child agent"));
        assert!(prompt.contains("Pre-installed Python packages: requests, numpy."));
        assert!(prompt.contains("`bash(command, timeout=None)` starts a command"));
        assert!(prompt.contains("end your turn"));
        assert!(prompt.contains("await edit(path, old, new)"));
        assert!(prompt.contains("Working directory: /repo"));
        assert!(prompt.contains(
            "# Project Context\n\nProject-specific instructions and guidelines:\n\n\
             ## /repo/AGENTS.md\n\nproject rules"
        ));
        assert!(prompt.contains("read_text()"));
        assert!(prompt.contains("<name>nix</name>"));
        assert!(!prompt.contains("Use the skill tool"));
        assert!(!prompt.contains("Batch tool"));
        assert!(!prompt.contains("goal.fail"));
    }

    #[test]
    fn the_rlm_prompt_lists_bundled_skills_unless_a_discovered_one_takes_the_name() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::set(&[("MUSH_DATA_DIR", Some("/data/mush"))]);
        let bundled = build_rlm_system_prompt(
            &ProjectContext::default(),
            std::path::Path::new("/repo"),
            &[],
            &RlmRole::root(2),
        );
        assert!(bundled.contains("<name>goal</name>"));
        assert!(bundled.contains("<location>/data/mush/bundled-skills/goal/SKILL.md</location>"));
        assert!(
            bundled
                .contains("<location>/data/mush/bundled-skills/rlm-heartbeat/SKILL.md</location>")
        );

        let context = ProjectContext {
            skills: vec![Skill {
                name: "goal".into(),
                description: "the project goal skill".into(),
                path: "/repo/.mush/skills/goal/SKILL.md".into(),
                source: mush_ext::SkillSource::Project,
                disable_model_invocation: false,
            }],
            ..ProjectContext::default()
        };
        let overridden = build_rlm_system_prompt(
            &context,
            std::path::Path::new("/repo"),
            &[],
            &RlmRole::root(2),
        );
        assert_eq!(overridden.matches("<name>goal</name>").count(), 1);
        assert!(overridden.contains("<location>/repo/.mush/skills/goal/SKILL.md</location>"));
        assert!(!overridden.contains("/data/mush/bundled-skills/goal/"));
        assert!(overridden.contains("<name>rlm-heartbeat</name>"));

        let classic = build_system_prompt_from_context(
            &ProjectContext::default(),
            std::path::Path::new("/repo"),
        );
        assert!(!classic.contains("bundled-skills"));
    }

    #[test]
    fn a_child_at_the_depth_limit_gets_the_doctrine_and_no_spawn_section() {
        let context = ProjectContext::default();

        let prompt = build_rlm_system_prompt(
            &context,
            std::path::Path::new("/repo"),
            &[],
            &RlmRole::child("auth", 2, 2),
        );

        assert!(prompt.contains("You are a child agent named `auth` (depth 2)"));
        assert!(prompt.contains("[task from parent]"));
        assert!(!prompt.contains("await rlm.spawn("));
        assert!(prompt.contains("await agent_observe.list_agents()"));
    }

    #[test]
    fn a_lone_session_that_cannot_spawn_is_not_told_about_its_family() {
        let prompt = build_rlm_system_prompt(
            &ProjectContext::default(),
            std::path::Path::new("/repo"),
            &[],
            &RlmRole::child("only", 1, 1),
        );
        assert!(prompt.contains("await agent_observe.list_agents()"));

        let root_at_limit = build_rlm_system_prompt(
            &ProjectContext::default(),
            std::path::Path::new("/repo"),
            &[],
            &RlmRole::root(0),
        );
        assert!(!root_at_limit.contains("await agent_observe.list_agents()"));
        assert!(!root_at_limit.contains("await rlm.spawn("));
    }

    #[test]
    fn a_system_md_replaces_the_default_and_keeps_context_files() {
        let context = ProjectContext {
            agents_md: vec![mush_ext::AgentsMd {
                path: "/repo/AGENTS.md".into(),
                content: "project rules".into(),
            }],
            skills: vec![],
            skill_diagnostics: vec![],
            system_prompt: Some(mush_ext::PromptFile {
                path: "/repo/.mush/SYSTEM.md".into(),
                content: "you are a terse reviewer\n".into(),
            }),
            append_system_prompt: Some(mush_ext::PromptFile {
                path: "/home/u/.config/mush/APPEND_SYSTEM.md".into(),
                content: "always answer in british english".into(),
            }),
        };

        let prompt = build_system_prompt_from_context(&context, std::path::Path::new("/repo"));

        assert!(prompt.starts_with("you are a terse reviewer"));
        assert!(
            prompt.ends_with(
                "\nCurrent working directory: /repo\n\nalways answer in british english"
            )
        );
        assert!(prompt.contains("\n\nCurrent date: "));
        assert!(!prompt.contains("You are running inside mush"));
        assert!(prompt.contains("## /repo/AGENTS.md\n\nproject rules"));
    }

    #[test]
    fn an_rlm_system_md_keeps_context_skills_cwd_and_the_child_doctrine() {
        let context = ProjectContext {
            agents_md: vec![mush_ext::AgentsMd {
                path: "/repo/AGENTS.md".into(),
                content: "repo rules".into(),
            }],
            system_prompt: Some(mush_ext::PromptFile {
                path: "/repo/.mush/SYSTEM.md".into(),
                content: "custom base".into(),
            }),
            ..ProjectContext::default()
        };

        let prompt = build_rlm_system_prompt(
            &context,
            std::path::Path::new("/repo"),
            &[],
            &RlmRole::child("worker", 1, 2),
        );

        assert!(prompt.starts_with("custom base\n\n# Project Context"));
        let cwd_at = prompt.find("Current working directory: /repo").unwrap();
        let doctrine_at = prompt.find("You are a child agent").unwrap();
        assert!(prompt.find("## /repo/AGENTS.md").unwrap() < cwd_at);
        assert!(cwd_at < doctrine_at);
        assert!(!prompt.contains("the `ipython` tool is your only tool"));
    }

    #[test]
    fn a_system_flag_is_the_custom_prompt_and_keeps_the_context() {
        let mut context = ProjectContext {
            agents_md: vec![mush_ext::AgentsMd {
                path: "/repo/AGENTS.md".into(),
                content: "repo rules".into(),
            }],
            system_prompt: Some(mush_ext::PromptFile {
                path: "/repo/.mush/SYSTEM.md".into(),
                content: "from SYSTEM.md".into(),
            }),
            ..ProjectContext::default()
        };
        apply_prompt_overrides(&mut context, Some("from the flag"), None);
        let prompt = build_system_prompt_from_context(&context, std::path::Path::new("/repo"));
        assert!(prompt.starts_with("from the flag\n\n# Project Context"));
        assert!(prompt.contains("## /repo/AGENTS.md\n\nrepo rules"));
        assert!(prompt.ends_with("Current working directory: /repo"));
        assert!(!prompt.contains("from SYSTEM.md"));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("prompt.md");
        std::fs::write(&file, "prompt from a file\n").unwrap();
        apply_prompt_overrides(&mut context, file.to_str(), None);
        let prompt = build_system_prompt_from_context(&context, std::path::Path::new("/repo"));
        assert!(prompt.starts_with("prompt from a file\n\n"));
    }

    #[test]
    fn the_append_prompt_comes_last_and_the_cli_value_replaces_the_file() {
        let mut context = ProjectContext {
            agents_md: vec![mush_ext::AgentsMd {
                path: "/repo/AGENTS.md".into(),
                content: "repo rules".into(),
            }],
            skills: vec![Skill {
                name: "nix".into(),
                description: "nix things".into(),
                path: "/skills/nix/SKILL.md".into(),
                source: mush_ext::SkillSource::User,
                disable_model_invocation: false,
            }],
            append_system_prompt: Some(mush_ext::PromptFile {
                path: "/repo/.mush/APPEND_SYSTEM.md".into(),
                content: "from the file".into(),
            }),
            ..ProjectContext::default()
        };
        let root = RlmRole::root(2);
        let prompt = build_rlm_system_prompt(&context, std::path::Path::new("/repo"), &[], &root);
        assert!(prompt.ends_with("</available_skills>\n\nfrom the file"));

        apply_prompt_overrides(&mut context, None, Some("from the cli"));
        let prompt = build_rlm_system_prompt(&context, std::path::Path::new("/repo"), &[], &root);
        assert!(prompt.ends_with("</available_skills>\n\nfrom the cli"));
        assert!(!prompt.contains("from the file"));

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("extra.md");
        std::fs::write(&file, "read from disk\n").unwrap();
        apply_prompt_overrides(&mut context, None, file.to_str());
        let prompt = build_system_prompt_from_context(&context, std::path::Path::new("/repo"));
        assert!(prompt.ends_with("\n\nread from disk"));
    }

    #[test]
    fn context_files_share_one_header_and_name_their_paths() {
        let context = ProjectContext {
            agents_md: vec![
                mush_ext::AgentsMd {
                    path: "/home/u/.config/mush/AGENTS.md".into(),
                    content: "global rules\n".into(),
                },
                mush_ext::AgentsMd {
                    path: "/repo/AGENTS.md".into(),
                    content: "repo rules".into(),
                },
            ],
            ..ProjectContext::default()
        };

        let prompt = build_system_prompt_from_context(&context, std::path::Path::new("/repo"));

        assert_eq!(prompt.matches("# Project Context").count(), 1);
        assert!(prompt.ends_with(
            "# Project Context\n\nProject-specific instructions and guidelines:\n\n\
             ## /home/u/.config/mush/AGENTS.md\n\nglobal rules\n\n\
             ## /repo/AGENTS.md\n\nrepo rules"
        ));
    }

    #[test]
    fn hidden_skills_stay_out_of_the_prompt() {
        let context = ProjectContext {
            agents_md: vec![],
            skills: vec![Skill {
                name: "secret".into(),
                description: "explicit only".into(),
                path: "/tmp/secret/SKILL.md".into(),
                source: mush_ext::SkillSource::User,
                disable_model_invocation: true,
            }],
            skill_diagnostics: vec![],
            system_prompt: None,
            append_system_prompt: None,
        };

        let prompt = build_system_prompt_from_context(&context, std::path::Path::new("/repo"));

        assert!(!prompt.contains("secret"));
        assert!(!prompt.contains("<available_skills>"));
    }

    #[test]
    fn default_prompt_encourages_targeted_exploration() {
        let context = ProjectContext {
            agents_md: vec![],
            skills: vec![],
            skill_diagnostics: vec![],
            system_prompt: None,
            append_system_prompt: None,
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
            skill_diagnostics: vec![],
            system_prompt: None,
            append_system_prompt: None,
        };
        let prompt = build_system_prompt_from_context(&context, std::path::Path::new("/repo"));

        assert!(prompt.contains("plain text that will be styled by the terminal TUI"));
        assert!(prompt.contains("Markdown-compatible basics"));
        assert!(prompt.contains("terminal TUI"));
        assert!(prompt.contains("fenced code blocks"));
        assert!(prompt.contains("Avoid HTML, ANSI codes"));
    }

    #[test]
    fn default_prompt_sets_work_boundaries_without_presence_claims() {
        let context = ProjectContext {
            agents_md: vec![],
            skills: vec![],
            skill_diagnostics: vec![],
            system_prompt: None,
            append_system_prompt: None,
        };
        let prompt = build_system_prompt_from_context(&context, std::path::Path::new("/repo"));
        assert!(prompt.contains("Continue authorised work within the task scope."));
        assert!(prompt.contains("Keep a record of decisions, checks and unresolved issues."));
        assert!(prompt.contains("Stop and request clarification when the next step needs approval or essential information"));
        for claim in [
            "unattended",
            "unmonitored",
            "unsupervised",
            "user is away",
            "user is absent",
            "nobody is watching",
            "you are being monitored",
            "user is watching",
        ] {
            assert!(!prompt.to_lowercase().contains(claim), "{claim}");
        }
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
                    source: mush_ext::SkillSource::User,
                    disable_model_invocation: false,
                },
                Skill {
                    name: "alpha".into(),
                    description: "alpha skill".into(),
                    path: "/tmp/alpha/SKILL.md".into(),
                    source: mush_ext::SkillSource::User,
                    disable_model_invocation: false,
                },
            ],
            skill_diagnostics: vec![],
            system_prompt: None,
            append_system_prompt: None,
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
    fn skill_fields_are_xml_escaped() {
        let skills = [Skill {
            name: "a&b".into(),
            description: "use <this> when \"x\" isn't y".into(),
            path: "/tmp/a&b/SKILL.md".into(),
            source: mush_ext::SkillSource::User,
            disable_model_invocation: false,
        }];

        let block = format_available_skills(&skills);

        assert!(block.contains("<name>a&amp;b</name>"));
        assert!(block.contains(
            "<description>use &lt;this&gt; when &quot;x&quot; isn&apos;t y</description>"
        ));
        assert!(block.contains("<location>/tmp/a&amp;b/SKILL.md</location>"));
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
    fn anthropic_auth_defaults_to_opus_5_5() {
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

        assert_eq!(default_model_id(&cfg, dir.path()), "claude-opus-5-5");
    }

    #[test]
    fn falls_back_to_most_recent_model_when_cwd_has_none() {
        let _lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        let _guard = EnvGuard::set(&[
            ("MUSH_CONFIG_DIR", Some(path)),
            ("MUSH_DATA_DIR", Some(path)),
            ("ANTHROPIC_API_KEY", Some("sk-ant-test")),
            ("ANTHROPIC_OAUTH_TOKEN", None),
        ]);

        config::save_recent_models(&["claude-opus-4-6".to_string()]);

        let cfg: config::Config = toml::from_str("").unwrap();
        assert_eq!(default_model_id(&cfg, dir.path()), "claude-opus-4-6");
    }

    #[test]
    fn no_auth_defaults_to_opus_5_5() {
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

        assert_eq!(default_model_id(&cfg, dir.path()), "claude-opus-5-5");
    }

    fn reasoning_model(levels: &[ThinkingLevel], default: Option<ThinkingLevel>) -> Model {
        let mut m = mush_ai::models::all_models_with_user()
            .into_iter()
            .next()
            .unwrap();
        m.reasoning = true;
        m.supported_thinking_levels = levels.to_vec();
        m.default_thinking_level = default;
        m
    }

    #[test]
    fn resolve_thinking_defaults_to_medium_and_ignores_the_model_default() {
        let model = reasoning_model(&[], Some(ThinkingLevel::High));
        let cfg: config::Config = toml::from_str("").unwrap();
        assert_eq!(resolve_thinking(false, &model, &cfg), ThinkingLevel::Medium);
    }

    #[test]
    fn resolve_thinking_uses_the_configured_level_over_the_model_default() {
        let model = reasoning_model(&[], Some(ThinkingLevel::High));
        let cfg: config::Config = toml::from_str("thinking = \"low\"").unwrap();
        assert_eq!(resolve_thinking(false, &model, &cfg), ThinkingLevel::Low);
    }

    #[test]
    fn a_session_keeps_its_recorded_level_unless_the_flag_asks_for_more() {
        let model = reasoning_model(&[], None);
        let cfg: config::Config = toml::from_str("thinking = \"low\"").unwrap();
        let mut conversation = mush_session::ConversationState::new();
        assert_eq!(
            session_thinking(false, &model, &cfg, &conversation),
            ThinkingLevel::Low
        );
        conversation.record_thinking_level(&ThinkingLevel::Xhigh);
        assert_eq!(
            session_thinking(false, &model, &cfg, &conversation),
            ThinkingLevel::Xhigh
        );
        assert_eq!(
            session_thinking(true, &model, &cfg, &conversation),
            ThinkingLevel::High
        );
    }

    #[test]
    fn resolve_thinking_clamps_to_the_model() {
        let model = reasoning_model(&[ThinkingLevel::Low, ThinkingLevel::High], None);
        let cfg: config::Config = toml::from_str("").unwrap();
        assert_eq!(resolve_thinking(false, &model, &cfg), ThinkingLevel::High);
        let mut plain = model;
        plain.reasoning = false;
        assert_eq!(resolve_thinking(true, &plain, &cfg), ThinkingLevel::Off);
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
        let result: Result<config::Config, _> = toml::from_str(r#"forbidden_chars = ["nope"]"#);
        assert!(
            result.is_err(),
            "multi-char string should fail to parse as char"
        );
    }

    #[test]
    fn forbidden_char_classes_parse_from_toml() {
        use mush_tools::forbidden_chars::CharClass;
        let cfg: config::Config =
            toml::from_str(r#"forbidden_char_classes = ["emoji", "arrows"]"#).unwrap();
        assert_eq!(
            cfg.forbidden_char_classes,
            vec![CharClass::Emoji, CharClass::Arrows]
        );
    }

    #[test]
    fn forbidden_char_classes_default_to_empty() {
        let cfg: config::Config = toml::from_str("").unwrap();
        assert!(cfg.forbidden_char_classes.is_empty());
    }
}
