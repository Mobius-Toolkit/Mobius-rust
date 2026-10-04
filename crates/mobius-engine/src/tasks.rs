use std::error::Error;

use mobius_domain::{Blocker, Live, NeedsHuman, TaskLine};
use mobius_github::{Issue, Repository};

use crate::labels::{NEEDS_HUMAN_LABEL, READY_LABEL, WORKSTREAM_LABEL};
use crate::{Engine, ends, github, trust, workstreams};

// Issues below a Workstream issue of their own belong to that Workstream.
// The walk is depth-first, so a nested task follows its parent in the list.
pub async fn list(
    engine: &Engine,
    repository: &str,
    workstream: i64,
) -> Result<Vec<TaskLine>, Box<dyn Error + Send + Sync>> {
    let repository = engine.repository(repository)?;
    let trusted = trust::trusted_authors(engine, &repository);
    let mut lines = Vec::new();
    // Each frame walks the sub-issues of one issue at one display depth.
    let mut frames: Vec<(i64, std::vec::IntoIter<Issue>)> =
        vec![(0, repository.sub_issues(workstream).await?.into_iter())];
    while let Some((depth, issue)) = next_issue(&mut frames) {
        // An issue below a nested Workstream is a task of that Workstream.
        if issue.has_label(WORKSTREAM_LABEL) {
            continue;
        }
        let visible = issue.state == "open" && trusted(&issue.user.login);
        // An issue in another repository keeps its line, but its number names
        // a different issue here, so this repository cannot give its blockers,
        // its task state, or its children.
        let own_repository = !ends::in_other_repository(&issue, &repository.full_name);
        if visible {
            let mut line = task_line(&issue, depth);
            if own_repository && issue.issue_dependencies_summary.blocked_by > 0 {
                line.blocked_by = blockers(&repository, workstream, issue.number).await?;
            }
            if own_repository
                && line.state == "working"
                && engine
                    .store
                    .tasks()
                    .live(&repository.full_name, issue.number)
                    .await?
                    .is_some_and(|task| task.state == "queued")
            {
                line.state = "queued".to_string();
            }
            lines.push(line);
        }
        // The sub-issues of a closed or untrusted issue still belong to the
        // Workstream. They take the depth of their hidden parent, so a nested
        // task does not move below an unrelated sibling.
        if !own_repository {
            continue;
        }
        let children = repository.sub_issues(issue.number).await?.into_iter();
        frames.push((depth + i64::from(visible), children));
    }
    Ok(lines)
}

// The open issues with `mobius:needs-human` of one repository, each with its Workstream.
// The label list needs one request, so it does not walk the sub-issue tree.
async fn needs_human_issues(
    engine: &Engine,
    repository: &Repository,
) -> Result<Vec<(i64, Issue)>, Box<dyn Error + Send + Sync>> {
    let trusted = trust::trusted_authors(engine, repository);
    let mut issues = Vec::new();
    for issue in repository.open_issues_with_label(NEEDS_HUMAN_LABEL).await? {
        if issue.has_label(WORKSTREAM_LABEL) || !trusted(&issue.user.login) {
            continue;
        }
        if let Some(workstream) = workstreams::workstream_of(repository, issue.number).await? {
            issues.push((workstream, issue));
        }
    }
    Ok(issues)
}

pub async fn needs_human(
    engine: &Engine,
    repository: &str,
    workstream: i64,
) -> Result<Vec<NeedsHuman>, Box<dyn Error + Send + Sync>> {
    let repository = engine.repository(repository)?;
    let mut issues = needs_human_issues(engine, &repository).await?;
    issues.retain(|(issue_workstream, _)| *issue_workstream == workstream);
    issues.sort_by_key(|(_, issue)| issue.number);
    let mut result = Vec::new();
    for (_, issue) in issues {
        let pull_request = engine
            .store
            .tasks()
            .live(&repository.full_name, issue.number)
            .await?
            .and_then(|task| task.pull_request);
        let pull_request_url = match pull_request {
            Some(number) => Some(repository.pull_request(number).await?.html_url),
            None => None,
        };
        result.push(NeedsHuman {
            number: issue.number,
            title: issue.title,
            url: issue.html_url,
            pull_request,
            pull_request_url,
        });
    }
    Ok(result)
}

// The Workstreams that have a needs-human issue, as repository and number.
pub async fn needs_human_workstreams(
    engine: &Engine,
) -> Result<Vec<(String, i64)>, Box<dyn Error + Send + Sync>> {
    let repositories = engine.repositories.read().unwrap().clone();
    let mut workstreams = Vec::new();
    for repository in repositories {
        for (workstream, _) in needs_human_issues(engine, &repository).await? {
            workstreams.push((repository.full_name.clone(), workstream));
        }
    }
    workstreams.sort();
    workstreams.dedup();
    Ok(workstreams)
}

// The write uses the token of the Owner, because dispatch trusts `mobius:ready` only from a trusted user.
pub async fn resume(
    engine: &Engine,
    repository: &str,
    issue: i64,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let repository = engine.repository(repository)?;
    let user_token = github::user_token(engine, repository.app_id).await?;
    let as_owner = repository.with_user_token(&user_token)?;
    as_owner.remove_label(issue, NEEDS_HUMAN_LABEL).await?;
    as_owner.add_label(issue, READY_LABEL).await?;
    engine.broadcast(Live::Workstreams);
    Ok(())
}

// Gives the next issue of the deepest frame, dropping each frame that ran out.
pub(crate) fn next_issue(
    frames: &mut Vec<(i64, std::vec::IntoIter<Issue>)>,
) -> Option<(i64, Issue)> {
    loop {
        match frames.last_mut() {
            Some((depth, issues)) => match issues.next() {
                Some(issue) => return Some((*depth, issue)),
                None => {
                    frames.pop();
                }
            },
            None => return None,
        }
    }
}

async fn blockers(
    repository: &Repository,
    workstream: i64,
    number: i64,
) -> Result<Vec<Blocker>, Box<dyn Error + Send + Sync>> {
    let mut blockers = Vec::new();
    for blocker in repository.blocked_by(number).await? {
        if blocker.state != "open" {
            continue;
        }
        if ends::in_other_repository(&blocker, &repository.full_name) {
            continue;
        }
        let other_workstream = match workstreams::workstream_of(repository, blocker.number).await? {
            Some(number) if number != workstream => repository.issue(number).await?,
            _ => None,
        };
        blockers.push(Blocker {
            number: blocker.number,
            workstream_title: other_workstream.map(|issue| issue.title),
        });
    }
    Ok(blockers)
}

fn task_line(issue: &Issue, depth: i64) -> TaskLine {
    let state = issue
        .labels
        .iter()
        .find_map(|label| label.name.strip_prefix("mobius:"))
        .unwrap_or("open");
    TaskLine {
        number: issue.number,
        title: issue.title.clone(),
        state: state.to_string(),
        url: issue.html_url.clone(),
        depth,
        blocked_by: Vec::new(),
    }
}

pub(crate) fn text(lines: &[TaskLine]) -> String {
    lines
        .iter()
        .map(|line| {
            let blockers: Vec<String> = line
                .blocked_by
                .iter()
                .map(|blocker| match &blocker.workstream_title {
                    Some(title) => format!("#{} (Workstream \"{title}\")", blocker.number),
                    None => format!("#{}", blocker.number),
                })
                .collect();
            let blocked_by = if blockers.is_empty() {
                String::new()
            } else {
                format!(", blocked by {}", blockers.join(", "))
            };
            format!(
                "#{} {}: {}{blocked_by}\n",
                line.number, line.title, line.state
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use mobius_github::{DependenciesSummary, Label, User};

    use super::*;

    fn issue(labels: &[&str]) -> Issue {
        Issue {
            id: 100_041,
            number: 41,
            title: "Add plan model".to_string(),
            body: None,
            state: "open".to_string(),
            html_url: "https://github.com/owner/shop/issues/41".to_string(),
            repository_url: "https://api.github.com/repos/owner/shop".to_string(),
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
            labels: labels
                .iter()
                .map(|name| Label {
                    name: name.to_string(),
                })
                .collect(),
            pull_request: None,
            user: User {
                login: "owner".to_string(),
            },
            issue_dependencies_summary: DependenciesSummary { blocked_by: 0 },
        }
    }

    #[test]
    fn a_task_line_shows_the_mobius_label() {
        assert_eq!(
            text(&[task_line(&issue(&["bug", "mobius:working"]), 0)]),
            "#41 Add plan model: working\n"
        );
    }

    #[test]
    fn a_task_line_shows_its_blockers_and_the_workstream_of_a_blocker_in_another_workstream() {
        let mut line = task_line(&issue(&["mobius:ready"]), 0);
        line.blocked_by = vec![
            Blocker {
                number: 40,
                workstream_title: None,
            },
            Blocker {
                number: 88,
                workstream_title: Some("Billing".to_string()),
            },
        ];

        assert_eq!(
            text(&[line]),
            "#41 Add plan model: ready, blocked by #40, #88 (Workstream \"Billing\")\n"
        );
    }

    #[test]
    fn a_task_line_with_no_mobius_label_shows_open() {
        assert_eq!(
            text(&[task_line(&issue(&["bug"]), 0)]),
            "#41 Add plan model: open\n"
        );
    }
}
