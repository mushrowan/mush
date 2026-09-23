use std::path::Path;

use mush_ai::types::ThinkingLevel;

use crate::session::Session;
use crate::slash::{self, SlashAction};

use super::{ThinkingPrefs, TuiConfig};

pub(super) struct SlashEnv<'a> {
    pub tui_config: &'a mut TuiConfig,
    pub thinking_prefs: &'a ThinkingPrefs,
    pub cwd: &'a Path,
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

async fn send_schedule(
    session: &mut Session,
    tui_config: &mut TuiConfig,
    command: mush_runtime::ScheduleCommand,
) -> bool {
    match session
        .connection
        .send(mush_runtime::ClientCommand::Schedule(command))
        .await
    {
        Ok(events) => {
            super::looping::apply_command_events(session, tui_config, events).await;
            true
        }
        Err(error) => {
            session.app.push_system_message(error.to_string());
            false
        }
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
        text.push_str(&format!(
            "\ngates ({} retries, {}s each): {}",
            status.gates.max_retries,
            status.gates.timeout_ms / 1000,
            status.gates.commands.join(" ; ")
        ));
    }
    if let Some(failure) = &status.last_failure {
        text.push_str(&format!(
            "\nlast gate failure: `{}` {} (attempt {})",
            failure.command, failure.exit_text, failure.attempt
        ));
    }
    if let Some(reason) = &status.stopped {
        text.push_str(&format!("\nstopped: {reason}"));
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
        text.push_str(&format!(" of {budget}"));
    }
    if let Some(summary) = &goal.summary {
        text.push_str(&format!("\nsummary: {summary}"));
    }
    if let Some(error) = &goal.error {
        text.push_str(&format!("\nerror: {error}"));
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
        thinking_prefs,
        cwd,
        pending_prompt,
    } = env;
    let mut state_changed = false;
    let changes_conversation = matches!(
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
    if changes_conversation
        && state.controlled
        && (state.turn_active
            || session.app.stream.active
            || !session.app.active_tools.is_empty()
            || state.pending_prompt
            || pending_prompt.is_some()
            || state.compacting)
    {
        session.app.status = Some("wait for the active turn before switching conversation".into());
        return false;
    }
    match action {
        SlashAction::Subagents { args } => {
            super::subagents::command(&mut session.app, tui_config, &args).await;
        }
        SlashAction::Compact { focus } => {
            match session
                .connection
                .send(mush_runtime::ClientCommand::Compact { focus, auto: false })
                .await
            {
                Ok(events) => {
                    super::looping::apply_command_events(session, tui_config, events).await;
                    state_changed = true;
                }
                Err(error) => session.app.push_system_message(error.to_string()),
            }
        }
        SlashAction::Refine(request) => {
            match session
                .connection
                .send(mush_runtime::ClientCommand::Refine(request))
                .await
            {
                Ok(events) => {
                    super::looping::apply_command_events(session, tui_config, events).await;
                }
                Err(error) => session.app.push_system_message(error.to_string()),
            }
        }
        SlashAction::Side { question } => {
            slash::side_question::ask_side_question(&mut session.app, &question);
            let _ = session
                .connection
                .send(mush_runtime::ClientCommand::SideQuestion(question))
                .await;
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
        }
        SlashAction::Autonomous(action) => {
            let command = match action {
                slash::AutonomousAction::Status => {
                    session
                        .app
                        .push_system_message(format_autonomous(session.app.autonomous.as_ref()));
                    return false;
                }
                slash::AutonomousAction::On(overrides) => {
                    let (limits, gates, subagent_keep_alive_ms) =
                        overrides.resolve(&tui_config.autonomous);
                    mush_runtime::AutonomousCommand::On {
                        limits,
                        gates,
                        subagent_keep_alive_ms,
                    }
                }
                slash::AutonomousAction::Off => mush_runtime::AutonomousCommand::Off,
            };
            match session
                .connection
                .send(mush_runtime::ClientCommand::Autonomous(command))
                .await
            {
                Ok(events) => {
                    super::looping::apply_command_events(session, tui_config, events).await;
                    session
                        .app
                        .push_system_message(format_autonomous(session.app.autonomous.as_ref()));
                }
                Err(error) => session.app.push_system_message(error.to_string()),
            }
        }
        SlashAction::Heartbeat(action) => {
            let command = match action {
                slash::HeartbeatAction::Status => {
                    session
                        .app
                        .push_system_message(format_heartbeat(session.app.heartbeat()));
                    return false;
                }
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
            };
            send_schedule(session, tui_config, command).await;
            session
                .app
                .push_system_message(format_heartbeat(session.app.heartbeat()));
        }
        SlashAction::Schedule(action) => {
            let command = match action {
                slash::ScheduleAction::List => {
                    session
                        .app
                        .push_system_message(format_jobs(&session.app.jobs));
                    return false;
                }
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
            };
            if send_schedule(session, tui_config, command).await {
                session
                    .app
                    .push_system_message(format_jobs(&session.app.jobs));
            }
        }
        SlashAction::Goal(action) => {
            let command = match action {
                slash::GoalAction::Status => {
                    session
                        .app
                        .push_system_message(format_goal(session.app.goal.as_ref()));
                    return false;
                }
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
                slash::GoalAction::Complete => {
                    mush_runtime::GoalCommand::Complete { summary: None }
                }
                slash::GoalAction::Fail { error } => mush_runtime::GoalCommand::Fail { error },
            };
            match session
                .connection
                .send(mush_runtime::ClientCommand::Goal(command))
                .await
            {
                Ok(events) => {
                    super::looping::apply_command_events(session, tui_config, events).await;
                }
                Err(error) => session.app.push_system_message(error.to_string()),
            }
        }
        SlashAction::Export { path } => {
            let pane = &mut *session;
            slash::handle_export(
                &mut pane.app,
                &pane.connection.history().await,
                path.as_deref().unwrap_or(""),
            );
        }
        SlashAction::LoginComplete { code } => {
            let pane = &mut *session;
            let logged_in = complete_login(&mut pane.app, code).await;
            if logged_in {
                refresh_models_after_login(session, tui_config).await;
            }
        }
        SlashAction::Login { provider } => {
            let pane = &mut *session;
            slash::handle_login_start(&mut pane.app, tui_config, provider.as_deref()).await;
        }
        SlashAction::Credits => {
            handle_credits(session, tui_config).await;
        }
        SlashAction::Endpoints { model_id } => {
            handle_endpoints(session, tui_config, model_id.as_deref()).await;
        }
        SlashAction::Mcp(query) => {
            let report = mcp_report(tui_config.mcp.as_deref(), &query).await;
            session.app.push_system_message(report);
        }
        SlashAction::New { name, prompt } => {
            let created =
                run_session_command(session, tui_config, mush_runtime::SessionCommand::New).await;
            state_changed = true;
            if !created {
                if let Some(prompt) = prompt {
                    session.app.input.text = prompt;
                    session.app.input.cursor = session.app.input.text.len();
                }
                return state_changed;
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
        SlashAction::Fork { index: Some(index) } => {
            state_changed = run_session_command(
                session,
                tui_config,
                mush_runtime::SessionCommand::Fork {
                    before_user_message: index,
                },
            )
            .await;
        }
        SlashAction::Clone => {
            run_session_command(session, tui_config, mush_runtime::SessionCommand::Clone).await;
            state_changed = true;
        }
        SlashAction::Clear => {
            run_session_command(session, tui_config, mush_runtime::SessionCommand::Clear).await;
        }
        SlashAction::Undo => {
            state_changed =
                run_session_command(session, tui_config, mush_runtime::SessionCommand::Undo).await;
        }
        SlashAction::NavigateTree { entry_id } => {
            let leaves_something_behind = session.connection.navigation_impact(&entry_id).await > 0;
            if leaves_something_behind && !tui_config.branch_summary_skip_prompt {
                session.app.interaction.branch_summary_ask = Some(entry_id);
                session.app.interaction.mode = crate::app::AppMode::ToolConfirm;
            } else {
                state_changed = run_session_command(
                    session,
                    tui_config,
                    mush_runtime::SessionCommand::NavigateTree {
                        entry_id,
                        summary: None,
                    },
                )
                .await;
            }
        }
        SlashAction::Branch { index: Some(index) } => {
            state_changed = run_session_command(
                session,
                tui_config,
                mush_runtime::SessionCommand::Branch {
                    user_message: index,
                },
            )
            .await;
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
        }
        SlashAction::Resume { session_id } => {
            state_changed = run_session_command(
                session,
                tui_config,
                mush_runtime::SessionCommand::Resume { id: session_id },
            )
            .await;
        }
        SlashAction::Reload => {
            let Some(reload) = tui_config.reload_context.clone() else {
                session
                    .app
                    .push_system_message("/reload not supported in this build".to_string());
                return false;
            };
            let reloaded = reload(cwd);
            let _ = session
                .connection
                .send(mush_runtime::ClientCommand::ReloadSystemPrompt)
                .await;
            apply_reloaded_templates(session, &reloaded.templates);
            apply_reloaded_skills(session, &reloaded.skills.skills);
            let warnings = reloaded.skills.diagnostics.len();
            tui_config.skills = reloaded.skills;
            session.app.status = Some(crate::app_state::Status::from(format!(
                "reloaded AGENTS.md, {n} prompt template{s}, {k} skill{ks}{w}",
                n = reloaded.templates.len(),
                s = if reloaded.templates.len() == 1 {
                    ""
                } else {
                    "s"
                },
                k = tui_config.skills.skills.len(),
                ks = if tui_config.skills.skills.len() == 1 {
                    ""
                } else {
                    "s"
                },
                w = if warnings == 0 {
                    String::new()
                } else {
                    format!(" ({warnings} warnings, /skills)")
                },
            )));
        }
        SlashAction::RefreshModels => {
            let client = tui_config
                .http_client
                .clone()
                .unwrap_or_else(mush_ai::http::client);
            session.app.status = Some("refreshing models…".into());
            let summary = mush_ai::discovery::refresh_and_save(client).await;
            let line = summary.one_line();
            for pane in std::slice::from_mut(session) {
                crate::input::reload_model_completions(&mut pane.app);
            }
            session
                .app
                .push_system_message(format!("model discovery: {line}"));
            session.app.status = None;
        }
        other_action => {
            let pane = &mut *session;
            if let Some(prompt) = slash::handle(
                &mut pane.app,
                pane.connection.as_mut(),
                tui_config,
                thinking_prefs,
                &other_action,
            )
            .await
            {
                let app = &mut pane.app;
                app.start_streaming();
                *pending_prompt = Some(prompt);
            }
        }
    }

    state_changed
}

fn apply_reloaded_templates(session: &mut Session, templates: &[mush_ext::PromptTemplate]) {
    for pane in std::slice::from_mut(session) {
        let template_names: std::collections::HashSet<&str> = pane
            .app
            .completion
            .templates
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        pane.app
            .completion
            .completions
            .retain(|c| !template_names.contains(c.trim_start_matches('/')));
        pane.app
            .completion
            .slash_commands
            .retain(|c| !template_names.contains(c.name.as_str()));

        pane.app.completion.templates.clear();
        for template in templates {
            pane.app
                .completion
                .completions
                .push(format!("/{}", template.name));
            pane.app
                .completion
                .slash_commands
                .push(crate::app::SlashCommand {
                    name: template.name.clone(),
                    description: template.description.clone().unwrap_or_default(),
                });
            pane.app.completion.templates.push(template.clone());
        }
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

pub(super) fn save_thinking_pref(
    prefs: &mut ThinkingPrefs,
    saver: &Option<super::ThinkingPrefsSaver>,
    cwd: &Path,
    model_id: &str,
    level: ThinkingLevel,
) {
    prefs.set(cwd.to_path_buf(), model_id.to_string(), level);
    if let Some(saver) = saver {
        saver(prefs);
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
        Ok(creds) => {
            save_login_credentials(app, &pending.provider_id, &pending.provider_name, creds)
        }
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
    creds: mush_ai::oauth::OAuthCredentials,
) -> bool {
    use mush_ai::oauth;

    let mut store = oauth::load_credentials().unwrap_or_default();
    store.providers.insert(provider_id.to_string(), creds);
    match oauth::save_credentials(&store) {
        Ok(()) => {
            app.push_system_message(format!(
                "✓ logged in to {provider_name} (credentials saved)"
            ));
            true
        }
        Err(e) => {
            app.push_system_message(format!("logged in but saving credentials failed: {e}"));
            false
        }
    }
}

pub(super) async fn refresh_models_after_login(session: &mut Session, tui_config: &TuiConfig) {
    let client = tui_config
        .http_client
        .clone()
        .unwrap_or_else(mush_ai::http::client);
    session.app.status = Some("refreshing models…".into());
    let summary = mush_ai::discovery::refresh_and_save(client).await;
    let line = summary.one_line();
    for pane in std::slice::from_mut(session) {
        crate::input::reload_model_completions(&mut pane.app);
    }
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
    let mut drained = DrainedLogins::default();
    for pane in std::slice::from_mut(session) {
        let Some(mut pending) = pane.app.pending_browser_login.take() else {
            continue;
        };
        let Some(outcome) = pending.poll() else {
            pane.app.pending_browser_login = Some(pending);
            continue;
        };
        drained.settled = true;
        match outcome {
            Ok(creds) => {
                drained.saved |= save_login_credentials(
                    &mut pane.app,
                    &pending.provider_id,
                    &pending.provider_name,
                    creds,
                );
            }
            Err(reason) => {
                pane.app.push_system_message(format!(
                    "{} login failed: {reason}\nrun /login to try again",
                    pending.provider_name
                ));
            }
        }
        pane.app.status = None;
        if let Some(picker) = pane.app.interaction.login_picker.as_mut() {
            picker.rebuild_sources();
        }
    }
    drained
}

async fn handle_credits(pane: &mut crate::session::Session, tui_config: &TuiConfig) {
    use mush_ai::types::Provider;

    let openrouter_model = mush_ai::types::Model {
        provider: Provider::OpenRouter,
        ..tui_config.model.clone()
    };
    let (key, _) = crate::event_handler::resolve_auth_for_model(
        &openrouter_model,
        &tui_config.provider_api_keys,
    )
    .await;
    let Some(key) = key else {
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
    use mush_ai::types::Provider;

    let openrouter_model = mush_ai::types::Model {
        provider: Provider::OpenRouter,
        ..tui_config.model.clone()
    };
    let (key, _) = crate::event_handler::resolve_auth_for_model(
        &openrouter_model,
        &tui_config.provider_api_keys,
    )
    .await;
    let Some(key) = key else {
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
                report.push_str(&format!(
                    "\n  · {} (ctx {}){}",
                    ep.provider_name, ep.context_length, cache_badge
                ));
            }
            if let Some(unique) = endpoints.unique_caching_provider() {
                report.push_str(&format!("\n→ sole caching provider: {unique}"));
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
                thinking_prefs: &ThinkingPrefs::default(),
                cwd: std::path::Path::new("/tmp"),
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
        assert_eq!(
            session.in_process().runtime.conversation.context(),
            vec![user("first")]
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
                name: Some("port prime".into()),
                prompt: Some("fix it".into()),
            },
            SlashEnv {
                tui_config: &mut config,
                thinking_prefs: &ThinkingPrefs::default(),
                cwd: std::path::Path::new("/tmp"),
                pending_prompt: &mut pending_prompt,
            },
        ));

        assert_ne!(config.session_id, old_id);
        assert_eq!(session.app.session_title.as_deref(), Some("port prime"));
        assert_eq!(pending_prompt.as_deref(), Some("fix it"));
        let last = session.app.messages.last().expect("the prompt shows");
        assert_eq!(last.role, crate::app::MessageRole::User);
        assert_eq!(last.content, "fix it");
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
            status: mush_runtime::ChildStatus::Idle,
            spawned_at: Timestamp::zero(),
            updated_at: Timestamp::zero(),
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
                title: Some("  port\tprime  ".into()),
            },
        );
        assert_eq!(session.app.session_title.as_deref(), Some("port prime"));

        run(
            &mut session,
            &mut config,
            SlashAction::Name {
                title: Some("\x07".into()),
            },
        );
        assert_eq!(session.app.session_title.as_deref(), Some("port prime"));
    }

    #[test]
    fn undo_and_branch_rewind_the_transcript() {
        let mut session = conversation_session(vec![user("a"), user("b"), user("c")]);
        let mut config = TuiConfig::for_test();

        run(&mut session, &mut config, SlashAction::Undo);
        assert_eq!(session.in_process().runtime.conversation.context_len(), 2);

        run(
            &mut session,
            &mut config,
            SlashAction::Branch { index: Some(1) },
        );
        assert_eq!(session.in_process().runtime.conversation.context_len(), 0);
        assert!(session.app.messages.is_empty());
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
            stored.providers["anthropic"].access_token, "access-token",
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
