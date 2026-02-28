//! TUI runner - wires terminal, agent loop, and event handling together

use std::io;

use crossterm::ExecutableCommand;
use crossterm::event::{self, Event};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use mush_agent::tool::AgentTool;
use mush_agent::{AgentConfig, AgentEvent, agent_loop};
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

    let mut app = App::new(tui_config.model.id.clone());
    let mut pending_prompt: Option<String> = None;
    let mut conversation: Vec<Message> = Vec::new();

    // draw initial frame
    draw(&mut terminal, &app)?;

    loop {
        // if there's a pending prompt and we're not streaming, start the agent
        if let Some(prompt) = pending_prompt.take() {
            conversation.push(Message::User(UserMessage {
                content: UserContent::Text(prompt),
                timestamp_ms: timestamp_ms(),
            }));

            let config = AgentConfig {
                model: &tui_config.model,
                system_prompt: tui_config.system_prompt.clone(),
                tools,
                registry,
                options: tui_config.options.clone(),
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
                    app.push_user_message(text.clone());
                    app.start_streaming();
                    pending_prompt = Some(text);
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
            let summary = summarise_tool_args(tool_name, args);
            app.start_tool(tool_name, &summary);
        }
        AgentEvent::ToolExecEnd {
            tool_name, result, ..
        } => {
            app.end_tool(tool_name, result.is_error);
        }
        AgentEvent::TurnStart { .. } if !app.is_streaming => {
            app.start_streaming();
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

/// summarise tool args for display (shared with print mode)
pub fn summarise_tool_args(tool_name: &str, args: &serde_json::Value) -> String {
    match tool_name {
        "read" | "write" | "edit" => args["path"].as_str().unwrap_or("").to_string(),
        "bash" => {
            let cmd = args["command"].as_str().unwrap_or("");
            if cmd.len() > 60 {
                format!("{}...", &cmd[..57])
            } else {
                cmd.to_string()
            }
        }
        "grep" => {
            let pattern = args["pattern"].as_str().unwrap_or("");
            let path = args["path"].as_str().unwrap_or(".");
            format!("{pattern} in {path}")
        }
        "find" => args["pattern"].as_str().unwrap_or("").to_string(),
        "ls" => args["path"].as_str().unwrap_or(".").to_string(),
        _ => format!("{args}"),
    }
}

fn timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
