//! TUI runner - wires terminal, agent loop, and event handling together

use std::io;
use std::sync::Arc;

use crossterm::ExecutableCommand;
use crossterm::event::{self, Event};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::Mutex;

use mush_agent::tool::AgentTool;
use mush_agent::{AgentConfig, AgentEvent, agent_loop, summarise_tool_args};
use mush_ai::models;
use mush_ai::registry::ApiRegistry;
use mush_ai::stream::StreamEvent;
use mush_ai::types::*;

use crate::app::{App, AppEvent};
use crate::input::handle_key;
use crate::ui::Ui;

/// configuration for the TUI runner (owned, 'static-friendly)
pub struct TuiConfig {
    pub model: Model,
    pub system_prompt: Option<String>,
    pub options: StreamOptions,
    pub max_turns: usize,
    /// initial conversation history (for session resume)
    pub initial_messages: Vec<Message>,
}

/// run the interactive TUI
pub async fn run_tui(
    tui_config: TuiConfig,
    tools: &[Box<dyn AgentTool>],
    registry: &ApiRegistry,
) -> io::Result<()> {
    // set up terminal
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(tui_config.model.id.0.clone());
    let mut pending_prompt: Option<String> = None;
    let mut conversation: Vec<Message> = Vec::new();

    // load initial messages (session resume)
    if !tui_config.initial_messages.is_empty() {
        for msg in &tui_config.initial_messages {
            match msg {
                Message::User(u) => {
                    let text = match &u.content {
                        UserContent::Text(t) => t.clone(),
                        UserContent::Parts(p) => p
                            .iter()
                            .filter_map(|part| match part {
                                UserContentPart::Text(t) => Some(t.text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" "),
                    };
                    app.push_user_message(text);
                }
                Message::Assistant(a) => {
                    let text: String = a
                        .content
                        .iter()
                        .filter_map(|p| match p {
                            AssistantContentPart::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    let thinking: Option<String> = a.content.iter().find_map(|p| match p {
                        AssistantContentPart::Thinking(t) => Some(t.thinking.clone()),
                        _ => None,
                    });
                    app.messages.push(crate::app::DisplayMessage {
                        role: crate::app::MessageRole::Assistant,
                        content: text,
                        tool_calls: vec![],
                        thinking,
                        thinking_expanded: false,
                        usage: Some(a.usage),
                        cost: None,
                    });
                }
                _ => {} // tool results displayed inline with their tool calls
            }
        }
        conversation = tui_config.initial_messages.clone();
        app.status = Some(format!("resumed session ({} messages)", conversation.len()));
    }

    // shared steering queue: user can type while agent is running
    let steering_queue: Arc<Mutex<Vec<Message>>> = Arc::new(Mutex::new(Vec::new()));

    // draw initial frame
    draw(&mut terminal, &app)?;

    loop {
        // if there's a pending prompt and we're not streaming, start the agent
        if let Some(prompt) = pending_prompt.take() {
            conversation.push(Message::User(UserMessage {
                content: UserContent::Text(prompt),
                timestamp_ms: Timestamp::now(),
            }));

            let context_window = tui_config.model.context_window as usize;
            let transform: Option<mush_agent::ContextTransform<'_>> = Some(Box::new(move |msgs| {
                Box::pin(async move { auto_compact(msgs, context_window) })
            }));

            // steering callback: drains any messages queued by user input
            let sq = steering_queue.clone();
            let steering: Option<mush_agent::MessageCallback<'_>> = Some(Box::new(move || {
                let sq = sq.clone();
                Box::pin(async move {
                    let mut q = sq.lock().await;
                    q.drain(..).collect()
                })
            }));

            let config = AgentConfig {
                model: &tui_config.model,
                system_prompt: tui_config.system_prompt.clone(),
                tools,
                registry,
                options: tui_config.options.clone(),
                max_turns: tui_config.max_turns,
                get_steering: steering,
                get_follow_up: None,
                transform_context: transform,
            };

            let mut stream = std::pin::pin!(agent_loop(config, conversation.clone()));

            // inner loop: process agent events while also handling terminal input
            loop {
                let tick = tokio::time::sleep(std::time::Duration::from_millis(16));
                tokio::pin!(tick);

                tokio::select! {
                    agent_event = stream.next() => {
                        match agent_event {
                            Some(event) => {
                                handle_agent_event(&mut app, &mut conversation, &event, &tui_config.model);
                                if matches!(event, AgentEvent::AgentEnd) {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    _ = tick => {
                        // check for terminal input during streaming
                        if event::poll(std::time::Duration::ZERO)?
                            && let Event::Key(key) = event::read()?
                            && let Some(app_event) = handle_key(&mut app, key)
                        {
                            match app_event {
                                AppEvent::Quit => {
                                    cleanup(&mut terminal)?;
                                    return Ok(());
                                }
                                AppEvent::Abort => {
                                    app.is_streaming = false;
                                    app.active_tool = None;
                                    app.status = Some("aborted".into());
                                    break;
                                }
                                AppEvent::UserSubmit { text } => {
                                    // queue as steering message
                                    let msg = Message::User(UserMessage {
                                        content: UserContent::Text(text.clone()),
                                        timestamp_ms: Timestamp::now(),
                                    });
                                    steering_queue.lock().await.push(msg);
                                    app.push_user_message(text);
                                    app.status = Some("steering message queued".into());
                                }
                                _ => {}
                            }
                        }
                    }
                }

                draw(&mut terminal, &app)?;
            }

            draw(&mut terminal, &app)?;
            continue;
        }

        // idle: wait for terminal input
        if event::poll(std::time::Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
            && let Some(app_event) = handle_key(&mut app, key)
        {
            match app_event {
                AppEvent::Quit => break,
                AppEvent::UserSubmit { text } => {
                    let expanded = expand_template(&text);
                    app.push_user_message(expanded.clone());
                    app.start_streaming();
                    pending_prompt = Some(expanded);
                }
                _ => {}
            }
        }

        draw(&mut terminal, &app)?;
    }

    cleanup(&mut terminal)?;
    Ok(())
}

fn handle_agent_event(
    app: &mut App,
    conversation: &mut Vec<Message>,
    event: &AgentEvent,
    model: &Model,
) {
    match event {
        AgentEvent::StreamEvent { event } => match event {
            StreamEvent::TextDelta { delta, .. } => app.push_text_delta(delta),
            StreamEvent::ThinkingDelta { delta, .. } => app.push_thinking_delta(delta),
            _ => {}
        },
        AgentEvent::MessageEnd { message } => {
            let cost = models::calculate_cost(model, &message.usage);
            app.finish_streaming(Some(message.usage), Some(cost.total()));
            conversation.push(Message::Assistant(message.clone()));
        }
        AgentEvent::ToolExecStart {
            tool_name, args, ..
        } => {
            let summary = summarise_tool_args(tool_name.as_str(), args);
            app.start_tool(tool_name.as_str(), &summary);
        }
        AgentEvent::ToolExecEnd {
            tool_name, result, ..
        } => {
            let output_text = result.content.iter().find_map(|p| match p {
                ToolResultContentPart::Text(t) => Some(t.text.as_str()),
                _ => None,
            });
            app.end_tool(tool_name.as_str(), result.is_error, output_text);
        }
        AgentEvent::TurnStart { .. } if !app.is_streaming => {
            app.start_streaming();
        }
        AgentEvent::SteeringInjected { count } => {
            app.status = Some(format!("steering: {count} messages injected"));
        }
        AgentEvent::FollowUpInjected { count } => {
            app.status = Some(format!("follow-up: {count} messages queued"));
        }
        AgentEvent::ContextTransformed {
            before_count,
            after_count,
        } => {
            app.status = Some(format!(
                "compacted: {before_count} → {after_count} messages"
            ));
        }
        AgentEvent::MaxTurnsReached { max_turns } => {
            app.is_streaming = false;
            app.status = Some(format!("hit max turns limit ({max_turns})"));
        }
        AgentEvent::Error { error } => {
            app.is_streaming = false;
            app.status = Some(format!("error: {error}"));
        }
        AgentEvent::AgentEnd => {
            app.is_streaming = false;
        }
        _ => {}
    }
}

/// expand /template_name args... into template content
fn expand_template(prompt: &str) -> String {
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

/// compact messages if estimated tokens exceed 75% of context window
fn auto_compact(messages: Vec<Message>, context_window: usize) -> Vec<Message> {
    use mush_session::compact;

    let estimated = compact::estimate_tokens(&messages);
    let threshold = context_window * 3 / 4;

    if estimated <= threshold || messages.len() <= 10 {
        return messages;
    }

    let prompt = compact::build_compaction_prompt(&messages[..messages.len() - 10]);
    let summary = format!(
        "## Summary of earlier conversation\n\n\
         The following is a condensed summary of the conversation so far:\n\n\
         {prompt}"
    );

    let result = compact::compact_with_summary(messages, &summary, Some(10));
    result.messages
}

fn draw(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &App) -> io::Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();
        let ui = Ui::new(app);
        let (cx, cy) = ui.cursor_position(area);
        frame.render_widget(ui, area);
        if !app.is_streaming {
            frame.set_cursor_position((cx, cy));
        }
    })?;
    Ok(())
}

fn cleanup(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}
