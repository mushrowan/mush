use std::sync::Arc;
use std::time::Duration;

use mush_kernel::{HostError, HostRequestHandler, Kernel, KernelConfig, KernelError, NoHost};
use serde_json::{Value, json};

async fn kernel() -> Kernel {
    kernel_with(Arc::new(NoHost)).await
}

#[expect(clippy::panic, reason = "the suite needs python3 on PATH")]
async fn kernel_with(handler: Arc<dyn HostRequestHandler>) -> Kernel {
    match Kernel::start(KernelConfig::default(), handler).await {
        Ok(kernel) => kernel,
        Err(error) => panic!("kernel start (needs python3 on PATH): {error}"),
    }
}

#[tokio::test]
async fn state_persists_and_a_trailing_expression_is_the_result() {
    let kernel = kernel().await;

    let first = kernel.execute("x = 20").await.unwrap();
    let second = kernel.execute("x + 1").await.unwrap();

    assert!(first.ok);
    assert_eq!(first.result, None);
    assert_eq!(second.result.as_deref(), Some("21"));
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn prints_land_in_stdout_and_a_none_expression_has_no_result() {
    let kernel = kernel().await;

    let output = kernel.execute("print('hi')\nprint('there')").await.unwrap();

    assert_eq!(output.stdout, "hi\nthere\n");
    assert_eq!(output.result, None);
    assert!(output.ok);
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_exception_is_reported_with_its_source_line_and_leaves_the_kernel_alive() {
    let kernel = kernel().await;

    let failed = kernel.execute("y = 2\n1 / 0").await.unwrap();
    let after = kernel.execute("y").await.unwrap();

    assert!(!failed.ok);
    let error = failed.error.unwrap();
    assert_eq!(error.ename, "ZeroDivisionError");
    assert!(
        error.traceback.iter().any(|line| line.contains("1 / 0")),
        "{:?}",
        error.traceback
    );
    assert_eq!(after.result.as_deref(), Some("2"));
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn top_level_await_works() {
    let kernel = kernel().await;

    let output = kernel
        .execute("import asyncio\nawait asyncio.sleep(0.01)\n'slept'")
        .await
        .unwrap();

    assert_eq!(output.result.as_deref(), Some("'slept'"));
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn raw_fd_writes_from_subprocesses_are_background_output_not_mixed_into_the_protocol() {
    let kernel = kernel().await;

    let output = kernel
        .execute("import subprocess\nsubprocess.run(['echo', 'raw'])\nprint('tagged')")
        .await
        .unwrap();

    assert_eq!(output.background, "raw\n");
    assert_eq!(output.stdout, "tagged\n");
    assert!(output.ok);
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn raw_output_at_the_end_of_a_cell_is_drained_before_done() {
    let kernel = kernel().await;

    let output = kernel
        .execute("import os\nos.write(1, b'last words\\n')\nx = 1")
        .await
        .unwrap();
    let next = kernel.execute("2").await.unwrap();

    assert_eq!(output.background, "last words\n");
    assert_eq!(next.background, "");
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn stdout_behaves_like_a_real_stream() {
    let kernel = kernel().await;

    let output = kernel
        .execute(
            "import sys, subprocess\nsys.stdout.buffer.write(b'bytes\\n')\nsubprocess.run(['echo', 'child'], stdout=sys.stdout)\n(sys.stdout.encoding, sys.stdout.fileno() > 2)",
        )
        .await
        .unwrap();

    assert!(output.ok, "{output:?}");
    assert_eq!(output.result.as_deref(), Some("('utf-8', True)"));
    assert!(output.background.contains("bytes\n"), "{output:?}");
    assert!(output.background.contains("child\n"), "{output:?}");
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_partial_line_streams_before_the_cell_ends() {
    let kernel = kernel().await;
    let mut first = None;

    let output = kernel
        .execute_streaming(
            "import time\nprint('a', end='')\ntime.sleep(0.2)\nprint('b')",
            |event| {
                if first.is_none()
                    && let mush_kernel::Event::Stdout { text, .. } = event
                {
                    first = Some(text.clone());
                }
            },
        )
        .await
        .unwrap();

    assert_eq!(first.as_deref(), Some("a"));
    assert_eq!(output.stdout, "ab\n");
    kernel.shutdown().await.unwrap();
}

struct Echo;

#[async_trait::async_trait]
impl HostRequestHandler for Echo {
    async fn handle(&self, kind: &str, payload: Value) -> Result<Value, HostError> {
        match kind {
            "echo" => Ok(json!({ "echoed": payload })),
            "bash.start" => Ok(json!({
                "job_id": format!(
                    "job:{}{}",
                    payload["command"].as_str().unwrap_or(""),
                    payload["origin"]
                        .as_str()
                        .map(|origin| format!("@{origin}"))
                        .unwrap_or_default()
                ),
                "pid": 42
            })),
            "bash.wait" => Ok(json!({
                "exit_code": 3,
                "output": format!("ran {}", payload["job_id"]),
                "running": false
            })),
            "rlm.find_models" => Ok(json!([{
                "id": format!("{}-{}", payload["query"].as_str().unwrap_or(""), payload["limit"]),
                "name": "Claude Haiku 5.5",
                "provider": "anthropic",
                "context_window": 1_000_000,
                "max_output_tokens": 128_000,
                "thinking": ["off", "low"],
                "cost": {"input": 0.1, "output": 0.5},
                "also_on": [payload["all_providers"].to_string()]
            }])),
            other => Err(HostError(format!("unknown request {other}"))),
        }
    }
}

#[tokio::test]
async fn a_host_request_round_trips_through_the_handler() {
    let kernel = kernel_with(Arc::new(Echo)).await;

    let output = kernel
        .execute("reply = await host_request('echo', {'a': 1})\nreply['echoed']['a']")
        .await
        .unwrap();

    assert_eq!(output.result.as_deref(), Some("1"));
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn find_models_turns_host_rows_into_model_infos() {
    let kernel = kernel_with(Arc::new(Echo)).await;

    let output = kernel
        .execute("[m] = await rlm.find_models('haiku', limit=2, all_providers=True)\n(m.id, m.provider, m.context_window, m.max_output_tokens, m.thinking, m.cost['output'], m.also_on, repr(m).startswith('ModelInfo('))")
        .await
        .unwrap();

    assert_eq!(
        output.result.as_deref(),
        Some("('haiku-2', 'anthropic', 1000000, 128000, ['off', 'low'], 0.5, ['true'], True)")
    );
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn bash_is_a_typed_host_request() {
    let kernel = kernel_with(Arc::new(Echo)).await;

    let output = kernel
        .execute("h = bash('ls')\nr = await h\n(r.exit_code, r.ok, r.output, h.pid)")
        .await
        .unwrap();

    assert_eq!(
        output.result.as_deref(),
        Some("(3, False, 'ran \"job:ls\"', 42)")
    );
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_handle_started_in_a_cell_carries_the_cell_origin() {
    let kernel = kernel_with(Arc::new(Echo)).await;

    kernel
        .execute_from("h = bash('ls')", Some("call_7"), |_| {})
        .await
        .unwrap();
    let output = kernel.execute("(await h).output").await.unwrap();

    assert_eq!(output.result.as_deref(), Some("'ran \"job:ls@call_7\"'"));
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_cell_that_swallows_the_interrupt_leaves_the_kernel_busy_until_killed() {
    let kernel = Arc::new(
        Kernel::start(
            KernelConfig {
                cwd: std::env::temp_dir(),
                busy_wait: Duration::from_millis(600),
                ..KernelConfig::default()
            },
            Arc::new(Echo),
        )
        .await
        .unwrap(),
    );
    let stubborn = {
        let kernel = kernel.clone();
        tokio::spawn(async move {
            kernel
                .execute(
                    "import time\nwhile True:\n    try:\n        time.sleep(0.05)\n    except KeyboardInterrupt:\n        pass",
                )
                .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    stubborn.abort();
    let _ = stubborn.await;

    let outcome = kernel.execute("1 + 1").await;

    assert!(matches!(outcome, Err(KernelError::Busy)), "{outcome:?}");
    assert!(kernel.busy());
    Arc::try_unwrap(kernel).ok().unwrap().kill().await;
}

#[tokio::test]
async fn a_rejected_host_request_raises_in_the_cell() {
    let kernel = kernel_with(Arc::new(Echo)).await;

    let output = kernel
        .execute("await host_request('nope', {})")
        .await
        .unwrap();

    let error = output.error.unwrap();
    assert_eq!(error.ename, "HostError");
    assert_eq!(error.evalue, "unknown request nope");
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_nan_or_oversized_payload_raises_in_the_cell_instead_of_leaving_the_wire() {
    let kernel = kernel_with(Arc::new(Echo)).await;

    let nan = kernel
        .execute("await host_request('echo', {'x': float('nan')})")
        .await
        .unwrap();
    let big = kernel
        .execute("display({'text/plain': 'x' * (17 * 1024 * 1024)})")
        .await
        .unwrap();
    let after = kernel.execute("1 + 1").await.unwrap();

    assert_eq!(nan.error.unwrap().ename, "ValueError");
    let big = big.error.unwrap();
    assert_eq!(big.ename, "ValueError");
    assert!(big.evalue.contains("frame cap"), "{}", big.evalue);
    assert_eq!(after.result.as_deref(), Some("2"));
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn output_streams_and_the_result_are_capped_per_cell() {
    let kernel = kernel().await;

    let output = kernel
        .execute("for _ in range(200):\n    print('o' * 1000)\nimport sys\nsys.stderr.write('e' * 100_000)\n'r' * 100_000")
        .await
        .unwrap();

    let marker = "\n[... output truncated at 65536 chars ...]";
    assert_eq!(output.stdout.len(), 65536 + marker.len());
    assert!(output.stdout.ends_with(marker));
    assert!(output.stderr.starts_with("eee"));
    assert!(output.stderr.ends_with(marker));
    let result = output.result.unwrap();
    assert!(result.starts_with("'rrr"));
    assert!(result.ends_with(marker));
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_huge_result_or_error_is_capped_in_transit() {
    let kernel = kernel().await;

    let result = kernel.execute("'x' * 2_000_000").await.unwrap();
    let error = kernel
        .execute("raise ValueError('y' * 2_000_000)")
        .await
        .unwrap();

    assert!(
        result
            .result
            .unwrap()
            .ends_with("[... output truncated at 65536 chars ...]")
    );
    let error = error.error.unwrap();
    assert!(error.evalue.len() < 1_100_000, "{}", error.evalue.len());
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_unparseable_frame_fails_the_running_cell_and_kills_the_kernel() {
    let kernel = kernel().await;

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        kernel.execute(
            "import os, sys, time\nshim = next(m for m in list(sys.modules.values()) if getattr(m, '__file__', '') and m.__file__.endswith('mush_kernel.py'))\nos.write(shim._proto_fd, b'{not json\\n')\ntime.sleep(30)",
        ),
    )
    .await
    .expect("the bad frame fails the cell");
    let after = kernel.execute("1 + 1").await;

    assert!(
        matches!(&outcome, Err(KernelError::Corrupted(reason)) if reason.starts_with("line is not a kernel event")),
        "{outcome:?}"
    );
    assert!(matches!(after, Err(KernelError::Corrupted(_))), "{after:?}");
    assert!(kernel.protocol_failure().is_some());
    kernel.shutdown().await.unwrap();
}

fn process_is_gone(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
        stat.rsplit(')')
            .next()
            .is_some_and(|rest| rest.trim_start().starts_with('Z'))
    })
}

#[tokio::test]
async fn dropping_a_kernel_kills_its_process() {
    let kernel = kernel().await;
    let pid = kernel.pid().unwrap();

    drop(kernel);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !process_is_gone(pid) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(process_is_gone(pid));
}

#[tokio::test]
async fn a_bare_interrupt_with_nothing_running_is_dropped() {
    let kernel = kernel().await;

    kernel.interrupt().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let output = kernel.execute("1 + 1").await.unwrap();

    assert!(!output.interrupted(), "{output:?}");
    assert_eq!(output.result.as_deref(), Some("2"));
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_interrupt_for_a_finished_cell_does_not_touch_the_next_one() {
    let kernel = kernel().await;

    let first = kernel.execute("1").await.unwrap();
    kernel.interrupt_cell("cell-1").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let second = kernel.execute("2").await.unwrap();

    assert_eq!(first.result.as_deref(), Some("1"));
    assert!(!second.interrupted(), "{second:?}");
    assert_eq!(second.result.as_deref(), Some("2"));
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_interrupt_naming_another_cell_leaves_the_running_one_alone() {
    let kernel = Arc::new(kernel().await);

    let runner = {
        let kernel = kernel.clone();
        tokio::spawn(async move {
            kernel
                .execute("import time\ntime.sleep(0.4)\n'slept'")
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    kernel.interrupt_cell("cell-does-not-exist").await.unwrap();

    let output = tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .expect("the cell finishes")
        .unwrap()
        .unwrap();
    assert!(!output.interrupted(), "{output:?}");
    assert_eq!(output.result.as_deref(), Some("'slept'"));
    Arc::try_unwrap(kernel)
        .ok()
        .unwrap()
        .shutdown()
        .await
        .unwrap();
}

#[tokio::test]
async fn a_busy_loop_can_be_interrupted() {
    let kernel = Arc::new(kernel().await);

    let runner = {
        let kernel = kernel.clone();
        tokio::spawn(async move {
            kernel
                .execute("n = 0\nwhile True:\n    n += 1")
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    kernel.interrupt().await.unwrap();
    let output = tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .expect("interrupt landed")
        .unwrap();

    assert!(output.interrupted(), "{output:?}");
    let after = kernel.execute("n > 0").await.unwrap();
    assert_eq!(after.result.as_deref(), Some("True"));
    Arc::try_unwrap(kernel)
        .ok()
        .unwrap()
        .shutdown()
        .await
        .unwrap();
}

#[tokio::test]
async fn an_awaiting_cell_can_be_interrupted() {
    let kernel = Arc::new(kernel().await);

    let runner = {
        let kernel = kernel.clone();
        tokio::spawn(async move {
            kernel
                .execute("import asyncio\nawait asyncio.sleep(30)")
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    kernel.interrupt().await.unwrap();
    let output = tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .expect("interrupt landed")
        .unwrap();

    assert!(output.interrupted(), "{output:?}");
    Arc::try_unwrap(kernel)
        .ok()
        .unwrap()
        .shutdown()
        .await
        .unwrap();
}

#[tokio::test]
async fn dropping_an_execute_future_stops_the_cell_and_frees_the_kernel() {
    let kernel = Arc::new(kernel().await);

    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let runner = {
        let kernel = kernel.clone();
        tokio::spawn(async move {
            let mut started = Some(started_tx);
            let _ = kernel
                .execute_streaming(
                    "print('running', flush=True)\nn = 0\nwhile True:\n    n += 1",
                    |event| {
                        if matches!(event, mush_kernel::Event::Stdout { .. })
                            && let Some(tx) = started.take()
                        {
                            let _ = tx.send(());
                        }
                    },
                )
                .await;
        })
    };
    tokio::time::timeout(Duration::from_secs(5), started_rx)
        .await
        .expect("the cell starts")
        .unwrap();
    runner.abort();
    let _ = runner.await;

    let after = tokio::time::timeout(Duration::from_secs(5), kernel.execute("'alive'"))
        .await
        .expect("the abandoned loop was interrupted, so the kernel takes a new cell")
        .unwrap();
    assert_eq!(after.result.as_deref(), Some("'alive'"));
    assert!(!kernel.busy());
    Arc::try_unwrap(kernel)
        .ok()
        .unwrap()
        .shutdown()
        .await
        .unwrap();
}

#[tokio::test]
async fn a_snapshot_survives_a_fresh_kernel() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.pickle");
    let first = kernel().await;
    first
        .execute("keep = {'a': [1, 2]}\nunpicklable = (i for i in range(3))")
        .await
        .unwrap();

    let report = first.snapshot(&path, None).await.unwrap();
    first.shutdown().await.unwrap();

    assert_eq!(report.saved, ["keep"]);
    assert_eq!(report.skipped, ["unpicklable"]);
    let second = kernel().await;
    let restored = second.restore(&path).await.unwrap();
    assert_eq!(restored.restored, ["keep"]);
    let output = second.execute("keep['a'][1]").await.unwrap();
    assert_eq!(output.result.as_deref(), Some("2"));
    assert_eq!(second.list_names().await.unwrap(), ["keep"]);
    second.shutdown().await.unwrap();
}

#[tokio::test]
async fn without_dill_cell_functions_and_classes_are_skipped_not_saved_broken() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.pickle");
    let kernel = kernel().await;
    kernel
        .execute(
            "def helper():\n    return 1\nclass Thing:\n    pass\nthing = Thing()\nplain = [1]",
        )
        .await
        .unwrap();
    let has_dill = kernel
        .execute("import importlib.util\nimportlib.util.find_spec('dill') is not None")
        .await
        .unwrap();

    let report = kernel.snapshot(&path, None).await.unwrap();

    if has_dill.result.as_deref() == Some("True") {
        assert_eq!(report.skipped, Vec::<String>::new());
    } else {
        assert_eq!(report.saved, ["plain"]);
        assert_eq!(report.skipped, ["helper", "Thing", "thing", "importlib"]);
    }
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn with_dill_functions_classes_and_imports_survive_a_fresh_kernel() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.pickle");
    let first = kernel().await;
    let has_dill = first
        .execute("import importlib.util\nimportlib.util.find_spec('dill') is not None")
        .await
        .unwrap();
    if has_dill.result.as_deref() != Some("True") {
        first.shutdown().await.unwrap();
        return;
    }
    first
        .execute("import json\ndef helper(x):\n    return x + 1\nclass Thing:\n    def value(self):\n        return helper(41)\nthing = Thing()")
        .await
        .unwrap();

    let report = first.snapshot(&path, None).await.unwrap();
    first.shutdown().await.unwrap();
    let second = kernel().await;
    second.restore(&path).await.unwrap();
    let output = second
        .execute("(thing.value(), json.dumps(helper(1)), Thing.__name__)")
        .await
        .unwrap();

    assert!(report.skipped.is_empty(), "{report:?}");
    assert_eq!(
        output.result.as_deref(),
        Some("(42, '2', 'Thing')"),
        "{output:?}"
    );
    second.shutdown().await.unwrap();
}

#[tokio::test]
async fn cells_run_in_a_real_main_module() {
    let kernel = kernel().await;

    let output = kernel
        .execute("import sys\ndef f():\n    pass\n(f.__module__, sys.modules['__main__'].f is f)")
        .await
        .unwrap();

    assert_eq!(output.result.as_deref(), Some("('__main__', True)"));
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_snapshot_that_stalls_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let kernel = kernel().await;
    kernel
        .execute("import time\nclass Slow:\n    def __reduce__(self):\n        time.sleep(7)\n        return (Slow, ())\nslow = Slow()")
        .await
        .unwrap();

    let started = std::time::Instant::now();
    let outcome = kernel.snapshot(&dir.path().join("s.pickle"), None).await;

    assert!(
        matches!(outcome, Err(KernelError::RequestTimeout(_))),
        "{outcome:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(6));
    kernel.kill().await;
}

async fn appears(path: &std::path::Path) {
    while !path.exists() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn slow_to_pickle(marker: &std::path::Path) -> String {
    format!(
        "import pathlib, time\nclass Slow:\n    def __reduce__(self):\n        pathlib.Path({:?}).touch()\n        time.sleep(30)\n        return (Slow, ())\nkeep = 41\nslow = Slow()",
        marker.to_string_lossy()
    )
}

#[tokio::test]
async fn a_cell_sent_while_an_abandoned_snapshot_still_pickles_stops_it_and_runs_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("pickling");
    let path = dir.path().join("state.pickle");
    let first = kernel().await;
    first.execute(&slow_to_pickle(&marker)).await.unwrap();

    tokio::select! {
        outcome = first.snapshot(&path, None) => panic!("the stalled snapshot ended: {outcome:?}"),
        () = appears(&marker) => {}
    }
    let started = std::time::Instant::now();
    let output = tokio::time::timeout(Duration::from_secs(10), first.execute("keep + 1"))
        .await
        .expect("the cell does not wait for the snapshot")
        .unwrap();

    assert_eq!(output.result.as_deref(), Some("42"));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(!path.exists());
    first.execute("del slow").await.unwrap();
    let report = first.snapshot(&path, None).await.unwrap();
    assert!(report.saved.contains(&"keep".to_string()), "{report:?}");
    first.shutdown().await.unwrap();
    let fresh = kernel().await;
    fresh.restore(&path).await.unwrap();
    assert_eq!(
        fresh.execute("keep").await.unwrap().result.as_deref(),
        Some("41")
    );
    fresh.shutdown().await.unwrap();
}

#[tokio::test]
async fn restoring_a_missing_snapshot_is_not_an_error() {
    let kernel = kernel().await;

    let report = kernel
        .restore(std::path::Path::new("/nope/never.pickle"))
        .await
        .unwrap();

    assert!(report.restored.is_empty());
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_kernel_that_dies_mid_cell_reports_exit() {
    let kernel = kernel().await;

    let error = kernel.execute("import os\nos._exit(7)").await.unwrap_err();

    assert!(
        matches!(error, mush_kernel::KernelError::Exited(_)),
        "{error}"
    );
}

#[tokio::test]
#[ignore = "needs uv and the network"]
async fn a_managed_venv_is_bootstrapped_with_uv_and_hosts_a_kernel() {
    let dir = tempfile::tempdir().unwrap();
    let venv = mush_kernel::ManagedVenv {
        dir: dir.path().join("kernel-venv"),
        python_version: mush_kernel::venv::PYTHON_VERSION.into(),
        packages: vec!["tomli".into()],
        skills: Vec::new(),
    };
    let seen = std::sync::Mutex::new(Vec::new());
    let kernel = Kernel::start_with_progress(
        KernelConfig {
            cwd: std::env::temp_dir(),
            managed_venv: Some(venv.clone()),
            ..KernelConfig::default()
        },
        Arc::new(NoHost),
        &|message| seen.lock().unwrap().push(message.to_string()),
    )
    .await
    .unwrap();

    let output = kernel
        .execute("import tomli, sys\n(tomli.__name__, sys.version_info[:2] == (3, 12))")
        .await
        .unwrap();

    assert_eq!(output.result.as_deref(), Some("('tomli', True)"));
    assert_eq!(kernel.python, venv.python_path());
    assert!(venv.is_current());
    assert_eq!(seen.lock().unwrap().len(), 2, "{:?}", seen.lock().unwrap());
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn attach_image_ships_the_bytes_as_an_attachment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dot.png");
    let png = [
        0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 0, 0, 0, 13, b'I', b'H', b'D', b'R',
    ];
    std::fs::write(&path, png).unwrap();
    let kernel = kernel().await;

    let output = kernel
        .execute(&format!("attach_image({:?})", path.display().to_string()))
        .await
        .unwrap();

    assert_eq!(output.attachments.len(), 1, "{output:?}");
    assert_eq!(output.attachments[0].mime_type, "image/png");
    assert_eq!(
        output.attachments[0].path.as_deref(),
        Some(path.to_str().unwrap())
    );
    assert_eq!(
        output.result.as_deref(),
        Some(format!("'attached {}'", path.display()).as_str())
    );

    let refused = kernel
        .execute(&format!(
            "attach_image({:?})",
            dir.path().join("missing.png").display().to_string()
        ))
        .await
        .unwrap();
    assert_eq!(
        refused.error.as_ref().map(|e| e.ename.as_str()),
        Some("FileNotFoundError")
    );
    kernel.shutdown().await.unwrap();
}

#[expect(
    clippy::unwrap_used,
    reason = "a test helper that needs python3 on PATH"
)]
async fn raw_shim(owner: Option<u32>) -> (tempfile::TempDir, tokio::process::Child) {
    let dir = tempfile::tempdir().unwrap();
    let shim = dir.path().join("mush_kernel.py");
    std::fs::write(&shim, include_str!("../shim/mush_kernel.py")).unwrap();
    let mut command = tokio::process::Command::new("python3");
    command
        .arg(&shim)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(owner) = owner {
        command.env("MUSH_KERNEL_OWNER_PID", owner.to_string());
    }
    let child = command.spawn().unwrap();
    (dir, child)
}

#[tokio::test]
async fn shutdown_fails_a_cell_stuck_on_a_host_request_so_the_kernel_exits_at_once() {
    use tokio::io::AsyncWriteExt;
    let (_dir, mut child) = raw_shim(None).await;
    let mut stdin = child.stdin.take().unwrap();

    stdin
        .write_all(
            b"{\"type\":\"execute\",\"id\":\"c1\",\"code\":\"await host_request('never')\"}\n",
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    stdin.write_all(b"{\"type\":\"shutdown\"}\n").await.unwrap();
    let exited = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;

    assert!(exited.is_ok(), "the kernel waited on the stuck cell");
}

#[tokio::test]
async fn a_kernel_whose_owner_died_exits_even_with_a_busy_cell() {
    use tokio::io::AsyncWriteExt;
    let mut owner = std::process::Command::new("sleep")
        .arg("0.3")
        .spawn()
        .unwrap();
    let (_dir, mut child) = raw_shim(Some(owner.id())).await;
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"{\"type\":\"execute\",\"id\":\"c1\",\"code\":\"while True:\\n    pass\"}\n")
        .await
        .unwrap();
    owner.wait().unwrap();

    let exited = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;

    assert!(exited.is_ok(), "the kernel outlived its owner");
    drop(stdin);
}

#[tokio::test]
async fn python_skills_are_callable_by_import_name_and_a_broken_one_explains_itself() {
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().join("word_count");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("__init__.py"),
        "from collections import Counter\n\nasync def run(text: str, top: int = 5) -> str:\n    \"\"\"Count words.\"\"\"\n    return ' '.join(w for w, _ in Counter(text.split()).most_common(top))\n",
    )
    .unwrap();
    let helper = dir.path().join("plain_helper");
    std::fs::create_dir_all(&helper).unwrap();
    std::fs::write(helper.join("__init__.py"), "VALUE = 3\n").unwrap();
    let kernel = Kernel::start(
        KernelConfig {
            env: vec![(
                "PYTHONPATH".into(),
                dir.path().to_string_lossy().into_owned(),
            )],
            skills: vec![
                "word_count".into(),
                "plain_helper".into(),
                "not_installed_skill".into(),
            ],
            ..KernelConfig::default()
        },
        Arc::new(NoHost),
    )
    .await
    .unwrap();

    let called = kernel
        .execute("(await word_count('a b a', top=1), await word_count.run('b b a', top=1), word_count.__doc__)")
        .await
        .unwrap();
    let plain = kernel.execute("plain_helper.VALUE").await.unwrap();
    let broken = kernel
        .execute("await not_installed_skill('x')")
        .await
        .unwrap();
    let names = kernel
        .execute("import rlm\ncallable(rlm.spawn), rlm.host_request is host_request")
        .await
        .unwrap();

    assert_eq!(called.result.as_deref(), Some("('a', 'b', 'Count words.')"));
    assert_eq!(plain.result.as_deref(), Some("3"));
    let error = broken.error.unwrap();
    assert_eq!(error.ename, "RuntimeError");
    assert!(
        error.evalue.contains("not_installed_skill")
            && error.evalue.contains("ModuleNotFoundError"),
        "{}",
        error.evalue
    );
    assert_eq!(names.result.as_deref(), Some("(True, True)"));
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn child_summaries_answer_to_session_name_and_active_session_id() {
    let kernel = kernel().await;

    let output = kernel
        .execute("c = ChildSummary({'name': 'auth', 'session_id': 's1'})\n(c.session_name, c.active_session_id)")
        .await
        .unwrap();

    assert_eq!(output.result.as_deref(), Some("('auth', 's1')"));
    kernel.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_child_waiting_for_the_network_says_so() {
    let kernel = kernel().await;

    let output = kernel
        .execute(
            "waiting = ChildSummary({'name': 'scout', 'status': 'running', 'state': 'working', 'turns': 2, \
             'waiting_for_network': {'since': 1792324474269, 'error': 'dns error'}})\n\
             online = ChildSummary({'name': 'scout', 'status': 'running', 'state': 'working', 'turns': 2})\n\
             (repr(waiting), repr(online), waiting.settled)",
        )
        .await
        .unwrap();

    assert_eq!(
        output.result.as_deref(),
        Some(
            "(\"ChildSummary(name='scout', status='running', state='working', turns=2, waiting for the network)\", \
             \"ChildSummary(name='scout', status='running', state='working', turns=2)\", False)"
        )
    );
    kernel.shutdown().await.unwrap();
}
