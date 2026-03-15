//! compaction prompt bench
//!
//! generates synthetic conversations of varying sizes, runs compaction
//! through a real LLM, and reports token usage, latency, and summary
//! quality metrics.
//!
//! run: cargo run -p mush-eval --bin bench_compaction -- --openrouter
//!
//! requires OPENROUTER_API_KEY in the environment.
//! uses gpt-5-nano by default (very cheap: $0.05/M in, $0.40/M out).

use std::time::Instant;

use mush_ai::providers;
use mush_ai::registry::{ApiRegistry, LlmContext};
use mush_ai::types::*;
use mush_session::compact;

const COMPACTION_SYSTEM: &str = "\
You are a context summarisation assistant. Your task is to read a conversation between \
a user and an AI coding assistant, then produce a structured summary following the exact \
format specified.

Do not continue the conversation. Do not respond to any questions in the conversation. \
Only output the structured summary.";

const SUMMARISATION_INSTRUCTIONS: &str = "\
Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or \"(none)\" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or \"(none)\" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages.";

// ── helpers ─────────────────────────────────────────────────────────────────

fn user_msg(text: &str) -> Message {
    Message::User(UserMessage {
        content: UserContent::Text(text.into()),
        timestamp_ms: Timestamp::zero(),
    })
}

fn assistant_msg(text: &str) -> Message {
    Message::Assistant(AssistantMessage {
        content: vec![AssistantContentPart::Text(TextContent {
            text: text.into(),
        })],
        model: "bench".into(),
        provider: Provider::Custom("bench".into()),
        api: Api::OpenaiCompletions,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp_ms: Timestamp::zero(),
    })
}

fn assistant_with_tool(text: &str, tool_name: &str, args: &str) -> Message {
    Message::Assistant(AssistantMessage {
        content: vec![
            AssistantContentPart::Text(TextContent { text: text.into() }),
            AssistantContentPart::ToolCall(ToolCall {
                id: ToolCallId::from(format!("tc_{tool_name}")),
                name: ToolName::from(tool_name),
                arguments: serde_json::from_str(args).unwrap_or_default(),
            }),
        ],
        model: "bench".into(),
        provider: Provider::Custom("bench".into()),
        api: Api::OpenaiCompletions,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp_ms: Timestamp::zero(),
    })
}

fn tool_result(tool_name: &str, output: &str, error: bool) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: format!("tc_{tool_name}").into(),
        tool_name: tool_name.into(),
        content: vec![ToolResultContentPart::Text(TextContent {
            text: output.into(),
        })],
        outcome: if error {
            ToolOutcome::Error
        } else {
            ToolOutcome::Success
        },
        timestamp_ms: Timestamp::zero(),
    })
}

// ── scenarios ───────────────────────────────────────────────────────────────

struct Scenario {
    name: &'static str,
    messages: Vec<Message>,
}

// text-only: n user/assistant pairs cycling through rust topics
fn text_conversation(pairs: usize) -> Vec<Message> {
    let topics = [
        (
            "explain how async/await works in rust",
            "async/await in rust works through the Future trait. when you write `async fn`, the compiler transforms it into a state machine that implements Future. the `.await` points become yield points where the executor can switch to other tasks. the key insight is that rust futures are lazy and do nothing until polled. tokio is the most common runtime.",
        ),
        (
            "how do i handle errors properly in rust",
            "the idiomatic approach is using the Result type with the ? operator. define custom error types with thiserror for library code and use anyhow or color-eyre for applications. avoid unwrap() in production code. the error trait lets you chain causes with source().",
        ),
        (
            "what is the borrow checker and why does it matter",
            "the borrow checker enforces ownership rules at compile time: each value has one owner, values can be borrowed immutably (multiple) or mutably (exclusive). this prevents data races and use-after-free bugs without a garbage collector. it is one of rust's key innovations for memory safety.",
        ),
        (
            "show me how to use iterators effectively",
            "iterators in rust are zero-cost abstractions. chain methods like .map(), .filter(), .collect(). use .iter() for references, .into_iter() for ownership. custom iterators implement the Iterator trait with a next() method. they compose without intermediate allocations.",
        ),
        (
            "how do i write tests in rust",
            "use #[test] for unit tests in the same file, #[cfg(test)] mod tests {} for the test module. integration tests go in tests/ directory. use assert!, assert_eq!, assert_ne! macros. #[should_panic] for expected failures. proptest for property-based testing.",
        ),
        (
            "explain lifetimes in simple terms",
            "lifetimes tell the compiler how long references are valid. they prevent dangling references. most of the time, lifetime elision handles them automatically. when you need explicit lifetimes, use 'a syntax to say 'these references live at least as long as each other'.",
        ),
        (
            "what are traits and how do they compare to interfaces",
            "traits are rust's way of defining shared behaviour. they can have default implementations, associated types, and generic bounds. unlike interfaces in java/go, traits support blanket implementations and can be implemented for types you don't own (orphan rule limits this).",
        ),
        (
            "how should i structure a large rust project",
            "use a workspace with multiple crates. keep the binary thin (just cli/main), put logic in library crates. group related functionality into modules. use pub(crate) for internal APIs. separate concerns: types, logic, io, config.",
        ),
        (
            "explain smart pointers in rust",
            "Box<T> for heap allocation, Rc<T> for shared ownership (single-threaded), Arc<T> for shared ownership (multi-threaded). RefCell<T> and Mutex<T> for interior mutability. Cow<T> for clone-on-write. Pin<T> for self-referential types.",
        ),
        (
            "how do i do concurrency in rust",
            "use tokio or async-std for async concurrency. for CPU-bound work, use rayon for data parallelism or std::thread for OS threads. channels (mpsc, crossbeam) for message passing. Arc<Mutex<T>> for shared state. avoid shared mutable state when possible.",
        ),
    ];

    let mut msgs = Vec::with_capacity(pairs * 2);
    for i in 0..pairs {
        let (q, a) = topics[i % topics.len()];
        let suffix = if i >= topics.len() {
            format!(" (follow-up #{}, more detail please)", i / topics.len())
        } else {
            String::new()
        };
        msgs.push(user_msg(&format!("{q}{suffix}")));
        msgs.push(assistant_msg(a));
    }
    msgs
}

// tool-heavy: rounds of assistant tool calls + results
fn tool_conversation(rounds: usize) -> Vec<Message> {
    let file_ops = [
        (
            "read src/main.rs to understand the entry point",
            "read",
            r#"{"path": "src/main.rs"}"#,
            "fn main() {\n    let config = Config::load();\n    let app = App::new(config);\n    app.run();\n}\n\nstruct Config {\n    port: u16,\n    host: String,\n}\n\nimpl Config {\n    fn load() -> Self {\n        Config { port: 8080, host: \"localhost\".into() }\n    }\n}",
        ),
        (
            "i'll add error handling to the config loading",
            "edit",
            r#"{"path": "src/main.rs", "old": "fn load() -> Self", "new": "fn load() -> Result<Self, ConfigError>"}"#,
            "applied edit: src/main.rs (1 replacement)",
        ),
        (
            "let me check the test file",
            "read",
            r#"{"path": "tests/integration.rs"}"#,
            "#[test]\nfn test_config_loads() {\n    let config = Config::load().unwrap();\n    assert_eq!(config.port, 8080);\n}\n\n#[test]\nfn test_app_starts() {\n    let config = Config::load().unwrap();\n    let app = App::new(config);\n    assert!(app.is_running());\n}",
        ),
        (
            "running the tests to check for regressions",
            "bash",
            r#"{"command": "cargo test"}"#,
            "   Compiling myapp v0.1.0\n    Finished test [unoptimized + debuginfo]\n     Running tests/integration.rs\n\nrunning 2 tests\ntest test_config_loads ... ok\ntest test_app_starts ... ok\n\ntest result: ok. 2 passed; 0 failed",
        ),
        (
            "writing a new module for database connections",
            "write",
            r#"{"path": "src/db.rs", "content": "pub struct Pool { ... }"}"#,
            "wrote 42 lines to src/db.rs",
        ),
        (
            "let me check if there are any clippy warnings",
            "bash",
            r#"{"command": "cargo clippy"}"#,
            "warning: unused variable `pool` in src/db.rs:15\nwarning: method `connect` is never used in src/db.rs:22\n\n2 warnings generated",
        ),
        (
            "fixing the clippy warnings by using the pool",
            "edit",
            r#"{"path": "src/db.rs", "old": "let pool = Pool::new();", "new": "let pool = Pool::new();\n    pool.connect();"}"#,
            "applied edit: src/db.rs (1 replacement)",
        ),
        (
            "checking the project structure",
            "bash",
            r#"{"command": "find src -name '*.rs' | head -20"}"#,
            "src/main.rs\nsrc/db.rs\nsrc/config.rs\nsrc/app.rs\nsrc/routes/mod.rs\nsrc/routes/api.rs\nsrc/middleware/auth.rs",
        ),
    ];

    let mut msgs = Vec::with_capacity(rounds * 3);
    for i in 0..rounds {
        let (thought, tool_name, args, output) = file_ops[i % file_ops.len()];
        let suffix = if i >= file_ops.len() {
            format!(" (iteration {})", i / file_ops.len() + 1)
        } else {
            String::new()
        };
        if i > 0 && i % 3 == 0 {
            msgs.push(user_msg(&format!(
                "good, continue with the next step{suffix}"
            )));
        }
        msgs.push(assistant_with_tool(
            &format!("{thought}{suffix}"),
            tool_name,
            args,
        ));
        msgs.push(tool_result(tool_name, output, false));
    }
    msgs
}

// mixed: text intro then tool work then an error
fn mixed_conversation(size: usize) -> Vec<Message> {
    let mut msgs = Vec::new();
    let half = size / 2;
    msgs.extend(text_conversation(half.min(5)));
    msgs.extend(tool_conversation(half));
    msgs.push(user_msg("try running the formatter"));
    msgs.push(assistant_with_tool(
        "running rustfmt on the project",
        "bash",
        r#"{"command": "cargo fmt --check"}"#,
    ));
    msgs.push(tool_result(
        "bash",
        "error: could not compile `myapp`\n  --> src/db.rs:15:9\n   |\n15 |         pool.connect()\n   |         ^^^^ missing semicolon\n\nerror: aborting due to previous error",
        true,
    ));
    msgs.push(assistant_msg(
        "there is a syntax error in src/db.rs. let me fix the missing semicolon.",
    ));
    msgs
}

fn build_scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "text_small_20",
            messages: text_conversation(10),
        },
        Scenario {
            name: "text_medium_40",
            messages: text_conversation(20),
        },
        Scenario {
            name: "text_large_80",
            messages: text_conversation(40),
        },
        Scenario {
            name: "tools_small_16",
            messages: tool_conversation(8),
        },
        Scenario {
            name: "tools_medium_32",
            messages: tool_conversation(16),
        },
        Scenario {
            name: "tools_large_64",
            messages: tool_conversation(32),
        },
        Scenario {
            name: "mixed_medium",
            messages: mixed_conversation(20),
        },
        Scenario {
            name: "mixed_large",
            messages: mixed_conversation(40),
        },
    ]
}

// ── bench result ────────────────────────────────────────────────────────────

struct BenchResult {
    scenario: String,
    message_count: usize,
    estimated_tokens: usize,
    prompt_chars: usize,
    input_tokens: u64,
    output_tokens: u64,
    summary_chars: usize,
    summary_sections: usize,
    latency_ms: u64,
    has_goal: bool,
    has_progress: bool,
    has_next_steps: bool,
    #[allow(dead_code)]
    kept_messages: usize,
    compression_ratio: f64,
    summary_text: String,
}

fn count_sections(text: &str) -> usize {
    text.lines().filter(|l| l.starts_with("## ")).count()
}

fn has_section(text: &str, heading: &str) -> bool {
    text.to_lowercase()
        .contains(&format!("## {}", heading.to_lowercase()))
}

// ── config ──────────────────────────────────────────────────────────────────

struct BenchConfig {
    model: Model,
    options: StreamOptions,
}

fn parse_args() -> BenchConfig {
    let args: Vec<String> = std::env::args().collect();
    let mut model_id = "openai/gpt-5-nano".to_string();
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "--openrouter" => {}
            "--model" if i + 1 < args.len() => {
                i += 1;
                model_id = args[i].clone();
            }
            _ => {
                eprintln!("usage: bench_compaction [--openrouter] [--model MODEL_ID]");
                eprintln!();
                eprintln!("  --openrouter    use openrouter (default)");
                eprintln!("  --model ID      model to use (default: openai/gpt-5-nano)");
                eprintln!();
                eprintln!("requires OPENROUTER_API_KEY in environment");
                std::process::exit(1);
            }
        }
        i += 1;
    }

    let api_key = std::env::var("OPENROUTER_API_KEY").unwrap_or_else(|_| {
        eprintln!("error: OPENROUTER_API_KEY not set");
        std::process::exit(1);
    });

    let model = Model {
        id: model_id.clone().into(),
        name: model_id.into(),
        api: Api::OpenaiCompletions,
        provider: Provider::OpenRouter,
        base_url: "https://openrouter.ai/api/v1".into(),
        reasoning: false,
        input: vec![InputModality::Text],
        cost: ModelCost {
            input: 0.05,
            output: 0.40,
            cache_read: 0.005,
            cache_write: 0.0,
        },
        context_window: TokenCount::new(400_000),
        max_output_tokens: TokenCount::new(4096),
    };

    let options = StreamOptions {
        api_key: ApiKey::new(api_key),
        max_tokens: Some(TokenCount::new(4096)),
        ..Default::default()
    };

    BenchConfig { model, options }
}

// ── bench runner ────────────────────────────────────────────────────────────

async fn run_scenario(
    registry: &ApiRegistry,
    model: &Model,
    options: &StreamOptions,
    scenario: &Scenario,
) -> BenchResult {
    let msg_count = scenario.messages.len();
    let est_tokens = compact::estimate_tokens(&scenario.messages);

    let keep = 10usize;
    let split_at = msg_count.saturating_sub(keep);
    let old_messages = &scenario.messages[..split_at];

    // build the prompt (same as llm_compact)
    let conversation_dump = compact::build_compaction_prompt(old_messages);
    let prompt_chars = conversation_dump.len();

    let context = LlmContext {
        system_prompt: Some(COMPACTION_SYSTEM.to_string()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Text(format!(
                "<conversation>\n{conversation_dump}\n</conversation>\n\n{SUMMARISATION_INSTRUCTIONS}"
            )),
            timestamp_ms: Timestamp::now(),
        })],
        tools: vec![],
    };

    let provider = registry.get(model.api).expect("no provider for api");

    let start = Instant::now();
    let resp = mush_eval::collect::collect_response(provider, model, &context, options).await;
    let latency = start.elapsed().as_millis() as u64;

    let (summary_text, usage) = match resp {
        Ok(msg) => {
            let text: String = msg
                .content
                .iter()
                .filter_map(|p| match p {
                    AssistantContentPart::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            (text, msg.usage)
        }
        Err(e) => {
            eprintln!("    error: {e}");
            (String::new(), Usage::default())
        }
    };

    // run compaction with the summary to get the final message list
    let result =
        compact::compact_with_summary(scenario.messages.clone(), &summary_text, Some(keep));

    let post_tokens = compact::estimate_tokens(&result.messages);
    let compression = if est_tokens > 0 {
        post_tokens as f64 / est_tokens as f64
    } else {
        1.0
    };

    BenchResult {
        scenario: scenario.name.into(),
        message_count: msg_count,
        estimated_tokens: est_tokens,
        prompt_chars,
        input_tokens: usage.input_tokens.get(),
        output_tokens: usage.output_tokens.get(),
        summary_chars: summary_text.len(),
        summary_sections: count_sections(&summary_text),
        latency_ms: latency,
        has_goal: has_section(&summary_text, "goal"),
        has_progress: has_section(&summary_text, "progress"),
        has_next_steps: has_section(&summary_text, "next steps"),
        kept_messages: result.messages.len(),
        compression_ratio: compression,
        summary_text,
    }
}

#[tokio::main]
async fn main() {
    let config = parse_args();

    eprintln!("compaction prompt bench");
    eprintln!("model: {}", config.model.id);
    eprintln!();

    let http_client = reqwest::Client::new();
    let mut registry = ApiRegistry::new();
    providers::register_builtins(&mut registry, http_client);

    let scenarios = build_scenarios();
    eprintln!("running {} scenarios...\n", scenarios.len());

    let mut results = Vec::new();
    for scenario in &scenarios {
        let est = compact::estimate_tokens(&scenario.messages);
        eprint!(
            "  {:20} {:>3} msgs, ~{:>5} tokens ... ",
            scenario.name,
            scenario.messages.len(),
            est,
        );
        let result = run_scenario(&registry, &config.model, &config.options, scenario).await;
        eprintln!(
            "{}ms, {} chars, {} sections, {:.0}% kept",
            result.latency_ms,
            result.summary_chars,
            result.summary_sections,
            result.compression_ratio * 100.0,
        );
        results.push(result);
    }

    // results table
    eprintln!("\n{}", "=".repeat(130));
    eprintln!(
        "{:<20} {:>4} {:>7} {:>7} {:>7} {:>7} {:>5} {:>4} {:>4} {:>4} {:>6} {:>7}",
        "scenario",
        "msgs",
        "est_tok",
        "in_tok",
        "out_tok",
        "prompt",
        "sects",
        "goal",
        "prog",
        "next",
        "kept%",
        "ms",
    );
    eprintln!("{}", "-".repeat(130));

    for r in &results {
        eprintln!(
            "{:<20} {:>4} {:>7} {:>7} {:>7} {:>7} {:>5} {:>4} {:>4} {:>4} {:>5.0}% {:>7}",
            r.scenario,
            r.message_count,
            r.estimated_tokens,
            r.input_tokens,
            r.output_tokens,
            r.prompt_chars,
            r.summary_sections,
            if r.has_goal { "y" } else { "n" },
            if r.has_progress { "y" } else { "n" },
            if r.has_next_steps { "y" } else { "n" },
            r.compression_ratio * 100.0,
            r.latency_ms,
        );
    }
    eprintln!("{}", "-".repeat(130));

    // totals
    let total_ms: u64 = results.iter().map(|r| r.latency_ms).sum();
    let total_in: u64 = results.iter().map(|r| r.input_tokens).sum();
    let total_out: u64 = results.iter().map(|r| r.output_tokens).sum();
    let avg_ms = total_ms / results.len().max(1) as u64;
    let cost_in = total_in as f64 * 0.05 / 1_000_000.0;
    let cost_out = total_out as f64 * 0.40 / 1_000_000.0;

    eprintln!(
        "\ntotal: {}ms ({} avg), {} in + {} out tokens",
        total_ms, avg_ms, total_in, total_out,
    );
    eprintln!(
        "estimated cost: ${:.6} (${:.6} in + ${:.6} out)",
        cost_in + cost_out,
        cost_in,
        cost_out,
    );

    // quality
    let n = results.len();
    let goals = results.iter().filter(|r| r.has_goal).count();
    let progress = results.iter().filter(|r| r.has_progress).count();
    let nexts = results.iter().filter(|r| r.has_next_steps).count();
    eprintln!("quality: {goals}/{n} goal, {progress}/{n} progress, {nexts}/{n} next steps");

    // sample summaries
    eprintln!("\n{}", "=".repeat(80));
    eprintln!("sample summaries:\n");
    let show = [0, results.len() / 2, results.len() - 1];
    for &idx in &show {
        if idx < results.len() {
            let r = &results[idx];
            eprintln!("--- {} ({} msgs) ---", r.scenario, r.message_count);
            // show first 40 lines
            for (i, line) in r.summary_text.lines().enumerate() {
                if i >= 40 {
                    eprintln!("  ...(truncated)");
                    break;
                }
                eprintln!("  {line}");
            }
            eprintln!();
        }
    }
}
