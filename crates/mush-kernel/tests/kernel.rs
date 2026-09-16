use std::sync::Arc;
use std::time::Duration;

use mush_kernel::{HostError, HostRequestHandler, Kernel, KernelConfig, NoHost};
use serde_json::{Value, json};

async fn kernel() -> Kernel {
    kernel_with(Arc::new(NoHost)).await
}

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
async fn raw_fd_writes_from_subprocesses_are_captured_not_mixed_into_the_protocol() {
    let kernel = kernel().await;

    let output = kernel
        .execute("import subprocess\nsubprocess.run(['echo', 'raw'])\nprint('tagged')")
        .await
        .unwrap();

    assert!(output.stdout.contains("raw\n"), "{:?}", output.stdout);
    assert!(output.stdout.contains("tagged\n"), "{:?}", output.stdout);
    assert!(output.ok);
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
async fn dropping_an_execute_future_interrupts_the_cell() {
    let kernel = Arc::new(kernel().await);

    let runner = {
        let kernel = kernel.clone();
        tokio::spawn(async move {
            let _ = kernel.execute("n = 0\nwhile True:\n    n += 1").await;
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    runner.abort();
    let _ = runner.await;

    let after = tokio::time::timeout(Duration::from_secs(5), kernel.execute("n > 0"))
        .await
        .expect("the abandoned cell was interrupted")
        .unwrap();
    assert_eq!(after.result.as_deref(), Some("True"));
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
        .execute("keep = {'a': [1, 2]}\nimport os\nunpicklable = lambda: 1")
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
