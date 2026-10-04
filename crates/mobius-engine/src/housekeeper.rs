use std::collections::HashSet;
use std::error::Error;
use std::fs;
use std::time::Duration;

use mobius_domain::InboxKind;
use time::OffsetDateTime;

use crate::{Engine, TIME_FORMAT, implementer, inbox, lead_events};

// In bytes.
const MIN_FREE_DISK: u64 = 20 << 30;

const ERROR_TAIL: usize = 2_000;

pub(crate) fn spawn(engine: Engine) {
    tokio::spawn(async move {
        loop {
            if let Err(error) = clean(&engine).await {
                eprintln!("mobius: Housekeeper: {error}");
            }
            if let Err(error) = free_disk(&engine).await {
                eprintln!("mobius: Housekeeper: {error}");
            }
            tokio::time::sleep(engine.config.housekeeper_interval).await;
        }
    });
}

// The name of a directory tells its owner: `task-<issue>` a live task, `review-<id>`, `judge-<id>`, and `research-<id>` a session, and `scratch/<id>` a session. A Lead directory and a bare clone are not below `worktrees/` or `scratch/`.
async fn clean(engine: &Engine) -> Result<(), Box<dyn Error + Send + Sync>> {
    let data_dir = &engine.config.data_dir;
    let _git = engine.git.lock().await;
    let open: HashSet<String> = engine
        .store
        .sessions()
        .open_ids()
        .await?
        .into_iter()
        .map(|id| id.to_string())
        .collect();
    let mut repositories = HashSet::new();
    for (repository, name) in mobius_runner::worktree_dirs(data_dir)? {
        let owned = match name.split_once('-') {
            Some(("task", issue)) => engine
                .store
                .tasks()
                .live_in(&repository)
                .await?
                .iter()
                .any(|task| task.issue.to_string() == issue),
            Some(("review" | "judge" | "research", id)) => open.contains(id),
            _ => true,
        };
        if !owned {
            fs::remove_dir_all(data_dir.join("worktrees").join(&repository).join(&name))?;
        }
        repositories.insert(repository);
    }
    for repository in repositories {
        mobius_runner::prune(data_dir, &repository).await?;
    }
    for id in mobius_runner::scratch_ids(data_dir)? {
        if !open.contains(&id) {
            fs::remove_dir_all(data_dir.join("scratch").join(&id))?;
        }
    }
    Ok(())
}

// At `MIN_FREE_DISK`, each check that waits for disk space runs again.
async fn free_disk(engine: &Engine) -> Result<(), Box<dyn Error + Send + Sync>> {
    let free = mobius_runner::free_space(&engine.config.data_dir, &engine.harness_path).await?;
    if free < MIN_FREE_DISK {
        return Ok(());
    }
    for item in inbox::list(engine).await? {
        if item.kind == InboxKind::DiskFull {
            inbox::dismiss(engine, item.id).await?;
        }
    }
    engine.disk_freed.notify_waiters();
    Ok(())
}

// All checks on a full disk share one Inbox item.
pub(crate) async fn wait_for_disk(
    engine: &Engine,
    repository: &str,
    workstream: i64,
    number: i64,
    title: &str,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let freed = engine.disk_freed.notified();
    tokio::pin!(freed);
    freed.as_mut().enable();
    {
        let _pausing = engine.pausing.lock().await;
        if !inbox::list(engine)
            .await?
            .iter()
            .any(|item| item.kind == InboxKind::DiskFull)
        {
            let free =
                mobius_runner::free_space(&engine.config.data_dir, &engine.harness_path).await?;
            let issue = engine
                .repository(repository)?
                .issue(number)
                .await?
                .ok_or_else(|| format!("#{number} does not exist."))?;
            inbox::add(
                engine,
                InboxKind::DiskFull,
                repository,
                workstream,
                number,
                &format!(
                    "The disk of the Mobius server is full. The .mobius/check of #{number} \"{title}\" waits for {} GiB of free space. The disk has {} GiB of free space.",
                    MIN_FREE_DISK >> 30,
                    free >> 30
                ),
                &issue.html_url,
            )
            .await?;
        }
    }
    freed.await;
    Ok(())
}

// Waits before it gives `true`, and the Worker holds no slot during the wait. The wait ends not before the reset of the GitHub rate limit. At `max_worker_restarts`, the task goes to a human, and the Lead gets a stop event.
pub(crate) async fn restart(
    engine: &Engine,
    repository: &str,
    workstream: i64,
    task: i64,
    number: i64,
    title: &str,
    error: &str,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let max = engine.config.max_worker_restarts;
    if let Some(restarts) = engine.store.tasks().add_worker_restart(task, max).await? {
        let mut wait = engine.config.restart_wait(restarts);
        if let Some(reset) = engine
            .repository(repository)?
            .rate_limit_reset()
            .await
            .ok()
            .flatten()
        {
            let until_reset = reset - OffsetDateTime::now_utc().unix_timestamp();
            wait = wait.max(Duration::from_secs(until_reset.try_into().unwrap_or(0)));
        }
        tokio::time::sleep(wait).await;
        return Ok(true);
    }
    if !implementer::hand_to_human(engine, repository, task, number).await? {
        return Ok(false);
    }
    let count = error.chars().count();
    let tail: String = error
        .chars()
        .skip(count.saturating_sub(ERROR_TAIL))
        .collect();
    let text = format!(
        "{} stop of #{number} \"{title}\": the Worker failed after {max} restarts. Mobius added mobius:needs-human. The last error ends with these lines:\n\n```\n{tail}\n```",
        OffsetDateTime::now_utc().format(TIME_FORMAT)?
    );
    lead_events::add(engine, repository, workstream, Some(number), "stop", &text).await?;
    Ok(false)
}
