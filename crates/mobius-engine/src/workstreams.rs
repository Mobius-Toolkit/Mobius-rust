use std::collections::VecDeque;
use std::error::Error;

use mobius_domain::{Live, Workstream, organization};
use mobius_github::{Issue, IssueEvent, Repository};
use mobius_store::Task;
use time::OffsetDateTime;

use crate::config::Config;
use crate::labels::{
    AUTOPILOT_LABEL, NEEDS_HUMAN_LABEL, READY_LABEL, WORKING_LABEL, WORKSTREAM_LABEL,
};
use crate::{Engine, dispatch, ends, github, lead_events};

const CLOSED_TEXT: &str = "Workstream closed";

pub async fn list(engine: &Engine) -> Result<Vec<Workstream>, Box<dyn Error + Send + Sync>> {
    let repositories = engine.repositories.read().unwrap().clone();
    let mut workstreams = Vec::new();
    for repository in repositories {
        for issue in repository.open_issues_with_label(WORKSTREAM_LABEL).await? {
            workstreams.push(Workstream {
                repository: repository.full_name.clone(),
                number: issue.number,
                autopilot: issue_autopilot(engine, &repository, &issue).await?,
                all_tasks_closed: all_tasks_closed(&repository, issue.number)
                    .await
                    .unwrap_or(false),
                title: issue.title,
                body: issue.body.unwrap_or_default(),
            });
        }
    }
    Ok(workstreams)
}

pub fn organizations(engine: &Engine) -> Vec<String> {
    let mut organizations: Vec<String> = engine
        .repositories
        .read()
        .unwrap()
        .iter()
        .filter_map(|repository| repository.full_name.split_once('/'))
        .map(|(organization, _)| organization.to_string())
        .collect();
    organizations.sort();
    organizations.dedup();
    organizations
}

// An issue that lost `mobius:workstream` can still have a Lead or live tasks.
pub(crate) async fn has_work(
    engine: &Engine,
    repository: &str,
    workstream: i64,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    if engine.chats.lock().unwrap().contains_key(&(
        organization(repository).to_string(),
        repository.to_string(),
        workstream,
    )) {
        return Ok(true);
    }
    Ok(engine
        .store
        .tasks()
        .live_in(repository)
        .await?
        .iter()
        .any(|task| task.workstream == workstream))
}

async fn all_tasks_closed(
    repository: &Repository,
    workstream: i64,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let sub_issues = repository.sub_issues(workstream).await?;
    Ok(!sub_issues.is_empty() && sub_issues.iter().all(|issue| issue.state == "closed"))
}

// The poll of the close event runs `close` again, and the second run changes nothing.
pub async fn complete(
    engine: &Engine,
    repository: &str,
    workstream: i64,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let repository = engine.repository(repository)?;
    let open_workstream = repository
        .issue(workstream)
        .await?
        .is_some_and(|issue| issue.state == "open" && issue.has_label(WORKSTREAM_LABEL));
    if !open_workstream {
        return Err("The issue is not an open Workstream.".into());
    }
    if !all_tasks_closed(&repository, workstream).await? {
        return Err("The Workstream has no task, or a task is open.".into());
    }
    repository.close_as_completed(workstream).await?;
    engine.broadcast(Live::Workstreams);
    close(engine, &repository, workstream).await
}

// The branches and the Lead directory stay.
pub(crate) async fn close(
    engine: &Engine,
    repository: &Repository,
    workstream: i64,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let name = &repository.full_name;
    stop(engine, repository, workstream).await?;
    // A later reopen needs a new `mobius:autopilot` from a trusted user.
    repository.remove_label(workstream, AUTOPILOT_LABEL).await?;
    for number in engine.store.tasks().pull_requests(name, workstream).await? {
        if repository.pull_request(number).await?.state == "open" {
            repository.add_comment(number, CLOSED_TEXT).await?;
            repository.close_pull_request(number).await?;
        }
    }
    let mut parents = VecDeque::from([workstream]);
    while let Some(parent) = parents.pop_front() {
        for issue in repository.sub_issues(parent).await? {
            if issue.has_label(WORKSTREAM_LABEL) || ends::in_other_repository(&issue, name) {
                continue;
            }
            parents.push_back(issue.number);
            for label in [READY_LABEL, WORKING_LABEL, NEEDS_HUMAN_LABEL] {
                if issue.has_label(label) {
                    repository.remove_label(issue.number, label).await?;
                }
            }
            if issue.state == "open" {
                repository.add_comment(issue.number, CLOSED_TEXT).await?;
                repository.close_as_not_planned(issue.number).await?;
            }
        }
    }
    Ok(())
}

// The queued events of the Lead go to no later Lead.
pub(crate) async fn stop(
    engine: &Engine,
    repository: &Repository,
    workstream: i64,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let name = &repository.full_name;
    let _ = engine.lead_stops.send((name.clone(), workstream));
    engine
        .store
        .lead_events()
        .deliver_all(name, workstream)
        .await?;
    let tasks: Vec<Task> = engine
        .store
        .tasks()
        .live_in(name)
        .await?
        .into_iter()
        .filter(|task| task.workstream == workstream)
        .collect();
    for task in &tasks {
        ends::end(engine, repository, task).await?;
    }
    Ok(())
}

pub(crate) async fn reopen(
    engine: &Engine,
    repository: &Repository,
    issue: &Issue,
    actor: &str,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let text = dispatch::event_text(
        OffsetDateTime::now_utc(),
        "reopen of Workstream",
        issue,
        actor,
        issue.body.as_deref().unwrap_or_default(),
    )?;
    lead_events::add(
        engine,
        &repository.full_name,
        issue.number,
        None,
        "reopen",
        &text,
    )
    .await
}

pub(crate) async fn workstream_of(
    repository: &Repository,
    number: i64,
) -> Result<Option<i64>, Box<dyn Error + Send + Sync>> {
    let mut number = number;
    while let Some(parent) = repository.parent(number).await? {
        if parent.has_label(WORKSTREAM_LABEL) {
            return Ok(Some(parent.number));
        }
        number = parent.number;
    }
    Ok(None)
}

pub(crate) async fn autopilot(
    engine: &Engine,
    repository: &Repository,
    workstream: i64,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    match repository.issue(workstream).await? {
        Some(issue) => issue_autopilot(engine, repository, &issue).await,
        None => Ok(false),
    }
}

// The write uses the token of the Owner, because the label counts only when a trusted user added it last.
pub async fn set_autopilot(
    engine: &Engine,
    repository: &str,
    workstream: i64,
    on: bool,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let repository = engine.repository(repository)?;
    let user_token = github::user_token(engine, repository.app_id).await?;
    let as_owner = repository.with_user_token(&user_token)?;
    // GitHub records no `labeled` event for a label the issue already has, so a
    // last `labeled` of an untrusted actor would stay. Remove before the add.
    as_owner.remove_label(workstream, AUTOPILOT_LABEL).await?;
    if on {
        as_owner.add_label(workstream, AUTOPILOT_LABEL).await?;
    }
    engine.broadcast(Live::Workstreams);
    Ok(())
}

pub(crate) async fn issue_autopilot(
    engine: &Engine,
    repository: &Repository,
    issue: &Issue,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    if !issue.has_label(AUTOPILOT_LABEL) {
        return Ok(false);
    }
    let events = repository.issue_events(issue.number).await?;
    Ok(added_by_trusted_user(&engine.config, &events))
}

// The last `labeled` event of `mobius:autopilot` decides, so a trusted bot or the Mobius App cannot turn Autopilot on.
pub(crate) fn added_by_trusted_user(config: &Config, events: &[IssueEvent]) -> bool {
    events
        .iter()
        .rev()
        .find(|event| {
            event.event == "labeled"
                && event
                    .label
                    .as_ref()
                    .is_some_and(|label| label.name == AUTOPILOT_LABEL)
        })
        .and_then(|event| event.actor.as_ref())
        .is_some_and(|actor| {
            config
                .trusted_users
                .iter()
                .any(|user| user.eq_ignore_ascii_case(&actor.login))
        })
}

#[cfg(test)]
mod tests {
    use mobius_github::{Label, User};
    use time::OffsetDateTime;

    use super::*;

    fn config() -> Config {
        crate::config::parse(
            r#"
access_password = "correct horse"
trusted_users = ["owner"]
trusted_bots = ["coderabbitai[bot]"]

[roles]
lead        = { harness = "claude-code", model = "opus",    effort = "high" }
triager     = { harness = "claude-code", model = "sonnet",  effort = "medium" }
implementer = { harness = "devin",       model = "swe-1.5", effort = "high" }
researcher  = { harness = "antigravity", model = "gemini-3-pro" }
reviewer    = { harness = "claude-code", model = "opus",    effort = "high" }
judge       = { harness = "claude-code", model = "haiku",   effort = "low" }
"#,
        )
        .unwrap()
    }

    fn event(event: &str, label: &str, actor: &str) -> IssueEvent {
        IssueEvent {
            event: event.to_string(),
            actor: Some(User {
                login: actor.to_string(),
            }),
            label: Some(Label {
                name: label.to_string(),
            }),
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn autopilot_of_a_trusted_user_counts() {
        assert!(added_by_trusted_user(
            &config(),
            &[
                event("labeled", "mobius:autopilot", "Owner"),
                event("labeled", "bug", "mallory")
            ]
        ));
    }

    #[test]
    fn autopilot_of_a_bot_the_mobius_app_or_a_stranger_does_not_count() {
        for actor in ["coderabbitai[bot]", "mobius-app[bot]", "mallory"] {
            assert!(!added_by_trusted_user(
                &config(),
                &[
                    event("labeled", "mobius:autopilot", "owner"),
                    event("unlabeled", "mobius:autopilot", actor),
                    event("labeled", "mobius:autopilot", actor)
                ]
            ));
        }
    }
}
