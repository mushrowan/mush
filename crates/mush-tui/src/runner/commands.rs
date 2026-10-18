use std::fmt::Write as _;

use crate::session::Session;
use crate::slash::{self, SlashAction};

use super::{TuiConfig, WorkerOpener, WorkerTarget};

pub(super) struct SlashEnv<'a> {
    pub tui_config: &'a mut TuiConfig,
    pub pending_prompt: &'a mut Option<String>,
}

pub(super) async fn run_session_command(
    session: &mut Session,
    tui_config: &mut TuiConfig,
    command: mush_runtime::SessionCommand,
) -> bool {
    let events = match session
        .connection
        .send(mush_runtime::ClientCommand::Session(command))
        .await
    {
        Ok(events) => events,
        Err(error) => {
            session.app.push_system_message(error.to_string());
            return false;
        }
    };
    let events: Vec<mush_runtime::SessionEvent> = events
        .into_iter()
        .filter_map(|event| match event {
            mush_runtime::ClientEvent::Session(event) => Some(event),
            _ => None,
        })
        .collect();
    let changed = events.iter().any(|event| {
        matches!(
            event,
            mush_runtime::SessionEvent::TranscriptReplaced { .. }
                | mush_runtime::SessionEvent::SessionIdChanged(_)
        )
    });
    super::streams::apply_session_events(session, tui_config, events);
    changed
}

async fn switch_worker(
    session: &mut Session,
    tui_config: &mut TuiConfig,
    opener: &WorkerOpener,
    target: WorkerTarget,
) -> bool {
    let previous = tui_config.session_id.clone();
    let state = session.connection.state();
    let kept = state.context_len > 0
        || state.tree_len > 0
        || state.title.is_some()
        || state.turn_active
        || state.active_turns > 0;
    let connection = match opener(target).await {
        Ok(connection) => connection,
        Err(error) => {
            session.app.push_system_message(error);
            return false;
        }
    };
    super::runtime::reattach(session, tui_config, connection).await;
    if kept && tui_config.session_id != previous {
        let short: String = previous.as_str().chars().take(8).collect();
        session.app.push_system_message(format!(
            "detached from {short}, it keeps running (mush list, mush --resume {short})"
        ));
    }
    true
}

pub(super) async fn send_command(
    session: &mut Session,
    tui_config: &mut TuiConfig,
    command: mush_runtime::ClientCommand,
) -> bool {
    match session.connection.send(command).await {
        Ok(events) => {
            super::looping::apply_command_events(session, tui_config, events);
            true
        }
        Err(error) => {
            session.app.push_system_message(error.to_string());
            false
        }
    }
}

pub(crate) async fn refresh_decisions(session: &mut Session, tui_config: &mut TuiConfig) {
    let refresh = mush_runtime::ClientCommand::Decisions(mush_runtime::DecisionsCommand::Refresh);
    if let Ok(events) = session.connection.send(refresh).await {
        super::looping::apply_command_events(session, tui_config, events);
    }
}

pub(crate) fn format_autonomous(status: Option<&mush_runtime::AutonomousStatus>) -> String {
    let Some(status) = status else {
        return "autonomous mode off. /autonomous on [--gate \"cmd\"] [--max-turns N] ...".into();
    };
    let limit = |name: &str, value: Option<u64>, used: u64| match value {
        Some(max) => format!("{name} {used}/{max}"),
        None => format!("{name} {used}/unlimited"),
    };
    let mut text = format!(
        "autonomous mode {}\n{}, {}, {}, {}",
        if status.enabled { "on" } else { "off" },
        limit(
            "continuations",
            status.limits.max_continuations.map(u64::from),
            u64::from(status.continuations)
        ),
        limit(
            "turns",
            status.limits.max_turns.map(u64::from),
            u64::from(status.turns)
        ),
        limit("tokens", status.limits.max_tokens, status.tokens),
        limit(
            "seconds",
            status.limits.timeout_ms.map(|ms| ms / 1000),
            status.elapsed_ms / 1000
        ),
    );
    if !status.gates.commands.is_empty() {
        let _ = write!(
            text,
            "\ngates ({} retries, {}s each): {}",
            status.gates.max_retries,
            status.gates.timeout_ms / 1000,
            status.gates.commands.join(" ; ")
        );
    }
    if let Some(failure) = &status.last_failure {
        let _ = write!(
            text,
            "\nlast gate failure: `{}` {} (attempt {})",
            failure.command, failure.exit_text, failure.attempt
        );
    }
    if let Some(reason) = &status.stopped {
        let _ = write!(text, "\nstopped: {reason}");
    }
    text
}

pub(crate) fn format_heartbeat(heartbeat: Option<&mush_runtime::Job>) -> String {
    match heartbeat {
        None => "no heartbeat. /heartbeat [every 10m] <instruction>".into(),
        Some(job) => format!(
            "heartbeat {} ({:?}, {:?}): {}\nruns: {}{}",
            job.expression,
            job.status,
            job.delivery,
            job.prompt,
            job.runs,
            job.next_at()
                .map(|at| {
                    let now = mush_ai::types::Timestamp::now().as_ms();
                    format!(", next in {}s", at.saturating_sub(now) / 1000)
                })
                .unwrap_or_default()
        ),
    }
}

pub(crate) fn format_jobs(jobs: &[mush_runtime::Job]) -> String {
    let live: Vec<&mush_runtime::Job> = jobs
        .iter()
        .filter(|job| {
            matches!(
                job.status,
                mush_runtime::JobStatus::Active | mush_runtime::JobStatus::Paused
            )
        })
        .collect();
    if live.is_empty() {
        return "nothing scheduled. /schedule add \"in 30m\" <prompt>".into();
    }
    let now = mush_ai::types::Timestamp::now().as_ms();
    live.iter()
        .map(|job| {
            let next = job
                .next_at()
                .map(|at| format!("next in {}s", at.saturating_sub(now) / 1000))
                .unwrap_or_else(|| format!("{:?}", job.status));
            format!(
                "{} {} · {} · {} · {}",
                job.id,
                job.label.as_deref().unwrap_or("-"),
                job.expression,
                next,
                job.prompt.lines().next().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn format_goal(goal: Option<&mush_runtime::Goal>) -> String {
    let Some(goal) = goal else {
        return "no goal set. /goal [--budget N] <objective>".into();
    };
    let mut text = format!(
        "goal {} ({:?}): {}\ncontinuations: {}, {} elapsed, tokens used: {}",
        goal.id,
        goal.status,
        goal.objective,
        goal.continuations,
        crate::turn_activity::format_elapsed(std::time::Duration::from_secs(goal.elapsed_secs())),
        goal.tokens_used
    );
    if let Some(budget) = goal.token_budget {
        let _ = write!(text, " of {budget}");
    }
    if let Some(summary) = &goal.summary {
        let _ = write!(text, "\nsummary: {summary}");
    }
    if let Some(error) = &goal.error {
        let _ = write!(text, "\nerror: {error}");
    }
    text
}

pub(super) async fn handle_slash_action(
    session: &mut Session,
    action: SlashAction,
    env: SlashEnv<'_>,
) -> bool {
    let SlashEnv {
        tui_config,
        pending_prompt,
    } = env;
    if conversation_switch_blocked(session, tui_config, pending_prompt.is_some(), &action) {
        session.app.status = Some("wait for the active turn before switching conversation".into());
        return false;
    }
    match action {
        SlashAction::Subagents { args } => {
            super::subagents::command(&mut session.app, tui_config, &args).await;
            false
        }
        SlashAction::Compact { focus } => {
            send_command(
                session,
                tui_config,
                mush_runtime::ClientCommand::Compact { focus, auto: false },
            )
            .await
        }
        SlashAction::Refine(request) => {
            send_command(
                session,
                tui_config,
                mush_runtime::ClientCommand::Refine(request),
            )
            .await;
            false
        }
        SlashAction::Side { question } => {
            slash::side_question::ask_side_question(&mut session.app, &question);
            let _ = session
                .connection
                .send(mush_runtime::ClientCommand::SideQuestion(question))
                .await;
            false
        }
        SlashAction::SystemPrompt => {
            let text = match session.connection.system_prompt().await {
                Some(prompt) => format!(
                    "system prompt ({} chars):\n\n{prompt}",
                    prompt.chars().count()
                ),
                None => "no system prompt is set".to_string(),
            };
            session.app.push_system_message(text);
            false
        }
        SlashAction::Autonomous(action) => {
            let command = autonomous_command(action, &tui_config.autonomous)
                .map(mush_runtime::ClientCommand::Autonomous);
            if send_unless_status(session, tui_config, command).await {
                session
                    .app
                    .push_system_message(format_autonomous(session.app.autonomous.as_ref()));
            }
            false
        }
        SlashAction::Heartbeat(action) => {
            let command = heartbeat_command(action).map(mush_runtime::ClientCommand::Schedule);
            send_unless_status(session, tui_config, command).await;
            session
                .app
                .push_system_message(format_heartbeat(session.app.heartbeat()));
            false
        }
        SlashAction::Schedule(action) => {
            let command = schedule_command(action).map(mush_runtime::ClientCommand::Schedule);
            if send_unless_status(session, tui_config, command).await {
                session
                    .app
                    .push_system_message(format_jobs(&session.app.jobs));
            }
            false
        }
        SlashAction::Goal(action) => {
            if let Some(command) = goal_command(action) {
                send_command(
                    session,
                    tui_config,
                    mush_runtime::ClientCommand::Goal(command),
                )
                .await;
            } else {
                session
                    .app
                    .push_system_message(format_goal(session.app.goal.as_ref()));
            }
            false
        }
        SlashAction::Export { path } => {
            slash::handle_export(
                &mut session.app,
                &session.connection.history().await,
                path.as_deref().unwrap_or(""),
            );
            false
        }
        SlashAction::LoginComplete { code } => {
            if complete_login(&mut session.app, code).await {
                refresh_models(session, tui_config).await;
            }
            false
        }
        SlashAction::LoginApiKey { provider, key } => {
            let message = match super::input::run_validation_and_save(&provider, key.expose()).await
            {
                super::input::SaveOutcome::Saved(toast) => toast,
                super::input::SaveOutcome::Failed(reason) => {
                    format!("{provider} login failed: {reason}")
                }
            };
            session.app.push_system_message(message);
            false
        }
        SlashAction::Login { provider, account } => {
            slash::handle_login_start(&mut session.app, tui_config, provider.as_deref(), account)
                .await;
            false
        }
        SlashAction::Credits => {
            handle_credits(session, tui_config).await;
            false
        }
        SlashAction::Endpoints { model_id } => {
            handle_endpoints(session, tui_config, model_id.as_deref()).await;
            false
        }
        SlashAction::Mcp(query) => {
            let report = mcp_report(tui_config.mcp.as_deref(), &query).await;
            session.app.push_system_message(report);
            false
        }
        SlashAction::New { name, prompt } => {
            start_new_conversation(session, tui_config, pending_prompt, name, prompt).await;
            true
        }
        SlashAction::Fork { index: Some(index) } => {
            run_session_command(
                session,
                tui_config,
                mush_runtime::SessionCommand::Fork {
                    before_user_message: index,
                },
            )
            .await
        }
        SlashAction::Clone => {
            run_session_command(session, tui_config, mush_runtime::SessionCommand::Clone).await;
            true
        }
        SlashAction::Undo => {
            run_session_command(session, tui_config, mush_runtime::SessionCommand::Undo).await
        }
        SlashAction::NavigateTree { entry_id } => {
            navigate_tree(session, tui_config, entry_id).await
        }
        SlashAction::Branch { index: Some(index) } => {
            run_session_command(
                session,
                tui_config,
                mush_runtime::SessionCommand::Branch {
                    user_message: index,
                },
            )
            .await
        }
        SlashAction::Name { title: Some(title) } => {
            let title = crate::widgets::top_bar::sanitise_title(&title);
            if title.is_empty() {
                session.app.push_system_message("usage: /name <title>");
            } else {
                run_session_command(
                    session,
                    tui_config,
                    mush_runtime::SessionCommand::Name { title: Some(title) },
                )
                .await;
            }
            false
        }
        SlashAction::Resume { session_id } => {
            open_conversation(session, tui_config, WorkerTarget::Resume(session_id)).await
        }
        SlashAction::Reload => {
            reload_context(session, tui_config).await;
            false
        }
        SlashAction::RefreshModels => {
            refresh_models(session, tui_config).await;
            false
        }
        other_action => {
            if let Some(prompt) = slash::handle(
                &mut session.app,
                session.connection.as_mut(),
                tui_config,
                &other_action,
            )
            .await
            {
                session.app.start_streaming();
                *pending_prompt = Some(prompt);
            }
            false
        }
    }
}

fn conversation_switch_blocked(
    session: &Session,
    tui_config: &TuiConfig,
    prompt_pending: bool,
    action: &SlashAction,
) -> bool {
    let switches_worker = tui_config.worker_opener.is_some()
        && matches!(action, SlashAction::New { .. } | SlashAction::Resume { .. });
    let changes_conversation = !switches_worker
        && matches!(
            action,
            SlashAction::New { .. }
                | SlashAction::Resume { .. }
                | SlashAction::Undo
                | SlashAction::Branch { .. }
                | SlashAction::NavigateTree { .. }
                | SlashAction::Fork { .. }
                | SlashAction::Clone
        );
    let state = session.connection.state();
    changes_conversation
        && state.controlled
        && (state.turn_active
            || session.app.stream.active
            || !session.app.active_tools.is_empty()
            || state.pending_prompt
            || prompt_pending
            || state.compacting)
}

async fn open_conversation(
    session: &mut Session,
    tui_config: &mut TuiConfig,
    target: WorkerTarget,
) -> bool {
    match tui_config.worker_opener.clone() {
        Some(opener) => switch_worker(session, tui_config, &opener, target).await,
        None => {
            let command = match target {
                WorkerTarget::New => mush_runtime::SessionCommand::New,
                WorkerTarget::Resume(id) => mush_runtime::SessionCommand::Resume { id },
            };
            run_session_command(session, tui_config, command).await
        }
    }
}

async fn start_new_conversation(
    session: &mut Session,
    tui_config: &mut TuiConfig,
    pending_prompt: &mut Option<String>,
    name: Option<String>,
    prompt: Option<String>,
) {
    if !open_conversation(session, tui_config, WorkerTarget::New).await {
        if let Some(prompt) = prompt {
            session.app.input.text = prompt;
            session.app.input.cursor = session.app.input.text.len();
        }
        return;
    }
    if let Some(title) = name
        .map(|name| crate::widgets::top_bar::sanitise_title(&name))
        .filter(|title| !title.is_empty())
    {
        run_session_command(
            session,
            tui_config,
            mush_runtime::SessionCommand::Name { title: Some(title) },
        )
        .await;
    }
    if let Some(prompt) = prompt {
        session.app.push_user_message(prompt.clone());
        session.app.start_streaming();
        *pending_prompt = Some(prompt);
    }
}

async fn navigate_tree(
    session: &mut Session,
    tui_config: &mut TuiConfig,
    entry_id: mush_session::tree::EntryId,
) -> bool {
    let leaves_something_behind = session.connection.navigation_impact(&entry_id).await > 0;
    if leaves_something_behind && !tui_config.branch_summary_skip_prompt {
        session.app.interaction.branch_summary_ask = Some(entry_id);
        session.app.interaction.mode = crate::app::AppMode::ToolConfirm;
        return false;
    }
    run_session_command(
        session,
        tui_config,
        mush_runtime::SessionCommand::NavigateTree {
            entry_id,
            summary: None,
        },
    )
    .await
}

async fn send_unless_status(
    session: &mut Session,
    tui_config: &mut TuiConfig,
    command: Option<mush_runtime::ClientCommand>,
) -> bool {
    match command {
        Some(command) => send_command(session, tui_config, command).await,
        None => true,
    }
}

fn autonomous_command(
    action: slash::AutonomousAction,
    defaults: &mush_runtime::AutonomousDefaults,
) -> Option<mush_runtime::AutonomousCommand> {
    Some(match action {
        slash::AutonomousAction::Status => return None,
        slash::AutonomousAction::On(overrides) => {
            let (limits, gates, subagent_keep_alive_ms) = overrides.resolve(defaults);
            mush_runtime::AutonomousCommand::On {
                limits,
                gates,
                subagent_keep_alive_ms,
            }
        }
        slash::AutonomousAction::Off => mush_runtime::AutonomousCommand::Off,
    })
}

fn heartbeat_command(action: slash::HeartbeatAction) -> Option<mush_runtime::ScheduleCommand> {
    Some(match action {
        slash::HeartbeatAction::Status => return None,
        slash::HeartbeatAction::Set {
            expression,
            prompt,
            follow_up,
        } => mush_runtime::ScheduleCommand::Heartbeat {
            expression,
            prompt,
            delivery: if follow_up {
                mush_runtime::Delivery::FollowUp
            } else {
                mush_runtime::Delivery::Steer
            },
        },
        slash::HeartbeatAction::Pause => mush_runtime::ScheduleCommand::HeartbeatPause,
        slash::HeartbeatAction::Resume => mush_runtime::ScheduleCommand::HeartbeatResume,
        slash::HeartbeatAction::Clear => mush_runtime::ScheduleCommand::HeartbeatClear,
    })
}

fn schedule_command(action: slash::ScheduleAction) -> Option<mush_runtime::ScheduleCommand> {
    Some(match action {
        slash::ScheduleAction::List => return None,
        slash::ScheduleAction::Add {
            expression,
            prompt,
            label,
        } => mush_runtime::ScheduleCommand::Add {
            expression,
            prompt,
            label,
            source: mush_runtime::JobSource::Schedule,
        },
        slash::ScheduleAction::Pause(id) => mush_runtime::ScheduleCommand::Pause(id),
        slash::ScheduleAction::Resume(id) => mush_runtime::ScheduleCommand::Resume(id),
        slash::ScheduleAction::Cancel(id) => mush_runtime::ScheduleCommand::Cancel(id),
    })
}

fn goal_command(action: slash::GoalAction) -> Option<mush_runtime::GoalCommand> {
    Some(match action {
        slash::GoalAction::Status => return None,
        slash::GoalAction::Set {
            objective,
            token_budget,
        } => mush_runtime::GoalCommand::Create {
            objective,
            token_budget,
        },
        slash::GoalAction::Pause => mush_runtime::GoalCommand::Pause,
        slash::GoalAction::Resume => mush_runtime::GoalCommand::Resume,
        slash::GoalAction::Clear => mush_runtime::GoalCommand::Clear,
        slash::GoalAction::Complete => mush_runtime::GoalCommand::Complete { summary: None },
        slash::GoalAction::Fail { error } => mush_runtime::GoalCommand::Fail { error },
    })
}

async fn reload_context(session: &mut Session, tui_config: &mut TuiConfig) {
    let Some(reload) = tui_config.reload_context.clone() else {
        session
            .app
            .push_system_message("/reload not supported in this build".to_string());
        return;
    };
    let reloaded = reload(&tui_config.cwd);
    let _ = session
        .connection
        .send(mush_runtime::ClientCommand::ReloadSystemPrompt)
        .await;
    apply_reloaded_templates(session, &reloaded.templates);
    apply_reloaded_skills(session, &reloaded.skills.skills);
    let warnings = reloaded.skills.diagnostics.len();
    tui_config.skills = reloaded.skills;
    let templates = reloaded.templates.len();
    let skills = tui_config.skills.skills.len();
    session.app.status = Some(crate::app_state::Status::from(format!(
        "reloaded AGENTS.md, {templates} prompt template{s}, {skills} skill{ks}{w}",
        s = plural(templates),
        ks = plural(skills),
        w = if warnings == 0 {
            String::new()
        } else {
            format!(" ({warnings} warnings, /skills)")
        },
    )));
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn apply_reloaded_templates(session: &mut Session, templates: &[mush_ext::PromptTemplate]) {
    let completion = &mut session.app.completion;
    let template_names: std::collections::HashSet<&str> = completion
        .templates
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    completion
        .completions
        .retain(|c| !template_names.contains(c.trim_start_matches('/')));
    completion
        .slash_commands
        .retain(|c| !template_names.contains(c.name.as_str()));

    completion.templates.clear();
    for template in templates {
        completion.completions.push(format!("/{}", template.name));
        completion.slash_commands.push(crate::app::SlashCommand {
            name: template.name.clone(),
            description: template.description.clone().unwrap_or_default(),
        });
        completion.templates.push(template.clone());
    }
}

fn apply_reloaded_skills(session: &mut Session, skills: &[mush_ext::Skill]) {
    let app = &mut session.app;
    app.completion
        .completions
        .retain(|entry| !entry.starts_with("/skill:"));
    app.completion
        .slash_commands
        .retain(|command| !command.name.starts_with("skill:"));
    for skill in skills {
        app.completion
            .completions
            .push(format!("/skill:{}", skill.name));
        app.completion
            .slash_commands
            .push(crate::app::SlashCommand {
                name: format!("skill:{}", skill.name),
                description: skill.description.clone(),
            });
    }
}

async fn mcp_report(manager: Option<&mush_mcp::McpManager>, query: &slash::McpQuery) -> String {
    use mush_mcp::report;
    use slash::McpQuery;

    let Some(manager) = manager else {
        return "no MCP servers configured. add an [mcp.<name>] section to config.toml".to_string();
    };

    match query {
        McpQuery::Status => report::format_status(&manager.statuses().await),
        McpQuery::Tools { server } => {
            let tools = manager.catalogue().tools(server.as_deref()).await;
            report::format_tools(&tools, server.as_deref())
        }
        McpQuery::Schema { tool } => {
            let schema = manager.catalogue().schema(tool).await;
            report::format_schema(tool, schema.as_ref())
        }
        McpQuery::Reconnect { server } => {
            let state = manager.reconnect(server).await;
            report::format_reconnect(server, state.as_ref())
        }
    }
}

pub(super) async fn complete_login(app: &mut crate::app::App, code: String) -> bool {
    use mush_ai::oauth;

    let Some(pending) = app.pending_oauth.take() else {
        app.push_system_message(
            "no in-progress oauth flow. start one with /login [provider] first",
        );
        return false;
    };

    let Some(provider) = oauth::get_provider(&pending.provider_id) else {
        app.push_system_message(format!(
            "oauth provider {} is no longer available",
            pending.provider_id
        ));
        return false;
    };

    let trimmed = code.trim();
    if trimmed.is_empty() {
        app.push_system_message("no code provided. usage: /login-complete <code>");
        app.pending_oauth = Some(pending);
        return false;
    }

    match provider.exchange_code(trimmed, &pending.pkce).await {
        Ok(creds) => save_login_credentials(
            app,
            &pending.provider_id,
            &pending.provider_name,
            pending.account.clone(),
            creds,
        ),
        Err(e) => {
            app.push_system_message(format!("oauth login failed: {e}"));
            app.pending_oauth = Some(pending);
            false
        }
    }
}

fn save_login_credentials(
    app: &mut crate::app::App,
    provider_id: &str,
    provider_name: &str,
    account: Option<mush_ai::types::AccountLabel>,
    creds: mush_ai::oauth::OAuthCredentials,
) -> bool {
    match mush_ai::oauth::save_login(provider_id, account, creds) {
        Ok(label) => {
            app.push_system_message(format!(
                "✓ logged in to {provider_name} as {label} (credentials saved)"
            ));
            true
        }
        Err(e) => {
            app.push_system_message(format!("logged in but saving credentials failed: {e}"));
            false
        }
    }
}

pub(super) async fn refresh_models(session: &mut Session, tui_config: &TuiConfig) {
    let client = tui_config
        .http_client
        .clone()
        .unwrap_or_else(mush_ai::http::client);
    session.app.status = Some("refreshing models…".into());
    let summary = mush_ai::discovery::refresh_and_save(client).await;
    let line = summary.one_line();
    crate::input::reload_model_completions(&mut session.app);
    session
        .app
        .push_system_message(format!("model discovery: {line}"));
    session.app.status = None;
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct DrainedLogins {
    pub saved: bool,
    pub settled: bool,
}

pub(super) fn drain_finished_browser_logins(session: &mut Session) -> DrainedLogins {
    let app = &mut session.app;
    let Some(mut pending) = app.pending_browser_login.take() else {
        return DrainedLogins::default();
    };
    let Some(outcome) = pending.poll() else {
        app.pending_browser_login = Some(pending);
        return DrainedLogins::default();
    };
    let saved = match outcome {
        Ok(creds) => save_login_credentials(
            app,
            &pending.provider_id,
            &pending.provider_name,
            pending.account.clone(),
            creds,
        ),
        Err(reason) => {
            app.push_system_message(format!(
                "{} login failed: {reason}\nrun /login to try again",
                pending.provider_name
            ));
            false
        }
    };
    app.status = None;
    if let Some(picker) = app.interaction.login_picker.as_mut() {
        picker.rebuild_sources();
    }
    DrainedLogins {
        saved,
        settled: true,
    }
}

async fn openrouter_key(tui_config: &TuiConfig) -> Option<mush_ai::ApiKey> {
    let openrouter_model = mush_ai::types::Model {
        provider: mush_ai::types::Provider::OpenRouter,
        ..tui_config.model.clone()
    };
    crate::event_handler::resolve_auth_for_model(
        &openrouter_model,
        &tui_config.provider_api_keys,
        None,
    )
    .await
    .0
}

async fn handle_credits(pane: &mut crate::session::Session, tui_config: &TuiConfig) {
    let Some(key) = openrouter_key(tui_config).await else {
        pane.app
            .push_system_message("/credits: no openrouter api key on file. /login first");
        return;
    };

    match mush_ai::openrouter_credits::fetch_credits(key.expose()).await {
        Ok(balance) => {
            pane.app.push_system_message(format!(
                "openrouter credits: {} remaining ({} purchased, {} used)",
                balance.remaining(),
                balance.total_credits,
                balance.total_usage,
            ));
        }
        Err(err) => {
            pane.app
                .push_system_message(format!("/credits lookup failed: {err}"));
        }
    }
}

async fn handle_endpoints(
    pane: &mut crate::session::Session,
    tui_config: &TuiConfig,
    explicit_model_id: Option<&str>,
) {
    let Some(key) = openrouter_key(tui_config).await else {
        pane.app
            .push_system_message("/endpoints: no openrouter api key on file. /login first");
        return;
    };

    let model_id = explicit_model_id
        .map(str::to_owned)
        .unwrap_or_else(|| pane.app.model_id.to_string());

    match mush_ai::openrouter_endpoints::fetch_endpoints(key.expose(), &model_id).await {
        Ok(endpoints) => {
            let mut report = format!("endpoints for {}:", endpoints.id);
            for ep in &endpoints.endpoints {
                let cache_badge = if ep.supports_cache_control {
                    " [cache_control]"
                } else {
                    ""
                };
                let _ = write!(
                    report,
                    "\n  · {} (ctx {}){}",
                    ep.provider_name, ep.context_length, cache_badge
                );
            }
            if let Some(unique) = endpoints.unique_caching_provider() {
                let _ = write!(report, "\n→ sole caching provider: {unique}");
            }
            pane.app.push_system_message(report);
        }
        Err(err) => {
            pane.app
                .push_system_message(format!("/endpoints lookup failed: {err}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, SlashCommand};
    use crate::browser_login::PendingBrowserLogin;
    use crate::session::Session;
    use mush_ai::oauth::OAuthCredentials;
    use mush_ai::types::{Message, ModelId, Timestamp, TokenCount, UserContent, UserMessage};

    fn user(text: &str) -> Message {
        Message::User(
            UserMessage::builder()
                .content(UserContent::Text(text.into()))
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    fn assistant(text: &str) -> Message {
        Message::Assistant(
            mush_ai::types::AssistantMessage::builder()
                .content(vec![mush_ai::types::AssistantContentPart::Text(
                    mush_ai::types::TextContent { text: text.into() },
                )])
                .model("m".into())
                .provider(mush_ai::types::Provider::Anthropic)
                .api(mush_ai::types::Api::AnthropicMessages)
                .stop_reason(mush_ai::types::StopReason::Stop)
                .timestamp_ms(Timestamp::zero())
                .build(),
        )
    }

    fn conversation_session(messages: Vec<Message>) -> Session {
        let app = App::new(ModelId::from("test"), TokenCount::new(1024));
        Session::with_conversation(
            app,
            mush_session::ConversationState::from_messages(messages),
        )
    }

    fn run(session: &mut Session, config: &mut TuiConfig, action: SlashAction) {
        let mut pending_prompt = None;
        futures::executor::block_on(handle_slash_action(
            session,
            action,
            SlashEnv {
                tui_config: config,
                pending_prompt: &mut pending_prompt,
            },
        ));
    }

    #[test]
    fn fork_starts_a_new_session_before_the_chosen_message() {
        let mut session = conversation_session(vec![user("first"), user("second"), user("third")]);
        let mut config = TuiConfig::for_test();
        let old_id = config.session_id.clone();
        session.app.session_title = Some("old".into());

        run(
            &mut session,
            &mut config,
            SlashAction::Fork { index: Some(2) },
        );

        assert_ne!(config.session_id, old_id);
        assert_eq!(
            session.in_process().runtime.conversation.context(),
            vec![user("first")]
        );
        assert_eq!(session.app.input.text, "second");
        assert!(session.app.session_title.is_none());
    }

    #[test]
    fn bare_fork_opens_a_picker_over_the_user_messages() {
        let mut session = conversation_session(vec![user("first"), user("second")]);
        let mut config = TuiConfig::for_test();

        run(&mut session, &mut config, SlashAction::Fork { index: None });

        assert_eq!(
            session.app.interaction.mode,
            crate::app::AppMode::MessagePicker
        );
        let picker = session.app.interaction.message_picker.as_ref().unwrap();
        assert_eq!(
            picker.purpose,
            crate::message_picker::MessagePickerPurpose::Fork
        );
        assert_eq!(picker.items.len(), 2);
        assert_eq!(picker.items[1].text, "second");
        assert_eq!(picker.selected_item().unwrap().index, 2);
    }

    #[test]
    fn tree_opens_a_picker_and_a_choice_rewinds_the_conversation() {
        let mut session = conversation_session(vec![user("first"), user("second")]);
        let mut config = TuiConfig::for_test();

        run(&mut session, &mut config, SlashAction::Tree);

        let picker = session.app.interaction.message_picker.as_ref().unwrap();
        assert_eq!(
            picker.purpose,
            crate::message_picker::MessagePickerPurpose::Tree
        );
        let target = picker.items[1].entry_id.clone().unwrap();
        session.app.close_message_picker();

        run(
            &mut session,
            &mut config,
            SlashAction::NavigateTree {
                entry_id: target.clone(),
            },
        );

        assert_eq!(
            session.app.interaction.mode,
            crate::app::AppMode::ToolConfirm
        );
        assert_eq!(session.app.interaction.branch_summary_ask, Some(target));
        assert_eq!(session.in_process().runtime.conversation.context_len(), 2);
    }

    #[test]
    fn skip_prompt_jumps_without_asking() {
        let mut session = conversation_session(vec![user("first"), user("second")]);
        let mut config = TuiConfig::for_test();
        config.branch_summary_skip_prompt = true;
        let target = session.in_process().runtime.conversation.tree().entries()[1]
            .id
            .clone();

        run(
            &mut session,
            &mut config,
            SlashAction::NavigateTree { entry_id: target },
        );

        assert_eq!(session.app.interaction.mode, crate::app::AppMode::Insert);
        let context = session.in_process().runtime.conversation.context();
        assert_eq!(context[0], user("first"));
        assert!(
            matches!(&context[1..], [Message::Event(event)] if event.kind == mush_ai::types::EventKind::Moved)
        );
        assert_eq!(session.app.input.text, "second");
    }

    #[test]
    fn navigating_to_the_current_leaf_asks_nothing() {
        let mut session = conversation_session(vec![user("first"), assistant("reply")]);
        let mut config = TuiConfig::for_test();
        let leaf = session.in_process().runtime.conversation.tree().entries()[1]
            .id
            .clone();

        run(
            &mut session,
            &mut config,
            SlashAction::NavigateTree { entry_id: leaf },
        );

        assert_eq!(session.app.interaction.mode, crate::app::AppMode::Insert);
        assert!(session.app.interaction.branch_summary_ask.is_none());
    }

    #[test]
    fn bare_fork_on_an_empty_session_says_so() {
        let mut session = conversation_session(vec![]);
        let mut config = TuiConfig::for_test();

        run(&mut session, &mut config, SlashAction::Fork { index: None });

        assert_eq!(session.app.interaction.mode, crate::app::AppMode::Insert);
        assert!(
            session
                .app
                .messages
                .last()
                .is_some_and(|m| m.content.contains("no messages to fork from"))
        );
    }

    #[test]
    fn fork_out_of_range_leaves_the_session_alone() {
        let mut session = conversation_session(vec![user("first")]);
        let mut config = TuiConfig::for_test();
        let old_id = config.session_id.clone();

        run(
            &mut session,
            &mut config,
            SlashAction::Fork { index: Some(4) },
        );

        assert_eq!(config.session_id, old_id);
        assert_eq!(session.in_process().runtime.conversation.context_len(), 1);
    }

    #[test]
    fn new_with_a_name_and_prompt_names_the_fresh_session_and_sends_the_prompt() {
        let mut session = conversation_session(vec![user("old")]);
        let mut config = TuiConfig::for_test();
        let old_id = config.session_id.clone();
        let mut pending_prompt = None;

        futures::executor::block_on(handle_slash_action(
            &mut session,
            SlashAction::New {
                name: Some("port parser".into()),
                prompt: Some("fix it".into()),
            },
            SlashEnv {
                tui_config: &mut config,
                pending_prompt: &mut pending_prompt,
            },
        ));

        assert_ne!(config.session_id, old_id);
        assert_eq!(session.app.session_title.as_deref(), Some("port parser"));
        assert_eq!(pending_prompt.as_deref(), Some("fix it"));
        let last = session.app.messages.last().expect("the prompt shows");
        assert_eq!(last.role, crate::app::MessageRole::User);
        assert_eq!(last.content, "fix it");
    }

    type Asked = std::sync::Arc<std::sync::Mutex<Vec<WorkerTarget>>>;

    fn opener_to(connection: Option<Session>) -> (WorkerOpener, Asked) {
        let asked: Asked = std::sync::Arc::default();
        let handed = std::sync::Mutex::new(connection.map(|session| session.connection));
        let seen = std::sync::Arc::clone(&asked);
        let opener: WorkerOpener = std::sync::Arc::new(move |target| {
            seen.lock().unwrap().push(target);
            let connection = handed.lock().unwrap().take();
            Box::pin(async move { connection.ok_or_else(|| "the daemon is gone".to_string()) })
        });
        (opener, asked)
    }

    fn slash(
        session: &mut Session,
        config: &mut TuiConfig,
        action: SlashAction,
        pending_prompt: &mut Option<String>,
    ) {
        futures::executor::block_on(handle_slash_action(
            session,
            action,
            SlashEnv {
                tui_config: config,
                pending_prompt,
            },
        ));
    }

    #[test]
    fn new_with_a_daemon_attaches_a_fresh_worker_instead_of_replacing_this_one() {
        let mut session = conversation_session(vec![user("old"), assistant("reply")]);
        let mut config = TuiConfig::for_test();
        let old_id = config.session_id.clone();
        let fresh = conversation_session(vec![]);
        let fresh_id = fresh.connection.state().session_id;
        let (opener, asked) = opener_to(Some(fresh));
        config.worker_opener = Some(opener);
        let mut pending_prompt = None;

        slash(
            &mut session,
            &mut config,
            SlashAction::New {
                name: Some("port parser".into()),
                prompt: Some("fix it".into()),
            },
            &mut pending_prompt,
        );

        assert_eq!(*asked.lock().unwrap(), [WorkerTarget::New]);
        assert_eq!(config.session_id, fresh_id);
        assert_ne!(config.session_id, old_id);
        assert_eq!(
            session.connection.state().title.as_deref(),
            Some("port parser")
        );
        assert_eq!(pending_prompt.as_deref(), Some("fix it"));
        assert!(session.in_process().runtime.conversation.is_empty());
        let short: String = old_id.as_str().chars().take(8).collect();
        assert!(
            session
                .app
                .messages
                .iter()
                .any(|message| message.content.contains(&format!("detached from {short}")))
        );
    }

    #[test]
    fn resume_with_a_daemon_attaches_the_worker_of_that_session() {
        let mut session = conversation_session(vec![user("old")]);
        let mut config = TuiConfig::for_test();
        let target = conversation_session(vec![user("over there"), assistant("hello")]);
        let target_id = target.connection.state().session_id;
        let (opener, asked) = opener_to(Some(target));
        config.worker_opener = Some(opener);

        slash(
            &mut session,
            &mut config,
            SlashAction::Resume {
                session_id: target_id.clone(),
            },
            &mut None,
        );

        assert_eq!(
            *asked.lock().unwrap(),
            [WorkerTarget::Resume(target_id.clone())]
        );
        assert_eq!(config.session_id, target_id);
        assert!(
            session
                .app
                .messages
                .iter()
                .any(|message| message.content == "over there")
        );
        assert!(
            !session
                .app
                .messages
                .iter()
                .any(|message| message.content == "old")
        );
    }

    #[test]
    fn resume_with_a_daemon_moves_into_the_folder_of_that_session() {
        let home = tempfile::tempdir().unwrap();
        let home_path = home.path().canonicalize().unwrap();
        let mut session = conversation_session(vec![user("old")]);
        let mut config = TuiConfig::for_test();
        let mut target = conversation_session(vec![user("over there")]);
        target
            .in_process()
            .set_injections(mush_agent::AgentInjections {
                cwd: Some(home_path.clone()),
                ..Default::default()
            });
        let target_id = target.connection.state().session_id;
        let (opener, _) = opener_to(Some(target));
        config.worker_opener = Some(opener);

        slash(
            &mut session,
            &mut config,
            SlashAction::Resume {
                session_id: target_id,
            },
            &mut None,
        );

        assert_eq!(config.cwd, home_path);
        assert_eq!(std::env::current_dir().unwrap(), home_path);
        let name = home_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(session.app.cwd.ends_with(&name), "{}", session.app.cwd);
    }

    #[test]
    fn a_daemon_that_cannot_open_a_worker_leaves_this_one_attached() {
        let mut session = conversation_session(vec![user("old")]);
        let mut config = TuiConfig::for_test();
        let old_id = config.session_id.clone();
        let (opener, _) = opener_to(None);
        config.worker_opener = Some(opener);

        slash(
            &mut session,
            &mut config,
            SlashAction::New {
                name: None,
                prompt: Some("keep me".into()),
            },
            &mut None,
        );

        assert_eq!(config.session_id, old_id);
        assert_eq!(session.in_process().runtime.conversation.context_len(), 1);
        assert_eq!(session.app.input.text, "keep me");
        assert!(
            session
                .app
                .messages
                .last()
                .is_some_and(|message| message.content.contains("the daemon is gone"))
        );
    }

    #[test]
    fn a_new_session_drops_the_old_sessions_children() {
        let mut session = conversation_session(vec![user("old")]);
        let mut config = TuiConfig::for_test();
        session.app.note_child(mush_runtime::ChildSummary {
            id: "sub-a".into(),
            name: "a".into(),
            parent: None,
            depth: 1,
            session_id: "s".into(),
            model_id: "m".into(),
            thinking: None,
            status: mush_runtime::ChildStatus::Idle,
            spawned_at: Timestamp::zero(),
            updated_at: Timestamp::zero(),
            idle_since: Timestamp::zero(),
            answer: None,
            error: None,
            progress_note: None,
            usage: mush_ai::types::Usage {
                input_tokens: TokenCount::new(100),
                ..Default::default()
            },
            cost: mush_ai::types::Cost {
                input: mush_ai::types::Dollars::new(0.5),
                ..Default::default()
            },
            turns: 1,
            streaming: false,
            tool_running: None,
            awaiting_confirmation: None,
            paused: None,
            context: None,
        });

        run(
            &mut session,
            &mut config,
            SlashAction::New {
                name: None,
                prompt: None,
            },
        );

        assert!(session.app.children.is_empty());
        assert_eq!(
            session.app.session_spend(),
            (TokenCount::ZERO, mush_ai::types::Dollars::ZERO)
        );
    }

    #[test]
    fn name_sets_the_session_title_and_a_blank_one_is_refused() {
        let mut session = conversation_session(vec![]);
        let mut config = TuiConfig::for_test();

        run(
            &mut session,
            &mut config,
            SlashAction::Name {
                title: Some("  port\tparser  ".into()),
            },
        );
        assert_eq!(session.app.session_title.as_deref(), Some("port parser"));

        run(
            &mut session,
            &mut config,
            SlashAction::Name {
                title: Some("\x07".into()),
            },
        );
        assert_eq!(session.app.session_title.as_deref(), Some("port parser"));
    }

    #[test]
    fn undo_and_branch_rewind_the_transcript() {
        let mut session = conversation_session(vec![user("a"), user("b"), user("c")]);
        let mut config = TuiConfig::for_test();

        run(&mut session, &mut config, SlashAction::Undo);
        assert_eq!(session.in_process().runtime.conversation.context_len(), 3);

        run(
            &mut session,
            &mut config,
            SlashAction::Branch { index: Some(1) },
        );
        let context = session.in_process().runtime.conversation.context();
        assert!(
            matches!(&context[..], [Message::Event(event)] if event.kind == mush_ai::types::EventKind::Moved)
        );
        assert_eq!(session.app.messages.len(), 1);
    }

    #[test]
    fn clone_keeps_the_whole_branch_under_a_new_id() {
        let mut session = conversation_session(vec![user("first"), user("second")]);
        let mut config = TuiConfig::for_test();
        let old_id = config.session_id.clone();

        run(&mut session, &mut config, SlashAction::Clone);

        assert_ne!(config.session_id, old_id);
        assert_eq!(
            session.in_process().runtime.conversation.context(),
            vec![user("first"), user("second")]
        );
    }

    fn credentials() -> OAuthCredentials {
        OAuthCredentials {
            access_token: "access-token".into(),
            refresh_token: "refresh-token".into(),
            expires_at: u64::MAX,
            account_id: Some("acct_1".into()),
        }
    }

    fn session() -> Session {
        let app = App::new(ModelId::from("test"), TokenCount::new(1024));
        Session::new(app)
    }

    static CONFIG_DIR_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct ConfigDir {
        _dir: tempfile::TempDir,
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved: Option<std::ffi::OsString>,
    }

    impl Drop for ConfigDir {
        fn drop(&mut self) {
            unsafe {
                match self.saved.take() {
                    Some(value) => std::env::set_var("MUSH_CONFIG_DIR", value),
                    None => std::env::remove_var("MUSH_CONFIG_DIR"),
                }
            }
        }
    }

    async fn isolated_config_dir() -> ConfigDir {
        let lock = CONFIG_DIR_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let saved = std::env::var_os("MUSH_CONFIG_DIR");
        unsafe { std::env::set_var("MUSH_CONFIG_DIR", dir.path()) };
        ConfigDir {
            _dir: dir,
            _lock: lock,
            saved,
        }
    }

    async fn drain_when_settled(session: &mut Session) -> DrainedLogins {
        for _ in 0..100 {
            let drained = drain_finished_browser_logins(session);
            if drained.settled {
                return drained;
            }
            tokio::task::yield_now().await;
        }
        panic!("login never settled");
    }

    #[tokio::test]
    async fn a_finished_browser_login_saves_credentials_without_login_complete() {
        let _config = isolated_config_dir().await;

        let mut session = session();
        session.app.pending_browser_login = Some(PendingBrowserLogin::spawn(
            "anthropic",
            "Anthropic (Claude Pro/Max)",
            async { Ok(credentials()) },
        ));
        let drained = drain_when_settled(&mut session).await;
        assert!(drained.saved, "credentials should be saved");

        let stored = mush_ai::oauth::load_credentials().expect("load credentials");
        assert_eq!(
            stored.get("anthropic", None).unwrap().1.access_token,
            "access-token",
            "credentials should land in the oauth store"
        );
        assert!(
            session.app.pending_browser_login.is_none(),
            "the finished flow should be cleared"
        );
        let messages = session.app.messages.last().unwrap().content.clone();
        assert!(
            messages.contains("Anthropic"),
            "the pane should say which provider logged in, got {messages:?}"
        );
    }

    #[tokio::test]
    async fn a_labelled_browser_login_adds_a_second_account() {
        let _config = isolated_config_dir().await;
        let work = mush_ai::types::AccountLabel::parse("work").unwrap();

        let mut session = session();
        session.app.pending_browser_login = Some(PendingBrowserLogin::spawn(
            "anthropic",
            "Anthropic (Claude Pro/Max)",
            async { Ok(credentials()) },
        ));
        drain_when_settled(&mut session).await;
        session.app.pending_browser_login = Some(
            PendingBrowserLogin::spawn("anthropic", "Anthropic (Claude Pro/Max)", async {
                Ok(credentials())
            })
            .for_account(Some(work.clone())),
        );
        drain_when_settled(&mut session).await;

        let stored = mush_ai::oauth::load_credentials().expect("load credentials");
        let labels: Vec<&str> = stored
            .accounts("anthropic")
            .unwrap()
            .labels()
            .map(mush_ai::types::AccountLabel::as_str)
            .collect();
        assert_eq!(labels, ["default", "work"]);
        assert_eq!(stored.get("anthropic", None).unwrap().0.as_str(), "default");
        let message = session.app.messages.last().unwrap().content.clone();
        assert!(message.contains("as work"), "got {message:?}");
    }

    #[tokio::test]
    async fn switching_account_updates_the_pane_and_says_so() {
        let _config = isolated_config_dir().await;
        let home = mush_ai::types::AccountLabel::parse("home").unwrap();
        mush_ai::oauth::save_login("anthropic", None, credentials()).unwrap();
        mush_ai::oauth::save_login("anthropic", Some(home.clone()), credentials()).unwrap();
        let mut session = session();
        let model = mush_ai::models::all_models()
            .into_iter()
            .find(|model| model.api == mush_ai::types::Api::AnthropicMessages)
            .unwrap();
        session.in_process().runtime.model_id = model.id.to_string();
        let mut config = TuiConfig::for_test();

        crate::slash::commands::handle(
            &mut session.app,
            session.connection.as_mut(),
            &mut config,
            &SlashAction::Account {
                label: Some(home.clone()),
            },
        )
        .await;

        assert_eq!(session.app.accounts.get("anthropic"), Some(&home));
        let message = session.app.messages.last().unwrap().content.clone();
        assert!(
            message.contains("anthropic account: home"),
            "got {message:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_browser_login_reports_the_reason_and_offers_the_paste_fallback() {
        let _config = isolated_config_dir().await;

        let mut session = session();
        session.app.pending_browser_login = Some(PendingBrowserLogin::spawn(
            "anthropic",
            "Anthropic (Claude Pro/Max)",
            async { Err(mush_ai::oauth::OAuthError::TokenExchange("nope".into())) },
        ));
        let drained = drain_when_settled(&mut session).await;
        assert!(!drained.saved, "a failure must not trigger a model refresh");
        assert!(session.app.pending_browser_login.is_none());
        let message = session.app.messages.last().unwrap().content.clone();
        assert!(message.contains("nope"), "got {message:?}");
        assert!(
            message.contains("/login"),
            "should point at retrying, got {message:?}"
        );
    }

    fn template(name: &str, description: &str) -> mush_ext::PromptTemplate {
        mush_ext::PromptTemplate {
            name: name.into(),
            description: Some(description.into()),
            content: format!("template body: {name}"),
            source: mush_ext::TemplateSource::User,
            path: std::path::PathBuf::from(format!("/tmp/{name}.md")),
        }
    }

    fn pane_with_existing_template() -> Session {
        let app = App::new(ModelId::from("test"), TokenCount::new(1024));
        let mut pane = Session::new(app);
        pane.app.completion.completions.push("/help".into());
        pane.app.completion.slash_commands.push(SlashCommand {
            name: "help".into(),
            description: "show help".into(),
        });
        pane.app.completion.completions.push("/old-template".into());
        pane.app.completion.slash_commands.push(SlashCommand {
            name: "old-template".into(),
            description: "stale".into(),
        });
        pane.app
            .completion
            .templates
            .push(template("old-template", "stale"));
        pane
    }

    #[tokio::test]
    async fn mcp_without_any_configured_servers_points_at_the_config_file() {
        let report = mcp_report(None, &slash::McpQuery::Status).await;
        assert!(report.contains("config.toml"), "got {report}");
    }

    #[tokio::test]
    async fn mcp_status_of_an_empty_manager_says_nothing_is_configured() {
        let manager = mush_mcp::McpManager::connect_all(&std::collections::HashMap::new()).await;
        let report = mcp_report(Some(&manager), &slash::McpQuery::Status).await;
        assert_eq!(report, "no MCP servers configured");
    }

    #[tokio::test]
    async fn mcp_reconnect_of_an_unknown_server_says_so() {
        let manager = mush_mcp::McpManager::connect_all(&std::collections::HashMap::new()).await;
        let report = mcp_report(
            Some(&manager),
            &slash::McpQuery::Reconnect {
                server: "ghost".into(),
            },
        )
        .await;
        assert!(report.contains("ghost"), "got {report}");
    }

    #[tokio::test]
    async fn mcp_tools_of_an_empty_manager_names_the_requested_server() {
        let manager = mush_mcp::McpManager::connect_all(&std::collections::HashMap::new()).await;
        let report = mcp_report(
            Some(&manager),
            &slash::McpQuery::Tools {
                server: Some("sweethome3d".into()),
            },
        )
        .await;
        assert!(report.contains("sweethome3d"), "got {report}");
    }

    #[test]
    fn mcp_is_listed_in_the_slash_menu() {
        assert!(
            super::super::runtime::BUILTIN_SLASH_COMMANDS
                .iter()
                .any(|(name, _)| *name == "mcp"),
            "/mcp should be discoverable in the slash menu"
        );
    }

    #[test]
    fn apply_reloaded_templates_replaces_template_entries_only() {
        let mut session = pane_with_existing_template();
        let new_templates = vec![
            template("review", "review code"),
            template("plan", "plan it"),
        ];

        apply_reloaded_templates(&mut session, &new_templates);

        let app = &session.app;
        let names: Vec<&str> = app
            .completion
            .templates
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(names, vec!["review", "plan"]);
        assert!(
            !app.completion
                .completions
                .iter()
                .any(|c| c == "/old-template"),
            "stale template completion should be cleared"
        );
        assert!(
            app.completion.completions.iter().any(|c| c == "/help"),
            "built-in slash commands must survive a /reload"
        );
        assert!(
            app.completion.completions.iter().any(|c| c == "/review"),
            "newly-discovered templates should be added as completions"
        );
    }
}
