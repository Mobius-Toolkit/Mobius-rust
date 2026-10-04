use std::collections::BTreeMap;
use std::error::Error;

use mobius_github::{Issue, Repository};
use mobius_store::Task;
use time::OffsetDateTime;
use tokio::sync::broadcast::Receiver;

use crate::labels::{NEEDS_HUMAN_LABEL, WORKING_LABEL};
use crate::trust::app_login;
use crate::workers::Work;
use crate::{Engine, TIME_FORMAT, activity, checks, conflicts, implementer, judge, lead_events};

// Adds the pull request of each task with work for an agent to `work`. A task that fails to check keeps its old entry.
pub(crate) async fn check(
    engine: &Engine,
    app_slug: &str,
    repository: &Repository,
    work: &mut BTreeMap<i64, Work>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    for task in engine.store.tasks().live_in(&repository.full_name).await? {
        match check_task(engine, app_slug, repository, &task).await {
            Ok(found) => work.extend(found.map(|found| (task.id, found))),
            Err(error) => {
                eprintln!(
                    "mobius: task of {}#{}: {error}",
                    repository.full_name, task.issue
                );
                work.extend(
                    engine
                        .workers
                        .work()
                        .remove(&task.id)
                        .map(|old| (task.id, old)),
                );
            }
        }
    }
    Ok(())
}

async fn check_task(
    engine: &Engine,
    app_slug: &str,
    repository: &Repository,
    task: &Task,
) -> Result<Option<Work>, Box<dyn Error + Send + Sync>> {
    let name = &repository.full_name;
    let Some(issue) = repository
        .issue(task.issue)
        .await?
        .filter(|issue| !in_other_repository(issue, name))
    else {
        end(engine, repository, task).await?;
        return Ok(None);
    };
    let pull_request = match task.pull_request {
        Some(number) => Some(repository.pull_request(number).await?),
        None => None,
    };
    if let Some(pull_request) = &pull_request
        && pull_request.state == "closed"
    {
        if pull_request.merged && issue.state == "open" {
            repository.close_as_completed(task.issue).await?;
        }
        end(engine, repository, task).await?;
        let what = if pull_request.merged {
            "merged"
        } else {
            "closed with no merge"
        };
        let text = format!(
            "{} end of #{} \"{}\": pull request #{} {what}.",
            OffsetDateTime::now_utc().format(TIME_FORMAT)?,
            task.issue,
            issue.title,
            pull_request.number
        );
        lead_events::add(
            engine,
            name,
            task.workstream,
            Some(task.issue),
            "end",
            &text,
        )
        .await?;
        return Ok(None);
    }
    if pull_request.is_none() && issue.state == "closed" {
        end(engine, repository, task).await?;
        return Ok(None);
    }
    if !matches!(task.state.as_str(), "stopped" | "needs_human")
        && !(task.worker.as_deref() == Some(judge::ROLE)
            && task.worker_input.as_deref() == Some("needs_human"))
        && !issue.has_label(WORKING_LABEL)
    {
        let events = repository.issue_events(task.issue).await?;
        if let Some(actor) = events
            .iter()
            .rev()
            .find(|event| {
                event.event == "unlabeled"
                    && event
                        .label
                        .as_ref()
                        .is_some_and(|label| label.name == WORKING_LABEL)
            })
            .and_then(|event| event.actor.as_ref())
            .filter(|actor| !actor.login.eq_ignore_ascii_case(&app_login(app_slug)))
        {
            stop(engine, repository, task, &issue, &actor.login).await?;
        }
        return Ok(None);
    }
    let Some(pull_request) = pull_request else {
        return Ok(None);
    };
    // The work is the round that the task queues or runs now, or the round that this check starts.
    let work = Work {
        repository: name.clone(),
        pull_request: pull_request.number,
        created_at: pull_request.created_at,
    };
    if matches!(task.state.as_str(), "queued" | "working") {
        let round = matches!(
            task.worker.as_deref(),
            Some(implementer::ROLE | implementer::CONFLICT_ROUND | judge::ROLE)
        );
        return Ok(round.then_some(work));
    }
    if !matches!(
        task.state.as_str(),
        "ready_for_review" | "reviewed" | "needs_human"
    ) {
        return Ok(None);
    }
    if task.state == "ready_for_review"
        && (pull_request.mergeable == Some(false) || conflicts::behind(&pull_request))
    {
        let round = conflicts::on_conflict(engine, repository, task, pull_request).await?;
        return Ok(round.then_some(work));
    }
    if task.state == "ready_for_review"
        && checks::on_failure(engine, repository, task, &pull_request).await?
    {
        return Ok(Some(work));
    }
    let waiting = task.state == "reviewed"
        && (pull_request.mergeable == Some(false)
            || conflicts::behind(&pull_request)
            || checks::unhandled_failure(engine, repository, task, &pull_request).await?);
    let judged = judge::check(engine, repository, task, pull_request, waiting).await?;
    Ok(judged.then_some(work))
}

pub(crate) async fn lost_access(
    engine: &Engine,
    repositories: &[Repository],
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let tasks = engine.store.tasks();
    for name in tasks.live_repositories().await? {
        if repositories
            .iter()
            .any(|repository| repository.full_name == name)
        {
            continue;
        }
        for task in tasks.live_in(&name).await? {
            tasks.end(task.id).await?;
            stop_workers(engine, &name, &task).await?;
        }
    }
    Ok(())
}

// `Engine` holds the sender, so the channel stays open while a Worker waits.
pub(crate) async fn stopped(stops: &mut Receiver<i64>, task: i64) {
    loop {
        if stops.recv().await.is_ok_and(|id| id == task) {
            return;
        }
    }
}

// The branch stays.
pub(crate) async fn end(
    engine: &Engine,
    repository: &Repository,
    task: &Task,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    engine.store.tasks().end(task.id).await?;
    stop_workers(engine, &repository.full_name, task).await?;
    repository.remove_label(task.issue, WORKING_LABEL).await?;
    Ok(repository
        .remove_label(task.issue, NEEDS_HUMAN_LABEL)
        .await?)
}

// The pull request and the branch stay.
async fn stop(
    engine: &Engine,
    repository: &Repository,
    task: &Task,
    issue: &Issue,
    actor: &str,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let name = &repository.full_name;
    if !engine.store.tasks().stop(task.id).await? {
        return Ok(());
    }
    stop_workers(engine, name, task).await?;
    repository
        .remove_label(task.issue, NEEDS_HUMAN_LABEL)
        .await?;
    if let Some(number) = task.pull_request {
        let pull_request = repository.pull_request(number).await?;
        repository
            .create_failed_check_run(
                implementer::CHECK_RUN,
                &pull_request.head.sha,
                "Stopped",
                "Stopped by a label removal.",
            )
            .await?;
    }
    activity::add(
        engine,
        name,
        task.workstream,
        task.issue,
        actor,
        &format!(
            "Stopped \"{}\" after a removal of {WORKING_LABEL}",
            issue.title
        ),
        &issue.html_url,
    )
    .await
}

async fn stop_workers(
    engine: &Engine,
    name: &str,
    task: &Task,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let _ = engine.stops.send(task.id);
    engine.workers.changed.notify_waiters();
    let data_dir = &engine.config.data_dir;
    let worktree = mobius_runner::task_dir(data_dir, name, task.issue);
    let _git = engine.git.lock().await;
    if worktree.exists() {
        mobius_runner::remove_worktree(data_dir, name, &worktree).await?;
    }
    Ok(())
}

pub(crate) fn in_other_repository(issue: &Issue, repository: &str) -> bool {
    !issue
        .repository_url
        .to_lowercase()
        .ends_with(&format!("/repos/{}", repository.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(repository_url: &str) -> Issue {
        serde_json::from_value(serde_json::json!({
            "id": 100_041,
            "number": 41,
            "title": "Add plan model",
            "body": null,
            "state": "open",
            "html_url": "https://github.com/owner/shop/issues/41",
            "repository_url": repository_url,
            "updated_at": "2026-09-27T14:02:00Z",
            "labels": [],
            "pull_request": null,
            "user": { "login": "owner" },
            "issue_dependencies_summary": { "blocked_by": 0 }
        }))
        .unwrap()
    }

    #[test]
    fn an_issue_of_the_same_repository_in_each_letter_case_is_not_in_another_repository() {
        assert!(!in_other_repository(
            &issue("https://api.github.com/repos/Owner/Shop"),
            "owner/shop"
        ));
    }

    #[test]
    fn an_issue_of_another_repository_is_in_another_repository() {
        assert!(in_other_repository(
            &issue("https://api.github.com/repos/owner/billing"),
            "owner/shop"
        ));
        assert!(in_other_repository(
            &issue("https://api.github.com/repos/owner/workshop"),
            "owner/shop"
        ));
    }
}
