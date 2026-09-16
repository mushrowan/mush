use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

static QUEUES: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub async fn with_file_mutation_queue<T, F>(path: &Path, run: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let key = key(path);
    let lock = QUEUES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(key.clone())
        .or_default()
        .clone();
    let guard = lock.lock().await;
    let result = run.await;
    drop(guard);
    let mut queues = QUEUES.lock().unwrap_or_else(PoisonError::into_inner);
    if queues
        .get(&key)
        .is_some_and(|current| Arc::strong_count(current) == 2)
    {
        queues.remove(&key);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn writes_to_one_file_do_not_overlap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "").unwrap();
        let in_flight = Arc::new(AtomicUsize::new(0));
        let overlapped = Arc::new(AtomicUsize::new(0));

        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                let in_flight = in_flight.clone();
                let overlapped = overlapped.clone();
                tokio::spawn(async move {
                    with_file_mutation_queue(&path, async {
                        if in_flight.fetch_add(1, Ordering::SeqCst) > 0 {
                            overlapped.fetch_add(1, Ordering::SeqCst);
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                    })
                    .await;
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }

        assert_eq!(overlapped.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn different_files_run_side_by_side() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();

        let holder = tokio::spawn({
            let a = a.clone();
            async move {
                with_file_mutation_queue(&a, async {
                    let _ = rx.await;
                })
                .await;
            }
        });
        tokio::time::timeout(
            Duration::from_secs(1),
            with_file_mutation_queue(&b, async {}),
        )
        .await
        .expect("b was not blocked by a");

        let _ = tx.send(());
        holder.await.unwrap();
    }

    #[tokio::test]
    async fn the_queue_is_dropped_once_idle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "").unwrap();

        with_file_mutation_queue(&path, async {}).await;

        assert!(
            !QUEUES
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains_key(&key(&path))
        );
    }
}
