use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use mush_ai::providers::sse::SseParser;
use mush_ai::providers::{anthropic, openai, openai_responses};

fn bench_tool_call_deltas(c: &mut Criterion) {
    let mut group = c.benchmark_group("tool_call_deltas");

    for (chunks, arg_bytes) in [(32usize, 4096usize), (128, 16384)] {
        group.throughput(Throughput::Bytes(arg_bytes as u64));
        group.bench_with_input(
            BenchmarkId::new("openai", format!("{chunks}x{arg_bytes}")),
            &(chunks, arg_bytes),
            |b, &(chunks, arg_bytes)| {
                b.iter(|| {
                    openai::benchmark_tool_call_deltas(black_box(chunks), black_box(arg_bytes))
                })
            },
        );
        group.bench_with_input(
            BenchmarkId::new("openai_responses", format!("{chunks}x{arg_bytes}")),
            &(chunks, arg_bytes),
            |b, &(chunks, arg_bytes)| {
                b.iter(|| {
                    openai_responses::benchmark_tool_call_deltas(
                        black_box(chunks),
                        black_box(arg_bytes),
                    )
                })
            },
        );
        group.bench_with_input(
            BenchmarkId::new("anthropic", format!("{chunks}x{arg_bytes}")),
            &(chunks, arg_bytes),
            |b, &(chunks, arg_bytes)| {
                b.iter(|| {
                    anthropic::benchmark_tool_call_deltas(black_box(chunks), black_box(arg_bytes))
                })
            },
        );
    }

    group.finish();
}

fn bench_sse_parser(c: &mut Criterion) {
    let mut group = c.benchmark_group("sse_parser");

    let long_event = format!("event: delta\ndata: {}\n\n", "x".repeat(256 * 1024));
    group.throughput(Throughput::Bytes(long_event.len() as u64));
    group.bench_function("one_long_event_in_1k_chunks", |b| {
        b.iter(|| {
            let mut parser = SseParser::new();
            let mut events = 0;
            for chunk in long_event.as_bytes().chunks(1024) {
                events += parser.push(black_box(chunk)).len();
            }
            events
        })
    });

    let many_events: String = (0..2048)
        .map(|i| format!("event: delta\ndata: {{\"index\":{i},\"text\":\"hello\"}}\n\n"))
        .collect();
    group.throughput(Throughput::Bytes(many_events.len() as u64));
    group.bench_function("many_events_in_one_chunk", |b| {
        b.iter(|| {
            SseParser::new()
                .push(black_box(many_events.as_bytes()))
                .len()
        })
    });

    group.finish();
}

criterion_group!(benches, bench_tool_call_deltas, bench_sse_parser);
criterion_main!(benches);
