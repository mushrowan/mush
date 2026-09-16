use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mush_ai::types::{ImageMimeType, TokenCount};
use mush_tui::app::{
    App, DisplayMessage, DisplayToolCall, IMAGE_PLACEHOLDER, InputBuffer, MessageRole,
    PendingImage, ToolCallStatus,
};
use mush_tui::markdown;
use mush_tui::screen::Screen;
use mush_tui::theme::Theme;
use mush_tui::ui;
use mush_tui::widgets::input_box::benchmark_build_input_layout;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fn bench_transcript_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("transcript_scaling");
    let area = Rect::new(0, 0, 100, 40);

    for &count in &[16usize, 64, 256, 1024] {
        let app = conversation_app(count);
        let mut screen = Screen::new();
        let mut buf = Buffer::empty(area);
        group.throughput(Throughput::Elements(count as u64));
        screen.render(&app, area, &mut buf);
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| {
                screen.render(black_box(&app), area, &mut buf);
                black_box(&buf);
            })
        });
    }
    group.finish();
}

fn bench_transcript_with_tool_output(c: &mut Criterion) {
    let mut group = c.benchmark_group("transcript_with_tool_output");
    let area = Rect::new(0, 0, 100, 40);

    for &count in &[64usize, 256] {
        let app = tool_heavy_app(count);
        let mut screen = Screen::new();
        let mut buf = Buffer::empty(area);
        group.throughput(Throughput::Elements(count as u64));
        screen.render(&app, area, &mut buf);
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, _| {
            b.iter(|| {
                screen.render(black_box(&app), area, &mut buf);
                black_box(&buf);
            })
        });
    }
    group.finish();
}

fn tool_heavy_app(message_count: usize) -> App {
    let mut app = conversation_app(message_count);
    let output = code_block_body_fixture(10);
    for (i, mut msg) in app.messages.iter_mut().enumerate() {
        if matches!(msg.role, MessageRole::Assistant) {
            msg.tool_calls.push(DisplayToolCall {
                tool_call_id: "tc".into(),
                name: "read".into(),
                summary: format!("crates/mush-tui/src/widgets/message_list{i}.rs"),
                status: ToolCallStatus::Done,
                output: Some(output.clone()),
                image_data: None,
                batch: 0,
                bash_done: None,
                cell: None,
            });
        }
    }
    app
}

fn conversation_app(message_count: usize) -> App {
    let mut app = App::new("bench-model".into(), TokenCount::new(200_000));
    let user = "summarise the performance tradeoffs in the render path";
    let assistant = "## summary\n\nthe hot path builds a `Vec<Line>` per frame, with \
        **bold** and `inline code` spans plus a fenced block:\n\n\
        ```rust\nfn render(buf: &mut Buffer) {\n    set_line(buf, line);\n}\n```\n\n\
        that covers wrapping, span allocation and width lookups.";
    for i in 0..message_count {
        let (role, content) = if i % 2 == 0 {
            (MessageRole::User, user.to_string())
        } else {
            (MessageRole::Assistant, assistant.to_string())
        };
        app.messages.push(DisplayMessage::new(role, content));
    }
    app
}

fn bench_markdown(c: &mut Criterion) {
    let source = markdown_fixture();
    let theme = Theme::default();
    let mut group = c.benchmark_group("markdown");
    group.throughput(Throughput::Bytes(source.len() as u64));
    group.bench_function("render_large", |b| {
        b.iter(|| markdown::render(black_box(source.as_str()), black_box(&theme)))
    });
    group.finish();
}

fn bench_code_blocks(c: &mut Criterion) {
    let rust_small_source = rust_code_block_fixture(24);
    let rust_large_source = rust_code_block_fixture(96);
    let plain_source = plain_code_block_fixture(96);
    let unknown_source = unknown_code_block_fixture(96);
    let theme = Theme::default();
    let mut group = c.benchmark_group("code_block");

    group.throughput(Throughput::Bytes(rust_small_source.len() as u64));
    group.bench_function("render_rust_small", |b| {
        b.iter(|| markdown::render(black_box(rust_small_source.as_str()), black_box(&theme)))
    });

    group.throughput(Throughput::Bytes(rust_large_source.len() as u64));
    group.bench_function("render_rust_large", |b| {
        b.iter(|| markdown::render(black_box(rust_large_source.as_str()), black_box(&theme)))
    });

    group.throughput(Throughput::Bytes(plain_source.len() as u64));
    group.bench_function("render_plain", |b| {
        b.iter(|| markdown::render(black_box(plain_source.as_str()), black_box(&theme)))
    });

    group.throughput(Throughput::Bytes(unknown_source.len() as u64));
    group.bench_function("render_unknown", |b| {
        b.iter(|| markdown::render(black_box(unknown_source.as_str()), black_box(&theme)))
    });

    group.finish();
}

fn bench_input_layout(c: &mut Criterion) {
    let (input, images) = input_fixture();
    let cursor = input.len();
    let mut group = c.benchmark_group("input_layout");
    group.throughput(Throughput::Bytes(input.len() as u64));
    group.bench_function("build_layout", |b| {
        b.iter(|| {
            benchmark_build_input_layout(
                black_box(input.as_str()),
                black_box(cursor),
                black_box(images.as_slice()),
                black_box(48),
            )
        })
    });

    let mut cached = InputBuffer::new();
    cached.text = input;
    cached.cursor = cursor;
    cached.images = images;
    let _ = ui::input_height(&cached, 50);

    group.bench_function("cached_height", |b| {
        b.iter(|| ui::input_height(black_box(&cached), black_box(50)))
    });
    group.finish();
}

fn markdown_fixture() -> String {
    let section = r#"# repo overview

## hotspots

- `crates/mush-tui/src/markdown.rs`
- `crates/mush-tui/src/widgets/input_box.rs`
- `crates/mush-ai/src/providers/openai_responses.rs`

### notes

this paragraph has **bold**, *italic*, `inline code`, and enough repeated text to make wrapping and span allocation interesting for the renderer.

```rust
fn render_markdown_cached(source: &str) {
    println!("{source}");
}
```
"#;

    section.repeat(24)
}

fn rust_code_block_fixture(repeat: usize) -> String {
    format!("```rust\n{}```\n", code_block_body_fixture(repeat))
}

fn plain_code_block_fixture(repeat: usize) -> String {
    format!("```\n{}```\n", code_block_body_fixture(repeat))
}

fn unknown_code_block_fixture(repeat: usize) -> String {
    format!("```unknown\n{}```\n", code_block_body_fixture(repeat))
}

fn code_block_body_fixture(repeat: usize) -> String {
    let line = r#"fn render_markdown_cached(source: &str) {
    println!("{source}");
}
"#;
    line.repeat(repeat)
}

fn input_fixture() -> (String, Vec<PendingImage>) {
    let image = PendingImage {
        data: vec![],
        mime_type: ImageMimeType::Png,
        dimensions: Some((1920, 1080)),
    };
    let input = format!(
        "summarise the current refactor state {IMAGE_PLACEHOLDER}\nthen explain the likely performance tradeoffs in the tui layout path and provider streaming path while keeping the answer concise and readable"
    );
    (input.repeat(8), vec![image; 8])
}

criterion_group!(
    benches,
    bench_transcript_scaling,
    bench_transcript_with_tool_output,
    bench_markdown,
    bench_code_blocks,
    bench_input_layout
);
criterion_main!(benches);
