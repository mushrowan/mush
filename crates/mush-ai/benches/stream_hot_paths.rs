use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
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

criterion_group!(benches, bench_tool_call_deltas);
criterion_main!(benches);
