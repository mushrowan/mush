use std::path::Path;

use mush_ai::registry::ApiRegistry;
use mush_ai::types::{Dollars, Message, ThinkingLevel, TokenCount};

use crate::file_tracker::FileTracker;
use crate::pane::PaneManager;
use crate::slash::{self, SlashAction};

use super::panes::close_focused_pane;
use super::{ThinkingPrefs, TuiConfig};

pub(super) struct SlashEnv<'a> {
    pub tui_config: &'a mut TuiConfig,
    pub thinking_prefs: &'a ThinkingPrefs,
    pub registry: &'a ApiRegistry,
    pub message_bus: &'a crate::messaging::MessageBus,
    pub file_tracker: &'a FileTracker,
    pub lifecycle_hooks: &'a mush_agent::LifecycleHooks,
    pub cwd: &'a Path,
    pub pending_prompt: &'a mut Option<String>,
    pub pending_compactions:
        &'a mut std::collections::HashMap<crate::pane::PaneId, crate::slash::PendingCompaction>,
}

pub(super) async fn handle_slash_action(
    pane_mgr: &mut PaneManager,
    action: SlashAction,
    env: SlashEnv<'_>,
) -> bool {
    let SlashEnv {
        tui_config,
        thinking_prefs,
        registry,
        message_bus,
        file_tracker,
        lifecycle_hooks,
        cwd,
        pending_prompt,
        pending_compactions,
    } = env;
    let mut state_changed = false;

    match action {
        SlashAction::Compact { context_budget } => {
            let pane_id = pane_mgr.focused().id;
            let messages = pane_mgr.focused().conversation.context();
            if messages.len() <= slash::MIN_MESSAGES_FOR_COMPACTION {
                pane_mgr
                    .focused_mut()
                    .app
                    .push_system_message("conversation too short to compact");
            } else {
                use std::collections::hash_map::Entry;
                match pending_compactions.entry(pane_id) {
                    Entry::Occupied(_) => {
                        pane_mgr
                            .focused_mut()
                            .app
                            .push_system_message("compaction already in progress on this pane");
                    }
                    Entry::Vacant(slot) => {
                        let target =
                            super::streams::compaction_target(pane_mgr, pane_id, tui_config).await;
                        let pane = pane_mgr.focused_mut();
                        pane.app.status = Some("compacting…".into());
                        pane.app.compacting = true;
                        let task = slash::start_compaction(
                            messages,
                            target.model,
                            target.options,
                            registry.clone(),
                            Some(lifecycle_hooks.clone()),
                            Some(cwd.to_path_buf()),
                            context_budget,
                        );
                        slot.insert(slash::PendingCompaction { task });
                        state_changed = true;
                    }
                }
            }
        }
        SlashAction::Export { path } => {
            let pane = pane_mgr.focused_mut();
            slash::handle_export(
                &mut pane.app,
                &pane.conversation,
                path.as_deref().unwrap_or(""),
            );
        }
        SlashAction::LoginComplete { code } => {
            let pane = pane_mgr.focused_mut();
            let logged_in = complete_login(&mut pane.app, code).await;
            if logged_in {
                refresh_models_after_login(pane_mgr, tui_config).await;
            }
        }
        SlashAction::Login { provider } => {
            let pane = pane_mgr.focused_mut();
            slash::handle_login_start(&mut pane.app, tui_config, provider.as_deref()).await;
        }
        SlashAction::Credits => {
            handle_credits(pane_mgr.focused_mut(), tui_config).await;
        }
        SlashAction::Endpoints { model_id } => {
            handle_endpoints(pane_mgr.focused_mut(), tui_config, model_id.as_deref()).await;
        }
        SlashAction::Broadcast { message } => {
            if !pane_mgr.is_multi_pane() {
                pane_mgr
                    .focused_mut()
                    .app
                    .push_system_message("no sibling panes to broadcast to");
            } else {
                let from = pane_mgr.focused().id;
                let stats = message_bus.broadcast(from, message);
                let summary = if stats.dropped == 0 {
                    format!("broadcast sent to {} pane(s)", stats.sent)
                } else {
                    format!(
                        "broadcast sent to {} pane(s), {} dropped",
                        stats.sent, stats.dropped
                    )
                };
                pane_mgr.focused_mut().app.push_system_message(summary);
            }
        }
        SlashAction::Lock { path } => {
            let pane_id = pane_mgr.focused().id;
            match file_tracker.lock(pane_id, &path) {
                Ok(()) => {
                    pane_mgr.focused_mut().app.status = Some(format!("locked {path}"));
                }
                Err(owner) => {
                    pane_mgr.focused_mut().app.status =
                        Some(format!("already locked by pane {}", owner.as_u32()));
                }
            }
        }
        SlashAction::Unlock { path } => {
            let pane_id = pane_mgr.focused().id;
            if file_tracker.unlock(pane_id, &path) {
                pane_mgr.focused_mut().app.status = Some(format!("unlocked {path}"));
            } else {
                pane_mgr.focused_mut().app.status = Some("not locked by this pane".into());
            }
        }
        SlashAction::Locks => {
            let locks = file_tracker.list_locks();
            if locks.is_empty() {
                pane_mgr
                    .focused_mut()
                    .app
                    .push_system_message("no file locks active");
            } else {
                let mut msg = String::from("file locks:\n");
                for (path, owner) in &locks {
                    msg.push_str(&format!("  {} (pane {})\n", path.display(), owner.as_u32()));
                }
                pane_mgr
                    .focused_mut()
                    .app
                    .push_system_message(msg.trim_end().to_string());
            }
        }
        SlashAction::Label { text } => {
            let pane_id = pane_mgr.focused().id;
            let label = match text {
                Some(label) => label,
                None => pane_mgr
                    .focused()
                    .conversation
                    .context()
                    .into_iter()
                    .find_map(|message| match message {
                        Message::User(user) => {
                            let text = user.text();
                            if text.is_empty() {
                                None
                            } else {
                                Some(text.chars().take(30).collect::<String>())
                            }
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| format!("pane {}", pane_id.as_u32())),
            };
            pane_mgr.focused_mut().label = Some(label.clone());
            pane_mgr.focused_mut().app.status = Some(format!("label: {label}"));
        }
        SlashAction::Panes => {
            let mut msg = String::from("active panes:\n");
            for (i, pane) in pane_mgr.panes().iter().enumerate() {
                let idx = i + 1;
                let label = pane.label.as_deref().unwrap_or("(unlabelled)");
                let status = if pane.app.stream.active {
                    "streaming"
                } else {
                    "idle"
                };
                let model = &pane.app.model_id;
                let cost = if pane.app.stats.total_cost > Dollars::ZERO {
                    format!(" {}", pane.app.stats.total_cost)
                } else {
                    String::new()
                };
                let focused = if i == pane_mgr.focused_index() {
                    " *"
                } else {
                    ""
                };
                msg.push_str(&format!(
                    "  {idx}. {label} [{status}] {model}{cost}{focused}\n"
                ));
            }
            pane_mgr
                .focused_mut()
                .app
                .push_system_message(msg.trim_end().to_string());
        }
        SlashAction::Cost if pane_mgr.is_multi_pane() => {
            let focused = &mut pane_mgr.focused_mut().app;
            focused.interaction.show_cost = !focused.interaction.show_cost;
            let show = focused.interaction.show_cost;
            let mut total_cost = Dollars::ZERO;
            let mut total_tokens = TokenCount::ZERO;
            let mut lines = Vec::new();
            for (i, pane) in pane_mgr.panes().iter().enumerate() {
                let idx = i + 1;
                let label = pane.label.as_deref().unwrap_or("(unlabelled)");
                let stats = &pane.app.stats;
                total_cost += stats.total_cost;
                total_tokens += stats.total_tokens;
                if stats.total_tokens > TokenCount::ZERO {
                    lines.push(format!(
                        "  pane {idx} ({label}): {}tok {}",
                        stats.total_tokens, stats.total_cost
                    ));
                }
            }
            if show {
                let mut msg = format!("total: {}tok {}\n", total_tokens, total_cost);
                for line in &lines {
                    msg.push_str(line);
                    msg.push('\n');
                }
                pane_mgr
                    .focused_mut()
                    .app
                    .push_system_message(msg.trim_end().to_string());
            }
            for pane in pane_mgr.panes_mut() {
                pane.app.interaction.show_cost = show;
            }
        }
        SlashAction::New => {
            // save current session before clearing
            if let Some(ref saver) = tui_config.save_session {
                saver(super::streams::build_session_snapshot(pane_mgr, tui_config));
            }
            // start a new session with a fresh id
            let new_id = mush_ai::types::SessionId::new();
            tui_config.session_id = new_id.clone();
            // keep the duplicated `options.session_id` consistent (see
            // /resume handler in slash/commands.rs for the rationale)
            tui_config.options.session_id = Some(new_id);
            let pane = pane_mgr.focused_mut();
            let (app, conversation, _) = pane.fields_mut();
            app.clear_messages();
            *conversation = mush_session::ConversationState::new();
            app.status = Some("new session started (previous session saved)".into());
            state_changed = true;
        }
        SlashAction::Reload => {
            let Some(reload) = tui_config.reload_context.clone() else {
                pane_mgr
                    .focused_mut()
                    .app
                    .push_system_message("/reload not supported in this build".to_string());
                return false;
            };
            let reloaded = reload(cwd);
            tui_config.system_prompt = Some(reloaded.system_prompt);
            apply_reloaded_templates(pane_mgr, &reloaded.templates);
            pane_mgr.focused_mut().app.status = Some(format!(
                "reloaded AGENTS.md and {n} prompt template{s}",
                n = reloaded.templates.len(),
                s = if reloaded.templates.len() == 1 {
                    ""
                } else {
                    "s"
                },
            ));
        }
        SlashAction::RefreshModels => {
            let client = tui_config
                .http_client
                .clone()
                .unwrap_or_else(mush_ai::http::client);
            pane_mgr.focused_mut().app.status = Some("refreshing models…".into());
            let summary = mush_ai::discovery::refresh_and_save(client).await;
            let line = summary.one_line();
            for pane in pane_mgr.panes_mut() {
                crate::input::reload_model_completions(&mut pane.app);
            }
            pane_mgr
                .focused_mut()
                .app
                .push_system_message(format!("model discovery: {line}"));
            pane_mgr.focused_mut().app.status = None;
        }
        SlashAction::Close => {
            close_focused_pane(pane_mgr, None, message_bus, file_tracker, cwd).await;
        }
        SlashAction::Merge => {
            let pane = pane_mgr.focused();
            match &pane.isolation {
                Some(crate::isolation::PaneIsolation::Worktree { .. }) => {
                    let pane_id = pane.id;
                    match crate::isolation::merge_worktree(cwd, pane_id).await {
                        Ok(info) => {
                            let summary = match info.summary {
                                Some(summary) => format!("merged {}: {summary}", info.branch),
                                None => format!("merged {}", info.branch),
                            };
                            pane_mgr.focused_mut().app.push_system_message(summary);
                        }
                        Err(error) => {
                            pane_mgr
                                .focused_mut()
                                .app
                                .push_system_message(format!("merge failed: {error}"));
                        }
                    }
                }
                Some(crate::isolation::PaneIsolation::Jj { change_id }) => {
                    let change_id = change_id.clone();
                    match crate::isolation::squash_jj_change(cwd, &change_id).await {
                        Ok(info) => {
                            let summary = match info.summary {
                                Some(summary) => format!("squashed jj change: {summary}"),
                                None => "squashed jj change".into(),
                            };
                            pane_mgr.focused_mut().app.push_system_message(summary);
                            pane_mgr.focused_mut().isolation = None;
                        }
                        Err(error) => {
                            pane_mgr
                                .focused_mut()
                                .app
                                .push_system_message(format!("squash failed: {error}"));
                        }
                    }
                }
                None => {
                    pane_mgr.focused_mut().app.status =
                        Some("no isolation to merge (mode is none)".into());
                }
            }
        }
        other_action => {
            let pane = pane_mgr.focused_mut();
            let (app, conversation, _) = pane.fields_mut();
            if let Some(prompt) =
                slash::handle(app, conversation, tui_config, thinking_prefs, &other_action)
            {
                app.start_streaming();
                *pending_prompt = Some(prompt);
            }
            if matches!(other_action, SlashAction::Undo | SlashAction::Branch { .. }) {
                state_changed = true;
            }
        }
    }

    state_changed
}

/// replace each pane's discovered prompt-template entries (the ones
/// preceded by `/` in the slash menu and the @-trigger expansion list)
/// with a freshly-discovered set. preserves the built-in slash commands
/// because those don't change at runtime
fn apply_reloaded_templates(pane_mgr: &mut PaneManager, templates: &[mush_ext::PromptTemplate]) {
    for pane in pane_mgr.panes_mut() {
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

/// finish the oauth flow started via /login: exchange the user-supplied
/// code for credentials using the PKCE challenge stashed on the app.
/// on success, save credentials so future runs can pick them up.
///
/// returns `true` when credentials were successfully exchanged and
/// saved, so the caller can kick off a discovery refresh to pull in
/// any models the just-logged-in subscription exposes
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
        // keep pending_oauth so they can retry
        app.pending_oauth = Some(pending);
        return false;
    }

    match provider.exchange_code(trimmed, &pending.pkce).await {
        Ok(creds) => {
            save_login_credentials(app, &pending.provider_id, &pending.provider_name, creds)
        }
        Err(e) => {
            app.push_system_message(format!("oauth login failed: {e}"));
            // restore pending state so user can try a different code
            app.pending_oauth = Some(pending);
            false
        }
    }
}

/// persist freshly exchanged oauth credentials and report the outcome in
/// the pane. returns whether the save succeeded, which is also the
/// signal to refresh the model catalogue
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

/// pull the model catalogue again after a successful login so any models
/// the fresh subscription unlocks show up in the picker straight away
pub(super) async fn refresh_models_after_login(pane_mgr: &mut PaneManager, tui_config: &TuiConfig) {
    let client = tui_config
        .http_client
        .clone()
        .unwrap_or_else(mush_ai::http::client);
    pane_mgr.focused_mut().app.status = Some("refreshing models…".into());
    let summary = mush_ai::discovery::refresh_and_save(client).await;
    let line = summary.one_line();
    for pane in pane_mgr.panes_mut() {
        crate::input::reload_model_completions(&mut pane.app);
    }
    pane_mgr
        .focused_mut()
        .app
        .push_system_message(format!("model discovery: {line}"));
    pane_mgr.focused_mut().app.status = None;
}

/// what a single drain pass settled
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct DrainedLogins {
    /// credentials landed, so the model catalogue is worth refreshing
    pub saved: bool,
    /// a flow finished either way, so the pane needs a redraw
    pub settled: bool,
}

/// take the outcome of any finished browser login, save its credentials
/// and report it in the pane that started the flow
pub(super) fn drain_finished_browser_logins(pane_mgr: &mut PaneManager) -> DrainedLogins {
    let mut drained = DrainedLogins::default();
    for pane in pane_mgr.panes_mut() {
        let Some(mut pending) = pane.app.pending_browser_login.take() else {
            continue;
        };
        let Some(outcome) = pending.poll() else {
            // still waiting on the browser, put it back untouched
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

/// resolve openrouter credentials and fetch the account's credit
/// balance. on failure (no key, network error, etc.) push a
/// diagnostic system message rather than panicking; the user can
/// retry without losing chat state
async fn handle_credits(pane: &mut crate::pane::Pane, tui_config: &TuiConfig) {
    use mush_ai::types::Provider;

    // /credits is openrouter-specific; signal cleanly when invoked
    // on a different provider so the user knows why nothing happened
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

/// resolve openrouter credentials and fetch the per-provider endpoint
/// list for a model. defaults to the pane's current model id when no
/// argument is supplied. on failure pushes a diagnostic system message
/// instead of panicking; this is a read-only inspection command
async fn handle_endpoints(
    pane: &mut crate::pane::Pane,
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
    use crate::pane::{Pane, PaneId, PaneManager};
    use mush_ai::oauth::OAuthCredentials;
    use mush_ai::types::{ModelId, TokenCount};

    fn credentials() -> OAuthCredentials {
        OAuthCredentials {
            access_token: "access-token".into(),
            refresh_token: "refresh-token".into(),
            expires_at: u64::MAX,
            account_id: Some("acct_1".into()),
        }
    }

    fn pane_manager() -> PaneManager {
        let app = App::new(ModelId::from("test"), TokenCount::new(1024));
        PaneManager::new(Pane::new(PaneId::new(1), app))
    }

    /// drain once the spawned flow has an outcome to report
    async fn drain_when_settled(pane_mgr: &mut PaneManager) -> DrainedLogins {
        for _ in 0..100 {
            let drained = drain_finished_browser_logins(pane_mgr);
            if drained.settled {
                return drained;
            }
            tokio::task::yield_now().await;
        }
        panic!("login never settled");
    }

    #[tokio::test]
    async fn a_finished_browser_login_saves_credentials_without_login_complete() {
        // the whole point of the browser flow: the user authorises and
        // mush picks the credentials up on its own
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process
        unsafe { std::env::set_var("MUSH_CONFIG_DIR", dir.path()) };

        let mut pane_mgr = pane_manager();
        pane_mgr.focused_mut().app.pending_browser_login = Some(PendingBrowserLogin::spawn(
            "anthropic",
            "Anthropic (Claude Pro/Max)",
            async { Ok(credentials()) },
        ));
        let drained = drain_when_settled(&mut pane_mgr).await;
        assert!(drained.saved, "credentials should be saved");

        let stored = mush_ai::oauth::load_credentials().expect("load credentials");
        assert_eq!(
            stored.providers["anthropic"].access_token, "access-token",
            "credentials should land in the oauth store"
        );
        assert!(
            pane_mgr.focused().app.pending_browser_login.is_none(),
            "the finished flow should be cleared"
        );
        let messages = pane_mgr
            .focused()
            .app
            .messages
            .last()
            .unwrap()
            .content
            .clone();
        assert!(
            messages.contains("Anthropic"),
            "the pane should say which provider logged in, got {messages:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_browser_login_reports_the_reason_and_offers_the_paste_fallback() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process
        unsafe { std::env::set_var("MUSH_CONFIG_DIR", dir.path()) };

        let mut pane_mgr = pane_manager();
        pane_mgr.focused_mut().app.pending_browser_login = Some(PendingBrowserLogin::spawn(
            "anthropic",
            "Anthropic (Claude Pro/Max)",
            async { Err(mush_ai::oauth::OAuthError::TokenExchange("nope".into())) },
        ));
        let drained = drain_when_settled(&mut pane_mgr).await;
        assert!(!drained.saved, "a failure must not trigger a model refresh");
        assert!(pane_mgr.focused().app.pending_browser_login.is_none());
        let message = pane_mgr
            .focused()
            .app
            .messages
            .last()
            .unwrap()
            .content
            .clone();
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

    fn pane_with_existing_template() -> Pane {
        let app = App::new(ModelId::from("test"), TokenCount::new(1024));
        let mut pane = Pane::new(PaneId::new(1), app);
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

    #[test]
    fn apply_reloaded_templates_replaces_template_entries_only() {
        // /reload swaps the discovered prompt-template entries for the
        // freshly-found set. existing built-in slash commands (here
        // /help) must survive untouched. stale templates must be
        // removed from completions, slash_commands, and templates lists
        let mut pane_mgr = PaneManager::new(pane_with_existing_template());
        let new_templates = vec![
            template("review", "review code"),
            template("plan", "plan it"),
        ];

        apply_reloaded_templates(&mut pane_mgr, &new_templates);

        let app = &pane_mgr.focused().app;
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
