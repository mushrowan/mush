use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use mush_ai::types::ImageMimeType;
use mush_tui::app::{IMAGE_PLACEHOLDER, InputBuffer, PendingImage};
use mush_tui::markdown;
use mush_tui::ui;
use mush_tui::widgets::input_box::benchmark_build_input_layout;

fn bench_markdown(c: &mut Criterion) {
    let source = markdown_fixture();
    let mut group = c.benchmark_group("markdown");
    group.throughput(Throughput::Bytes(source.len() as u64));
    group.bench_function("render_large", |b| {
        b.iter(|| markdown::render(black_box(source.as_str())))
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

criterion_group!(benches, bench_markdown, bench_input_layout);
criterion_main!(benches);
