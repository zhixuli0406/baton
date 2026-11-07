//! Cross-process CLI slots. Never unlink lock files: every waiter must lock
//! the same inode. Dropping the held file (including process death) frees it.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::agents::{Agent, AgentError, ErrorKind};
use crate::paths::{self, FileLock};

const RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// `dir` already exists. The caller retains the returned lock for the run.
pub async fn acquire(dir: &Path, agent: Agent, limit: usize, started: Instant, timeout: Duration) -> Result<Option<FileLock>, AgentError> {
    if limit == 0 {
        return Ok(None);
    }
    #[cfg(not(unix))]
    return Err(AgentError("concurrency slots require Unix flock support".into(), ErrorKind::Failed));

    #[cfg(unix)]
    loop {
        for slot in 0..limit {
            if started.elapsed() >= timeout {
                return Err(AgentError(
                    format!("{} timed out after waiting {:.2}s for a concurrency slot; no CLI was started, no tokens spent", agent.name(), started.elapsed().as_secs_f64()),
                    ErrorKind::Timeout,
                ));
            }
            let path = dir.join(format!("{}-{slot}.lock", agent.name()));
            if let Some(lock) = paths::try_lock(&path).map_err(|e| {
                AgentError(format!("cannot acquire concurrency slot {}: {e}", path.display()), ErrorKind::Failed)
            })? {
                return Ok(Some(lock));
            }
        }
        tokio::time::sleep(RETRY_INTERVAL.min(timeout.saturating_sub(started.elapsed()))).await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn slot_lock_child() {
        let Some(dir) = std::env::var_os("BATON_SLOT_TEST_DIR") else { return };
        let dir = std::path::PathBuf::from(dir);
        let _held = paths::lock(&dir.join("codex-0.lock")).unwrap();
        std::fs::write(dir.join("ready"), "locked").unwrap();
        std::thread::sleep(Duration::from_secs(30));
    }

    #[tokio::test]
    async fn slot_is_exclusive_across_processes_and_released_on_death() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .env_remove("TYPESAFE_API_KEY")
            .args(["--exact", "slots::tests::slot_lock_child"])
            .env("BATON_SLOT_TEST_DIR", dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn().unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !dir.path().join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        let err = acquire(dir.path(), Agent::Codex, 1, Instant::now(), Duration::from_millis(80)).await.err().unwrap();
        assert_eq!(err.1, ErrorKind::Timeout);
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        assert!(acquire(dir.path(), Agent::Codex, 1, Instant::now(), Duration::from_secs(5)).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn slot_waits_times_out_and_succeeds_after_release() {
        let dir = tempfile::tempdir().unwrap();
        let budget = Duration::from_secs(5);
        let held = acquire(dir.path(), Agent::Codex, 1, Instant::now(), budget).await.unwrap();
        let started = Instant::now();
        let err = acquire(dir.path(), Agent::Codex, 1, started, Duration::from_millis(80)).await.err().unwrap();
        assert_eq!(err.1, ErrorKind::Timeout);
        assert!(err.0.contains("concurrency slot") && err.0.contains("no CLI was started"));
        assert!(started.elapsed() >= Duration::from_millis(80));
        // Separate agents and unlimited calls are not blocked by codex-0.
        assert!(acquire(dir.path(), Agent::Grok, 1, Instant::now(), budget).await.unwrap().is_some());
        assert!(acquire(dir.path(), Agent::Codex, 0, Instant::now(), budget).await.unwrap().is_none());
        let waiting = acquire(dir.path(), Agent::Codex, 1, Instant::now(), budget);
        tokio::pin!(waiting);
        assert!(tokio::time::timeout(Duration::from_millis(80), &mut waiting).await.is_err());
        drop(held);
        assert!(waiting.await.unwrap().is_some());
    }
}
