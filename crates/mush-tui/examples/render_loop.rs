use std::time::{Duration, Instant};

use mush_ai::types::TokenCount;
use mush_tui::app::{App, DisplayMessage, MessageRole};
use mush_tui::ui::Ui;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn build_app(message_count: usize) -> App {
    let role_mode = std::env::var("MUSH_PROFILE_ROLE").unwrap_or_default();
    let plain = std::env::var_os("MUSH_PROFILE_PLAIN").is_some();
    let mut app = App::new("profile-model".into(), TokenCount::new(200_000));
    let user = "summarise the performance tradeoffs in the render path";
    let assistant = if plain {
        "## summary\n\nthe hot path builds a list per frame, with **bold** and \
         *italic* and plain prose that wraps across the content width a few times."
    } else {
        "## summary\n\nthe hot path builds a `Vec<Line>` per frame, with \
         **bold** and `inline code` spans plus a fenced block:\n\n\
         ```rust\nfn render(buf: &mut Buffer) {\n    set_line(buf, line);\n}\n```\n\n\
         that covers wrapping, span allocation and width lookups."
    };
    for i in 0..message_count {
        let role = match role_mode.as_str() {
            "u" => MessageRole::User,
            "a" => MessageRole::Assistant,
            _ if i % 2 == 0 => MessageRole::User,
            _ => MessageRole::Assistant,
        };
        let content = match role {
            MessageRole::User => user.to_string(),
            _ => assistant.to_string(),
        };
        app.messages.push(DisplayMessage::new(role, content));
    }
    app
}

fn main() {
    let msgs = env_usize("MUSH_PROFILE_MSGS", 1024);
    let secs = env_usize("MUSH_PROFILE_SECS", 10) as u64;

    let app = build_app(msgs);
    let area = Rect::new(0, 0, 100, 40);
    let mut buf = Buffer::empty(area);

    let warm_start = Instant::now();
    Ui::new(&app).render(area, &mut buf);
    let warm = warm_start.elapsed();

    eprintln!("warm-up render took {warm:.2?}");
    eprintln!("rendering {msgs}-message conversation for {secs}s (warm caches)...");
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut frames = 0u64;
    let loop_start = Instant::now();
    while Instant::now() < deadline {
        for _ in 0..512 {
            Ui::new(&app).render(area, &mut buf);
            frames += 1;
        }
    }
    let per_frame = loop_start.elapsed().as_nanos() as f64 / frames as f64;
    std::hint::black_box(&buf);
    eprintln!("rendered {frames} frames, {per_frame:.0}ns/frame");
}
