pub mod commands;
pub mod compaction;
pub mod refine;
pub mod side_question;

use mush_ai::types::{SessionId, TokenCount, TokenCountParseError};
use thiserror::Error;

pub(crate) use commands::handle_login_start;
pub use commands::{expand_skill, expand_template, handle, handle_export, rebuild_display};
pub use compaction::{
    AppliedCompaction, CompactionTaskResult, MIN_MESSAGES_FOR_COMPACTION, PendingCompaction,
    apply_applied_compaction, apply_compaction_result, extract_fresh_summary,
    fallback_applied_compaction, handle_compact, run_compaction, show_applied_compaction,
    start_compaction,
};
pub use refine::format_refinement;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpQuery {
    Status,
    Tools { server: Option<String> },
    Schema { tool: String },
    Reconnect { server: String },
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SlashParseError {
    #[error("slash commands must start with /")]
    MissingPrefix,
    #[error("usage: /branch <number> (try /tree first)")]
    BranchUsage,
    #[error(
        "usage: /refine [--global] [instructions] | /refine rollback <refinement-id> [--global]"
    )]
    RefineUsage,
    #[error("usage: /fork <number> (try /tree first)")]
    ForkUsage,
    #[error("usage: /logs [n]")]
    LogsUsage,
    #[error("usage: /mcp | /mcp tools [server] | /mcp schema <tool> | /mcp reconnect <server>")]
    McpUsage,
    #[error("usage: /login-complete <code>")]
    LoginCodeUsage,
    #[error("usage: /btw <question>")]
    SideUsage,
    #[error(
        "usage: /goal [--budget N] <objective> | /goal status|pause|resume|complete|clear | /goal fail <reason>"
    )]
    GoalUsage,
    #[error(
        "usage: /heartbeat [every 10m] [--follow-up] <instruction> | /heartbeat status|pause|resume|clear"
    )]
    HeartbeatUsage,
    #[error(
        "usage: /autonomous on [--max-continuations N] [--max-turns N] [--max-tokens N] [--timeout-ms N] [--gate \"cmd\"]... [--gate-retries N] [--gate-timeout-ms N] | /autonomous status|off"
    )]
    AutonomousUsage,
    #[error(
        "usage: /schedule add \"in 30m\"|\"every 10m\"|\"0 9 * * *\" [--label name] <prompt> | /schedule list|pause|resume|cancel <id>"
    )]
    ScheduleUsage,
    #[error("usage: /rlm-max-depth [<non-negative integer>]")]
    RlmMaxDepthUsage,
    #[error("usage: /compaction-model [same|<model-id>]")]
    CompactionModelUsage,
    #[error(
        "usage: /compact [--context-budget <count>] [instructions] (budget e.g. 10k, 1024, 1.5k, 1m)"
    )]
    CompactUsage,
    #[error("{0}. usage: /new [--name <name> --] [prompt]")]
    NewUsage(String),
    #[error("invalid context budget: {0}")]
    InvalidContextBudget(#[from] TokenCountParseError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashAction {
    Subagents {
        args: String,
    },
    Agents,
    SystemPrompt,
    Help,
    Keys,
    Clear,
    New {
        name: Option<String>,
        prompt: Option<String>,
    },
    Model {
        model_id: Option<String>,
        show_all: bool,
    },
    SubagentModel {
        model_id: Option<String>,
        unset: bool,
    },
    Pause {
        reason: Option<String>,
    },
    Resumed,
    Sessions,
    Resume {
        session_id: SessionId,
    },
    Branch {
        index: Option<usize>,
    },
    Fork {
        index: Option<usize>,
    },
    Clone,
    Tree,
    NavigateTree {
        entry_id: mush_session::tree::EntryId,
    },
    Compact {
        focus: mush_session::compact::CompactionFocus,
    },
    Refine(mush_session::refine::RefineRequest),
    CompactionModel {
        model_id: Option<String>,
    },
    Export {
        path: Option<String>,
    },
    Undo,
    Search {
        query: String,
    },
    Cost,
    Context,
    Credits,
    Name {
        title: Option<String>,
    },
    Skills,
    SessionInfo,
    RlmMaxDepth {
        max_depth: Option<u32>,
    },
    Side {
        question: String,
    },
    Goal(GoalAction),
    Heartbeat(HeartbeatAction),
    Schedule(ScheduleAction),
    Autonomous(AutonomousAction),
    Endpoints {
        model_id: Option<String>,
    },
    Logs {
        count: usize,
    },
    Settings {
        args: String,
    },
    Login {
        provider: Option<String>,
    },
    LoginComplete {
        code: String,
    },
    Card,
    Mcp(McpQuery),
    Debug,
    Reload,
    RefreshModels,
    Quit,
    ToggleThinkingExpanded,
    Other {
        name: String,
        args: String,
    },
}

impl SlashAction {
    #[must_use]
    pub fn is_safe_during_stream(&self) -> bool {
        !matches!(
            self,
            Self::Clear
                | Self::New { .. }
                | Self::Resume { .. }
                | Self::Branch { .. }
                | Self::NavigateTree { .. }
                | Self::Fork { .. }
                | Self::Clone
                | Self::Compact { .. }
                | Self::Undo
                | Self::Reload
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalAction {
    Status,
    Set {
        objective: String,
        token_budget: Option<u64>,
    },
    Pause,
    Resume,
    Clear,
    Complete,
    Fail {
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeartbeatAction {
    Status,
    Set {
        expression: String,
        prompt: String,
        follow_up: bool,
    },
    Pause,
    Resume,
    Clear,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleAction {
    List,
    Add {
        expression: String,
        prompt: String,
        label: Option<String>,
    },
    Pause(String),
    Resume(String),
    Cancel(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutonomousAction {
    Status,
    On(mush_runtime::AutonomousOverrides),
    Off,
}

fn parse_budget_number(value: &str) -> Result<Option<u64>, SlashParseError> {
    if value.eq_ignore_ascii_case("unlimited") {
        return Ok(None);
    }
    value
        .replace(['_', ','], "")
        .parse::<u64>()
        .map(Some)
        .map_err(|_| SlashParseError::AutonomousUsage)
}

pub fn parse_autonomous_options(
    words: &[String],
) -> Result<mush_runtime::AutonomousOverrides, SlashParseError> {
    let mut named_budget = false;
    let mut overrides = mush_runtime::AutonomousOverrides::default();
    let mut budget = mush_runtime::Limits::unlimited();
    let mut index = 0;
    while index < words.len() {
        let word = words[index].as_str();
        let (flag, inline) = match word.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_string())),
            None => (word, None),
        };
        let flag = flag
            .strip_prefix("--autonomous-")
            .map(|rest| format!("--{rest}"))
            .unwrap_or_else(|| flag.to_string());
        let value = match &inline {
            Some(inline) => Some(inline.clone()),
            None => words.get(index + 1).cloned(),
        };
        let mut take = |what: &mut dyn FnMut(&str) -> Result<(), SlashParseError>| {
            let value = value.clone().ok_or(SlashParseError::AutonomousUsage)?;
            what(&value)?;
            if inline.is_none() {
                index += 1;
            }
            Ok::<(), SlashParseError>(())
        };
        match flag.as_str() {
            "--max-continuations" => {
                take(&mut |v| {
                    budget.max_continuations = parse_budget_number(v)?.map(|n| n as u32);
                    Ok(())
                })?;
                named_budget = true;
            }
            "--max-turns" => {
                take(&mut |v| {
                    budget.max_turns = parse_budget_number(v)?.map(|n| n as u32);
                    Ok(())
                })?;
                named_budget = true;
            }
            "--max-tokens" => {
                take(&mut |v| {
                    budget.max_tokens = parse_budget_number(v)?;
                    Ok(())
                })?;
                named_budget = true;
            }
            "--timeout-ms" => {
                take(&mut |v| {
                    budget.timeout_ms = parse_budget_number(v)?;
                    Ok(())
                })?;
                named_budget = true;
            }
            "--gate" => take(&mut |v| {
                overrides.gates.push(v.to_string());
                Ok(())
            })?,
            "--gate-retries" => take(&mut |v| {
                overrides.gate_retries =
                    Some(parse_budget_number(v)?.map_or(u32::MAX, |n| n as u32));
                Ok(())
            })?,
            "--gate-timeout-ms" => take(&mut |v| {
                overrides.gate_timeout_ms = Some(parse_budget_number(v)?.unwrap_or(u64::MAX));
                Ok(())
            })?,
            "--subagent-keep-alive-ms" => take(&mut |v| {
                overrides.subagent_keep_alive_ms = Some(parse_budget_number(v)?.unwrap_or(0));
                Ok(())
            })?,
            _ => return Err(SlashParseError::AutonomousUsage),
        }
        index += 1;
    }
    if named_budget {
        overrides.limits = Some(budget);
    }
    Ok(overrides)
}

fn parse_autonomous(args: &str) -> Result<AutonomousAction, SlashParseError> {
    let words = shell_words(args.trim());
    match words.first().map(String::as_str) {
        None | Some("status") => Ok(AutonomousAction::Status),
        Some("off") => Ok(AutonomousAction::Off),
        Some("on") => Ok(AutonomousAction::On(parse_autonomous_options(&words[1..])?)),
        _ => Err(SlashParseError::AutonomousUsage),
    }
}

pub fn shell_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut has_word = false;
    for ch in text.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => {
                quote = Some(ch);
                has_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if has_word {
                    words.push(std::mem::take(&mut current));
                    has_word = false;
                }
            }
            (None, c) => {
                current.push(c);
                has_word = true;
            }
        }
    }
    if has_word {
        words.push(current);
    }
    words
}

fn parse_heartbeat(args: &str) -> Result<HeartbeatAction, SlashParseError> {
    let mut rest = args.trim();
    match rest {
        "" | "status" => return Ok(HeartbeatAction::Status),
        "pause" => return Ok(HeartbeatAction::Pause),
        "resume" => return Ok(HeartbeatAction::Resume),
        "clear" | "stop" => return Ok(HeartbeatAction::Clear),
        _ => {}
    }
    let mut expression = "every 5m".to_string();
    let mut follow_up = false;
    loop {
        if let Some(after) = rest.strip_prefix("--every") {
            let after = after.trim_start();
            let (amount, tail) = after
                .split_once(char::is_whitespace)
                .ok_or(SlashParseError::HeartbeatUsage)?;
            expression = format!("every {amount}");
            rest = tail.trim();
        } else if let Some(after) = rest.strip_prefix("every ") {
            let after = after.trim_start();
            let (amount, tail) = after
                .split_once(char::is_whitespace)
                .ok_or(SlashParseError::HeartbeatUsage)?;
            expression = format!("every {amount}");
            rest = tail.trim();
        } else if let Some(after) = rest.strip_prefix("--follow-up") {
            follow_up = true;
            rest = after.trim();
        } else if let Some(after) = rest.strip_prefix("--steer") {
            follow_up = false;
            rest = after.trim();
        } else {
            break;
        }
    }
    if rest.is_empty() {
        return Err(SlashParseError::HeartbeatUsage);
    }
    Ok(HeartbeatAction::Set {
        expression,
        prompt: rest.to_string(),
        follow_up,
    })
}

fn parse_schedule(args: &str) -> Result<ScheduleAction, SlashParseError> {
    let args = args.trim();
    let (verb, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let rest = rest.trim();
    match verb {
        "" | "list" | "ls" => Ok(ScheduleAction::List),
        "pause" if !rest.is_empty() => Ok(ScheduleAction::Pause(rest.to_string())),
        "resume" if !rest.is_empty() => Ok(ScheduleAction::Resume(rest.to_string())),
        "cancel" | "rm" if !rest.is_empty() => Ok(ScheduleAction::Cancel(rest.to_string())),
        "add" => {
            let quoted = rest
                .strip_prefix('"')
                .and_then(|after| after.split_once('"'))
                .map(|(expression, tail)| (expression.to_string(), tail.trim()));
            let (expression, prompt) = match quoted {
                Some(parts) => parts,
                None => {
                    let mut words = rest.splitn(3, char::is_whitespace);
                    let first = words.next().ok_or(SlashParseError::ScheduleUsage)?;
                    let second = words.next().ok_or(SlashParseError::ScheduleUsage)?;
                    (
                        format!("{first} {second}"),
                        words.next().unwrap_or("").trim(),
                    )
                }
            };
            let (label, prompt) = match prompt.strip_prefix("--label") {
                Some(after) => {
                    let after = after.trim_start();
                    let (label, tail) = after
                        .split_once(char::is_whitespace)
                        .ok_or(SlashParseError::ScheduleUsage)?;
                    (Some(label.to_string()), tail.trim())
                }
                None => (None, prompt),
            };
            if prompt.is_empty() {
                return Err(SlashParseError::ScheduleUsage);
            }
            Ok(ScheduleAction::Add {
                expression,
                prompt: prompt.to_string(),
                label,
            })
        }
        _ => Err(SlashParseError::ScheduleUsage),
    }
}

fn strip_flag<'a>(args: &'a str, flag: &str) -> Option<&'a str> {
    args.strip_prefix(flag)
        .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

fn new_usage(reason: impl Into<String>) -> SlashParseError {
    SlashParseError::NewUsage(reason.into())
}

fn new_prompt(rest: &str) -> Result<Option<String>, SlashParseError> {
    let prompt = rest.trim();
    if prompt.is_empty() {
        return Err(new_usage("missing prompt after --"));
    }
    Ok(Some(prompt.to_string()))
}

fn parse_new(args: &str) -> Result<SlashAction, SlashParseError> {
    if args.is_empty() {
        return Ok(SlashAction::New {
            name: None,
            prompt: None,
        });
    }
    if !args.starts_with('-') {
        return Ok(SlashAction::New {
            name: None,
            prompt: Some(args.to_string()),
        });
    }
    if let Some(rest) = strip_flag(args, "--") {
        return Ok(SlashAction::New {
            name: None,
            prompt: new_prompt(rest)?,
        });
    }
    let first_word = |text: &str| text.split_whitespace().next().unwrap_or(text).to_string();
    let Some(rest) = strip_flag(args, "--name") else {
        return Err(new_usage(format!("unknown option {}", first_word(args))));
    };
    let rest = rest.trim_start();
    if strip_flag(rest, "--name").is_some() {
        return Err(new_usage("--name given twice"));
    }
    if rest.is_empty() || rest.starts_with('-') {
        return Err(new_usage("missing value for --name"));
    }
    let (name, rest) = match rest.chars().next() {
        Some(quote @ ('"' | '\'')) => rest[1..]
            .split_once(quote)
            .ok_or_else(|| new_usage("unterminated quote in --name"))?,
        _ => rest.split_once(char::is_whitespace).unwrap_or((rest, "")),
    };
    let name = name.trim();
    if name.is_empty() {
        return Err(new_usage("--name cannot be empty"));
    }
    let name = Some(name.to_string());
    if rest.is_empty() {
        return Ok(SlashAction::New { name, prompt: None });
    }
    if !rest.starts_with(char::is_whitespace) {
        return Err(new_usage("expected -- before the prompt"));
    }
    let rest = rest.trim_start();
    if strip_flag(rest, "--name").is_some() {
        return Err(new_usage("--name given twice"));
    }
    if let Some(prompt) = strip_flag(rest, "--") {
        return Ok(SlashAction::New {
            name,
            prompt: new_prompt(prompt)?,
        });
    }
    if rest.starts_with('-') {
        return Err(new_usage(format!("unknown option {}", first_word(rest))));
    }
    Err(new_usage("expected -- before the prompt"))
}

fn parse_goal(args: &str) -> Result<GoalAction, SlashParseError> {
    let args = args.trim();
    match args {
        "" | "status" => return Ok(GoalAction::Status),
        "pause" => return Ok(GoalAction::Pause),
        "resume" => return Ok(GoalAction::Resume),
        "clear" => return Ok(GoalAction::Clear),
        "complete" | "done" => return Ok(GoalAction::Complete),
        "fail" => return Err(SlashParseError::GoalUsage),
        _ => {}
    }
    if let Some(error) = args.strip_prefix("fail ") {
        let error = error.trim();
        if error.is_empty() {
            return Err(SlashParseError::GoalUsage);
        }
        return Ok(GoalAction::Fail {
            error: error.to_string(),
        });
    }
    let mut token_budget = None;
    let mut rest = args;
    if let Some(after) = rest.strip_prefix("--budget") {
        let after = after.trim_start();
        let (number, tail) = after.split_once(char::is_whitespace).unwrap_or((after, ""));
        let parsed: u64 = number
            .replace(['_', ','], "")
            .parse()
            .map_err(|_| SlashParseError::GoalUsage)?;
        token_budget = Some(parsed);
        rest = tail.trim();
    }
    if rest.is_empty() {
        return Err(SlashParseError::GoalUsage);
    }
    Ok(GoalAction::Set {
        objective: rest.to_string(),
        token_budget,
    })
}

pub fn parse(input: &str) -> Result<SlashAction, SlashParseError> {
    let Some(command) = input.strip_prefix('/') else {
        return Err(SlashParseError::MissingPrefix);
    };

    let (name, args) = split_name_and_args(command);
    match name {
        "subagents" => Ok(SlashAction::Subagents {
            args: args.to_string(),
        }),
        "agents" => Ok(SlashAction::Agents),
        "system-prompt" | "system" => Ok(SlashAction::SystemPrompt),
        "help" => Ok(SlashAction::Help),
        "keys" => Ok(SlashAction::Keys),
        "new" => parse_new(args),
        "model" => {
            if args == "--all" {
                Ok(SlashAction::Model {
                    model_id: None,
                    show_all: true,
                })
            } else {
                Ok(SlashAction::Model {
                    model_id: (!args.is_empty()).then(|| args.to_string()),
                    show_all: false,
                })
            }
        }
        "sessions" => Ok(SlashAction::Sessions),
        "resume" if args.is_empty() => Ok(SlashAction::Sessions),
        "resume" => Ok(SlashAction::Resume {
            session_id: SessionId::from(args),
        }),
        "branch" if args.is_empty() => Ok(SlashAction::Branch { index: None }),
        "branch" => args
            .parse::<usize>()
            .map(|index| SlashAction::Branch { index: Some(index) })
            .map_err(|_| SlashParseError::BranchUsage),
        "fork" if args.is_empty() => Ok(SlashAction::Fork { index: None }),
        "fork" => args
            .parse::<usize>()
            .map(|index| SlashAction::Fork { index: Some(index) })
            .map_err(|_| SlashParseError::ForkUsage),
        "clone" => Ok(SlashAction::Clone),
        "tree" => Ok(SlashAction::Tree),
        "compact" => parse_compact_focus(args).map(|focus| SlashAction::Compact { focus }),
        "refine" => parse_refine(args).map(SlashAction::Refine),
        "subagent-model" | "sub-model" => {
            let unset = matches!(args, "unset" | "none" | "default" | "inherit");
            Ok(SlashAction::SubagentModel {
                model_id: (!args.is_empty() && !unset).then(|| args.to_string()),
                unset,
            })
        }
        "pause" if matches!(args, "off" | "clear") => Ok(SlashAction::Resumed),
        "pause" => Ok(SlashAction::Pause {
            reason: (!args.is_empty()).then(|| args.to_string()),
        }),
        "unpause" => Ok(SlashAction::Resumed),
        "compaction-model" | "compact-model" => Ok(SlashAction::CompactionModel {
            model_id: (!args.is_empty()).then(|| args.to_string()),
        }),
        "export" => Ok(SlashAction::Export {
            path: (!args.is_empty()).then(|| args.to_string()),
        }),
        "undo" => Ok(SlashAction::Undo),
        "search" => Ok(SlashAction::Search {
            query: args.to_string(),
        }),
        "cost" => Ok(SlashAction::Cost),
        "context" | "usage" => Ok(SlashAction::Context),
        "skills" => Ok(SlashAction::Skills),
        "session" => Ok(SlashAction::SessionInfo),
        "rlm-max-depth" if args.is_empty() => Ok(SlashAction::RlmMaxDepth { max_depth: None }),
        "rlm-max-depth" => args
            .parse()
            .map(|max_depth| SlashAction::RlmMaxDepth {
                max_depth: Some(max_depth),
            })
            .map_err(|_| SlashParseError::RlmMaxDepthUsage),
        "goal" => parse_goal(args).map(SlashAction::Goal),
        "heartbeat" => parse_heartbeat(args).map(SlashAction::Heartbeat),
        "autonomous" => parse_autonomous(args).map(SlashAction::Autonomous),
        "schedule" => parse_schedule(args).map(SlashAction::Schedule),
        "btw" | "side" if args.is_empty() => Err(SlashParseError::SideUsage),
        "btw" | "side" => Ok(SlashAction::Side {
            question: args.to_string(),
        }),
        "name" | "rename" => Ok(SlashAction::Name {
            title: (!args.is_empty()).then(|| args.to_string()),
        }),
        "credits" => Ok(SlashAction::Credits),
        "endpoints" => Ok(SlashAction::Endpoints {
            model_id: (!args.is_empty()).then(|| args.to_string()),
        }),
        "card" => Ok(SlashAction::Card),
        "mcp" => parse_mcp(args).map(SlashAction::Mcp),
        "logs" if args.is_empty() => Ok(SlashAction::Logs { count: 50 }),
        "logs" => args
            .parse::<usize>()
            .map(|count| SlashAction::Logs { count })
            .map_err(|_| SlashParseError::LogsUsage),
        "settings" => Ok(SlashAction::Settings {
            args: args.to_string(),
        }),
        "login" => Ok(SlashAction::Login {
            provider: (!args.is_empty()).then(|| args.to_string()),
        }),
        "login-complete" | "login-code" if args.is_empty() => Err(SlashParseError::LoginCodeUsage),
        "login-complete" | "login-code" => Ok(SlashAction::LoginComplete {
            code: args.to_string(),
        }),
        "debug" => Ok(SlashAction::Debug),
        "reload" => Ok(SlashAction::Reload),
        "refresh-models" | "models-refresh" => Ok(SlashAction::RefreshModels),
        "thinking-expand" => Ok(SlashAction::ToggleThinkingExpanded),
        "quit" | "exit" | "q" => Ok(SlashAction::Quit),
        other => Ok(SlashAction::Other {
            name: other.to_string(),
            args: args.to_string(),
        }),
    }
}

const MAX_SUGGESTED_NAME_LEN: usize = 64;

pub(crate) fn suggest_command<'a>(
    name: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let len = name.chars().count();
    if len > MAX_SUGGESTED_NAME_LEN {
        return None;
    }
    let threshold = if len <= 3 { 1 } else { (len / 3).max(2) };
    let mut closest: Option<(&str, usize)> = None;
    for candidate in candidates {
        let distance = edit_distance(name, candidate);
        if closest.is_none_or(|(_, best)| distance < best) {
            closest = Some((candidate, distance));
        }
    }
    closest
        .filter(|&(_, distance)| distance <= threshold)
        .map(|(candidate, _)| candidate)
}

fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for (i, l) in left.chars().enumerate() {
        current[0] = i + 1;
        for (j, r) in right.iter().enumerate() {
            let substitution = previous[j] + usize::from(l != *r);
            current[j + 1] = (previous[j + 1] + 1).min(current[j] + 1).min(substitution);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

pub(crate) fn split_name_and_args(command: &str) -> (&str, &str) {
    let trimmed = command.trim();
    match trimmed.split_once(char::is_whitespace) {
        Some((name, rest)) => (name, rest.trim()),
        None => (trimmed, ""),
    }
}

fn parse_refine(args: &str) -> Result<mush_session::refine::RefineRequest, SlashParseError> {
    let mut rest = args.trim();
    let mut global = false;
    if let Some(after) = rest.strip_prefix("--global")
        && (after.is_empty() || after.starts_with(char::is_whitespace))
    {
        global = true;
        rest = after.trim();
    }
    if let Some(after) = rest.strip_prefix("rollback")
        && (after.is_empty() || after.starts_with(char::is_whitespace))
    {
        let mut id = after.trim();
        if let Some(before) = id.strip_suffix("--global") {
            global = true;
            id = before.trim();
        }
        if id.is_empty() || id.contains(char::is_whitespace) {
            return Err(SlashParseError::RefineUsage);
        }
        return Ok(mush_session::refine::RefineRequest {
            instructions: None,
            rollback: Some(id.to_string()),
            global,
            source: mush_session::harness::RefinementSource::User,
        });
    }
    if rest.starts_with("--") {
        return Err(SlashParseError::RefineUsage);
    }
    Ok(mush_session::refine::RefineRequest {
        instructions: (!rest.is_empty()).then(|| rest.to_string()),
        rollback: None,
        global,
        source: mush_session::harness::RefinementSource::User,
    })
}

fn parse_compact_focus(
    args: &str,
) -> Result<mush_session::compact::CompactionFocus, SlashParseError> {
    let mut rest = args.trim();
    let mut context_budget = None;
    while let Some(flag_rest) = rest
        .strip_prefix("--context-budget")
        .or_else(|| rest.strip_prefix("-b"))
    {
        if !flag_rest.is_empty() && !flag_rest.starts_with(char::is_whitespace) {
            break;
        }
        let mut tokens = flag_rest.trim_start().splitn(2, char::is_whitespace);
        let value = tokens
            .next()
            .filter(|value| !value.is_empty())
            .ok_or(SlashParseError::CompactUsage)?;
        context_budget = Some(value.parse::<TokenCount>()?);
        rest = tokens.next().unwrap_or("").trim();
    }
    if rest.starts_with("--") {
        return Err(SlashParseError::CompactUsage);
    }
    Ok(mush_session::compact::CompactionFocus {
        context_budget,
        instructions: (!rest.is_empty()).then(|| rest.to_string()),
    })
}

fn parse_mcp(args: &str) -> Result<McpQuery, SlashParseError> {
    let (sub, rest) = split_name_and_args(args);
    match sub {
        "" => Ok(McpQuery::Status),
        "tools" => Ok(McpQuery::Tools {
            server: (!rest.is_empty()).then(|| rest.to_string()),
        }),
        "schema" if rest.is_empty() => Err(SlashParseError::McpUsage),
        "schema" => Ok(McpQuery::Schema {
            tool: rest.to_string(),
        }),
        "reconnect" if rest.is_empty() => Err(SlashParseError::McpUsage),
        "reconnect" => Ok(McpQuery::Reconnect {
            server: rest.to_string(),
        }),
        _ => Err(SlashParseError::McpUsage),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_near_typo_suggests_the_closest_command() {
        let commands = ["resume", "logs", "mcp", "skill:python"];
        assert_eq!(suggest_command("resuem", commands), Some("resume"));
        assert_eq!(suggest_command("log", commands), Some("logs"));
        assert_eq!(
            suggest_command("skill:pythno", commands),
            Some("skill:python")
        );
    }

    #[test]
    fn paths_and_far_words_get_no_suggestion() {
        let commands = ["resume", "logs", "mcp", "help"];
        assert_eq!(suggest_command("tmp", commands), None);
        assert_eq!(suggest_command("usr/bin/foo", commands), None);
        assert_eq!(suggest_command("etc/hosts", commands), None);
        assert_eq!(suggest_command(&"h".repeat(65), commands), None);
    }

    #[test]
    fn the_first_of_equally_close_commands_wins() {
        assert_eq!(suggest_command("mdoel", ["model", "models"]), Some("model"));
        assert_eq!(suggest_command("goa", ["go", "goal"]), Some("go"));
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
    }

    #[test]
    fn parse_rlm_max_depth_shows_or_sets() {
        assert_eq!(
            parse("/rlm-max-depth").unwrap(),
            SlashAction::RlmMaxDepth { max_depth: None }
        );
        assert_eq!(
            parse("/rlm-max-depth 3").unwrap(),
            SlashAction::RlmMaxDepth { max_depth: Some(3) }
        );
        assert!(matches!(
            parse("/rlm-max-depth -1"),
            Err(SlashParseError::RlmMaxDepthUsage)
        ));
    }

    #[test]
    fn parse_mcp_bare_is_status() {
        assert_eq!(parse("/mcp").unwrap(), SlashAction::Mcp(McpQuery::Status));
    }

    #[test]
    fn parse_mcp_tools_with_and_without_a_server() {
        assert_eq!(
            parse("/mcp tools").unwrap(),
            SlashAction::Mcp(McpQuery::Tools { server: None })
        );
        assert_eq!(
            parse("/mcp tools sweethome3d").unwrap(),
            SlashAction::Mcp(McpQuery::Tools {
                server: Some("sweethome3d".into())
            })
        );
    }

    #[test]
    fn parse_mcp_schema_needs_a_tool_name() {
        assert_eq!(
            parse("/mcp schema sweethome3d_render_photo").unwrap(),
            SlashAction::Mcp(McpQuery::Schema {
                tool: "sweethome3d_render_photo".into()
            })
        );
        assert_eq!(parse("/mcp schema"), Err(SlashParseError::McpUsage));
    }

    #[test]
    fn parse_mcp_reconnect_needs_a_server() {
        assert_eq!(
            parse("/mcp reconnect sweethome3d").unwrap(),
            SlashAction::Mcp(McpQuery::Reconnect {
                server: "sweethome3d".into()
            })
        );
        assert_eq!(parse("/mcp reconnect"), Err(SlashParseError::McpUsage));
    }

    #[test]
    fn parse_mcp_rejects_an_unknown_subcommand() {
        assert_eq!(parse("/mcp wibble"), Err(SlashParseError::McpUsage));
    }

    #[test]
    fn mcp_status_is_safe_during_stream() {
        assert!(SlashAction::Mcp(McpQuery::Status).is_safe_during_stream());
    }

    #[test]
    fn parse_thinking_expand_action() {
        assert_eq!(
            parse("/thinking-expand").unwrap(),
            SlashAction::ToggleThinkingExpanded
        );
    }

    fn compact(context_budget: Option<u64>, instructions: Option<&str>) -> SlashAction {
        SlashAction::Compact {
            focus: mush_session::compact::CompactionFocus {
                context_budget: context_budget.map(TokenCount::new),
                instructions: instructions.map(str::to_string),
            },
        }
    }

    #[test]
    fn context_and_usage_are_the_same_command() {
        assert_eq!(parse("/context").unwrap(), SlashAction::Context);
        assert_eq!(parse("/usage").unwrap(), SlashAction::Context);
    }

    #[test]
    fn refine_parses_instructions_scope_and_rollback() {
        use mush_session::refine::RefineRequest;
        assert_eq!(
            parse("/refine").unwrap(),
            SlashAction::Refine(RefineRequest::default())
        );
        assert_eq!(
            parse("/refine --global remember the port").unwrap(),
            SlashAction::Refine(RefineRequest {
                instructions: Some("remember the port".into()),
                rollback: None,
                global: true,
                ..RefineRequest::default()
            })
        );
        assert_eq!(
            parse("/refine rollback ref_abc --global").unwrap(),
            SlashAction::Refine(RefineRequest {
                instructions: None,
                rollback: Some("ref_abc".into()),
                global: true,
                ..RefineRequest::default()
            })
        );
        assert_eq!(
            parse("/refine rollback").unwrap_err(),
            SlashParseError::RefineUsage
        );
        assert_eq!(
            parse("/refine --bogus").unwrap_err(),
            SlashParseError::RefineUsage
        );
    }

    #[test]
    fn parse_compact_bare() {
        assert_eq!(parse("/compact").unwrap(), compact(None, None));
    }

    #[test]
    fn parse_compact_with_context_budget() {
        assert_eq!(
            parse("/compact --context-budget 10k").unwrap(),
            compact(Some(10_000), None)
        );
        assert_eq!(
            parse("/compact --context-budget 1024").unwrap(),
            compact(Some(1024), None)
        );
    }

    #[test]
    fn parse_compact_with_short_budget_flag() {
        assert_eq!(parse("/compact -b 5k").unwrap(), compact(Some(5_000), None));
    }

    #[test]
    fn parse_compact_with_instructions() {
        assert_eq!(
            parse("/compact focus on the auth refactor, keep the migration command").unwrap(),
            compact(
                None,
                Some("focus on the auth refactor, keep the migration command")
            )
        );
    }

    #[test]
    fn parse_compact_with_budget_and_instructions() {
        assert_eq!(
            parse("/compact -b 5k keep the failing tests").unwrap(),
            compact(Some(5_000), Some("keep the failing tests"))
        );
    }

    #[test]
    fn parse_compact_instructions_may_start_with_a_dash_word() {
        assert_eq!(
            parse("/compact -bare metal notes").unwrap(),
            compact(None, Some("-bare metal notes"))
        );
    }

    #[test]
    fn parse_fork_compact_is_no_longer_a_known_command() {
        assert!(matches!(
            parse("/fork-compact").unwrap(),
            SlashAction::Other { .. }
        ));
        assert!(matches!(parse("/fc").unwrap(), SlashAction::Other { .. }));
    }

    #[test]
    fn parse_compact_rejects_missing_budget_value() {
        assert!(parse("/compact --context-budget").is_err());
        assert!(parse("/compact -b").is_err());
    }

    #[test]
    fn parse_compact_rejects_bad_budget_value() {
        assert!(parse("/compact --context-budget garbage").is_err());
        assert!(parse("/compact -b 10g").is_err());
    }

    #[test]
    fn parse_compact_rejects_unknown_flag() {
        assert!(parse("/compact --frobnicate 10k").is_err());
    }

    #[test]
    fn parse_subagent_model() {
        assert_eq!(
            parse("/subagent-model").unwrap(),
            SlashAction::SubagentModel {
                model_id: None,
                unset: false
            }
        );
        assert_eq!(
            parse("/subagent-model claude-haiku-4-5").unwrap(),
            SlashAction::SubagentModel {
                model_id: Some("claude-haiku-4-5".into()),
                unset: false
            }
        );
        for word in ["unset", "none", "default", "inherit"] {
            assert_eq!(
                parse(&format!("/subagent-model {word}")).unwrap(),
                SlashAction::SubagentModel {
                    model_id: None,
                    unset: true
                }
            );
        }
        assert_eq!(
            parse("/sub-model unset").unwrap(),
            SlashAction::SubagentModel {
                model_id: None,
                unset: true
            }
        );
    }

    #[test]
    fn parse_compaction_model() {
        assert_eq!(
            parse("/compaction-model").unwrap(),
            SlashAction::CompactionModel { model_id: None }
        );
        assert_eq!(
            parse("/compact-model claude-sonnet-4-20250514").unwrap(),
            SlashAction::CompactionModel {
                model_id: Some("claude-sonnet-4-20250514".into())
            }
        );
    }

    #[test]
    fn parse_pause_takes_a_reason_and_off_unpauses() {
        assert_eq!(
            parse("/pause").unwrap(),
            SlashAction::Pause { reason: None }
        );
        assert_eq!(
            parse("/pause rebuilding mush").unwrap(),
            SlashAction::Pause {
                reason: Some("rebuilding mush".into()),
            }
        );
        assert_eq!(parse("/pause off").unwrap(), SlashAction::Resumed);
        assert_eq!(parse("/unpause").unwrap(), SlashAction::Resumed);
        assert_eq!(parse("/resume").unwrap(), SlashAction::Sessions);
    }

    #[test]
    fn parse_model_action() {
        assert_eq!(
            parse("/model claude-sonnet").unwrap(),
            SlashAction::Model {
                model_id: Some("claude-sonnet".into()),
                show_all: false,
            }
        );
    }

    #[test]
    fn parse_model_all_flag_opens_picker_with_hidden_visible() {
        assert_eq!(
            parse("/model --all").unwrap(),
            SlashAction::Model {
                model_id: None,
                show_all: true,
            }
        );
    }

    #[test]
    fn parse_other_command_preserves_args() {
        assert_eq!(
            parse("/review src/main.rs").unwrap(),
            SlashAction::Other {
                name: "review".into(),
                args: "src/main.rs".into(),
            }
        );
    }

    #[test]
    fn parse_new_command() {
        assert_eq!(
            parse("/new").unwrap(),
            SlashAction::New {
                name: None,
                prompt: None,
            }
        );
    }

    fn new_action(name: Option<&str>, prompt: Option<&str>) -> SlashAction {
        SlashAction::New {
            name: name.map(Into::into),
            prompt: prompt.map(Into::into),
        }
    }

    #[test]
    fn parse_new_takes_a_name_and_a_prompt() {
        assert_eq!(
            parse("/new fix the bug").unwrap(),
            new_action(None, Some("fix the bug"))
        );
        assert_eq!(
            parse("/new -- --flag looking text").unwrap(),
            new_action(None, Some("--flag looking text"))
        );
        assert_eq!(
            parse("/new --name ui").unwrap(),
            new_action(Some("ui"), None)
        );
        assert_eq!(
            parse("/new --name \"port prime\" -- fix it").unwrap(),
            new_action(Some("port prime"), Some("fix it"))
        );
        assert_eq!(
            parse("/new --name 'a b'").unwrap(),
            new_action(Some("a b"), None)
        );
    }

    #[test]
    fn parse_new_refuses_bad_flags() {
        for input in [
            "/new --bogus",
            "/new --name",
            "/new --name --name x",
            "/new --name ui fix it",
            "/new --name ui -- ",
            "/new --name \"unterminated",
            "/new --name ui --other",
            "/new --",
            "/new --name \"\"",
        ] {
            assert!(
                matches!(parse(input), Err(SlashParseError::NewUsage(_))),
                "{input}"
            );
        }
    }

    #[test]
    fn parse_clear_is_removed() {
        assert_eq!(
            parse("/clear").unwrap(),
            SlashAction::Other {
                name: "clear".into(),
                args: String::new(),
            }
        );
    }

    #[test]
    fn parse_debug_produces_debug_action() {
        assert_eq!(parse("/debug").unwrap(), SlashAction::Debug);
    }

    #[test]
    fn parse_resume_no_arg_opens_the_picker() {
        assert_eq!(parse("/resume").unwrap(), SlashAction::Sessions);
    }

    #[test]
    fn parse_resume_with_id_keeps_explicit_target() {
        assert_eq!(
            parse("/resume abc123").unwrap(),
            SlashAction::Resume {
                session_id: SessionId::from("abc123"),
            }
        );
    }

    #[test]
    fn heartbeat_and_schedule_commands_parse() {
        assert_eq!(
            parse("/heartbeat every 10m check the build").unwrap(),
            SlashAction::Heartbeat(HeartbeatAction::Set {
                expression: "every 10m".into(),
                prompt: "check the build".into(),
                follow_up: false,
            })
        );
        assert_eq!(
            parse("/heartbeat --follow-up look around").unwrap(),
            SlashAction::Heartbeat(HeartbeatAction::Set {
                expression: "every 5m".into(),
                prompt: "look around".into(),
                follow_up: true,
            })
        );
        assert_eq!(
            parse("/heartbeat pause").unwrap(),
            SlashAction::Heartbeat(HeartbeatAction::Pause)
        );
        assert!(parse("/heartbeat every 10m").is_err());
        assert_eq!(
            parse("/schedule add \"in 30m\" --label tests run the tests").unwrap(),
            SlashAction::Schedule(ScheduleAction::Add {
                expression: "in 30m".into(),
                prompt: "run the tests".into(),
                label: Some("tests".into()),
            })
        );
        assert_eq!(
            parse("/schedule add every 10m poke the log").unwrap(),
            SlashAction::Schedule(ScheduleAction::Add {
                expression: "every 10m".into(),
                prompt: "poke the log".into(),
                label: None,
            })
        );
        assert_eq!(
            parse("/schedule cancel ab12").unwrap(),
            SlashAction::Schedule(ScheduleAction::Cancel("ab12".into()))
        );
        assert_eq!(
            parse("/schedule").unwrap(),
            SlashAction::Schedule(ScheduleAction::List)
        );
        assert_eq!(
            parse("/goal --budget 50_000 finish the daemon").unwrap(),
            SlashAction::Goal(GoalAction::Set {
                objective: "finish the daemon".into(),
                token_budget: Some(50_000),
            })
        );
        assert_eq!(
            parse("/goal").unwrap(),
            SlashAction::Goal(GoalAction::Status)
        );
    }

    #[test]
    fn autonomous_options_define_the_whole_budget_when_named() {
        let defaults = mush_runtime::AutonomousDefaults::default();
        let (limits, gates, keep_alive) = parse_autonomous_options(&[]).unwrap().resolve(&defaults);
        assert_eq!(keep_alive, defaults.subagent_keep_alive_ms);
        assert_eq!(limits, mush_runtime::Limits::defaults());
        assert!(gates.commands.is_empty());

        let words: Vec<String> = shell_words(
            "--max-tokens 100,000 --gate \"cargo test\" --gate 'cargo clippy' --gate-retries 2 --max-turns=unlimited",
        );
        let (limits, gates, _) = parse_autonomous_options(&words)
            .unwrap()
            .resolve(&mush_runtime::AutonomousDefaults::default());
        assert_eq!(limits.max_tokens, Some(100_000));
        assert_eq!(limits.max_turns, None);
        assert_eq!(
            limits.max_continuations, None,
            "an unnamed limit becomes unlimited"
        );
        assert_eq!(gates.commands, ["cargo test", "cargo clippy"]);
        assert_eq!(gates.max_retries, 2);

        let cli_spelling: Vec<String> = shell_words("--autonomous-max-continuations 7");
        let (limits, _, _) = parse_autonomous_options(&cli_spelling)
            .unwrap()
            .resolve(&mush_runtime::AutonomousDefaults::default());
        assert_eq!(limits.max_continuations, Some(7));

        assert!(parse_autonomous_options(&shell_words("--max-turns")).is_err());
        assert!(parse_autonomous_options(&shell_words("--bogus 1")).is_err());
        assert_eq!(
            parse("/autonomous").unwrap(),
            SlashAction::Autonomous(AutonomousAction::Status)
        );
        assert_eq!(
            parse("/autonomous off").unwrap(),
            SlashAction::Autonomous(AutonomousAction::Off)
        );
        assert!(matches!(
            parse("/autonomous on --gate \"just check\"").unwrap(),
            SlashAction::Autonomous(AutonomousAction::On(overrides)) if overrides.gates == ["just check"]
        ));
    }

    #[test]
    fn parse_reload_command() {
        assert_eq!(parse("/reload").unwrap(), SlashAction::Reload);
    }

    #[test]
    fn parse_refresh_models_command() {
        assert_eq!(
            parse("/refresh-models").unwrap(),
            SlashAction::RefreshModels
        );
        assert_eq!(
            parse("/models-refresh").unwrap(),
            SlashAction::RefreshModels
        );
    }
}
