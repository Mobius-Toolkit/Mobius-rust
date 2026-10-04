use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;

use mobius_domain::Live;
use mobius_github::{Issue, IssueEvent, IssueLinks, Repository};
use mobius_store::{ChangedIssue, CopiedBlocker, CopiedIssue, CopiedWorkstream};

use crate::labels::{AUTOPILOT_LABEL, WORKSTREAM_LABEL};
use crate::{Engine, ends, tasks, workstreams};

// Gives the links of the open issues that the copy has after the sync.
// The links are read first, so a change during the sync shows in the next `relink`.
pub(crate) async fn sync(
    engine: &Engine,
    repository: &Repository,
) -> Result<BTreeMap<i64, IssueLinks>, Box<dyn Error + Send + Sync>> {
    let links = repository.links().await?;
    let mut workstreams = Vec::new();
    for workstream in repository.open_issues_with_label(WORKSTREAM_LABEL).await? {
        let autopilot = workstreams::issue_autopilot(engine, repository, &workstream).await?;
        workstreams.push(copied_workstream(repository, &workstream, autopilot).await?);
    }
    engine
        .store
        .workstream_copy()
        .replace(&repository.full_name, &workstreams)
        .await?;
    engine.broadcast(Live::Workstreams);
    Ok(links)
}

// Reads again the trees that have a sub-issue link or a blocker link that is different from the links of the last sync or `relink`.
// GitHub can change a link with no change of `updated_at`, so the `since` poll does not see it.
// A new or reopened issue has no row in a tree, but the tree of its parent has a new row.
// A closed issue does not change a tree, because the `since` poll shows it.
// A moved issue and its descendants get another Workstream, which the blocker rows of the other trees store.
pub(crate) async fn relink(
    engine: &Engine,
    repository: &Repository,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let name = &repository.full_name;
    let Some(before) = engine.copied.lock().unwrap().get(name).cloned() else {
        return Ok(());
    };
    let after = repository.links().await?;
    let copy = engine.store.workstream_copy();
    let mut stale = BTreeSet::new();
    for (number, links) in &after {
        let old = before.get(number);
        if old == Some(links) {
            continue;
        }
        if old.is_some() {
            stale.extend(copy.workstreams_holding(name, *number).await?);
        }
        if let Some(parent) = links.parent {
            stale.extend(copy.workstreams_holding(name, parent).await?);
        }
    }
    let mut moved = BTreeSet::new();
    for workstream in &stale {
        moved.extend(copy.issue_numbers(name, *workstream).await?);
        let issues = tree(repository, *workstream).await?;
        moved.extend(issues.iter().map(|issue| issue.number));
        copy.replace_issues(name, *workstream, &issues).await?;
    }
    let mut blocked_trees = BTreeSet::new();
    for number in moved {
        blocked_trees.extend(copy.workstreams_with_blocker(name, number).await?);
    }
    for workstream in blocked_trees.difference(&stale) {
        let issues = tree(repository, *workstream).await?;
        copy.replace_issues(name, *workstream, &issues).await?;
    }
    engine.copied.lock().unwrap().insert(name.clone(), after);
    if !stale.is_empty() {
        engine.broadcast(Live::Workstreams);
    }
    Ok(())
}

// Applies the change of one issue of the repository to the copy, and gives `true` when the copy changes.
// `events` are all events of the issue. The parent is read for an issue that the copy does not have,
// because its new link to a Workstream or a task changes the walk order of that Workstream.
// A task that gets or loses the Workstream label changes the walk order of its Workstream too,
// and the Workstream of its blockers in the other trees.
// A new Workstream changes the Workstream of the blockers that are in its tree, in the other trees.
pub(crate) async fn update(
    engine: &Engine,
    repository: &Repository,
    issue: &Issue,
    events: &[IssueEvent],
    find_parent: bool,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let name = &repository.full_name;
    let copy = engine.store.workstream_copy();
    let known = copy
        .workstream_of(name, issue.number, &issue.repository_url)
        .await?
        .is_some();
    let label_changed = copy
        .workstreams_with_label_change(
            name,
            issue.number,
            &issue.repository_url,
            WORKSTREAM_LABEL,
            issue.has_label(WORKSTREAM_LABEL),
        )
        .await?;
    let stored = copy.has_workstream(name, issue.number).await?;
    let wanted = issue.has_label(WORKSTREAM_LABEL) && issue.state == "open";
    let autopilot = issue.has_label(AUTOPILOT_LABEL)
        && workstreams::added_by_trusted_user(&engine.config, events);
    let body = issue.body.clone().unwrap_or_default();
    let mut changed = false;
    let mut added = Vec::new();
    if stored && !wanted {
        copy.remove_workstream(name, issue.number).await?;
        changed = true;
    } else if stored {
        changed = copy
            .update_workstream(name, issue.number, &issue.title, &body, autopilot)
            .await?;
    } else if wanted {
        let workstream = copied_workstream(repository, issue, autopilot).await?;
        copy.add_workstream(name, &workstream).await?;
        added = workstream.issues.iter().map(|issue| issue.number).collect();
        changed = true;
    }
    let changed_issue = ChangedIssue {
        number: issue.number,
        repository_url: issue.repository_url.clone(),
        title: issue.title.clone(),
        body,
        state: issue.state.clone(),
        labels: issue
            .labels
            .iter()
            .map(|label| label.name.clone())
            .collect(),
        author: issue.user.login.clone(),
    };
    changed |= copy.update_issue(&changed_issue).await?;
    if issue.state != "open" {
        changed |= copy.remove_blocker(name, issue.number).await?;
    }
    let mut stale = label_changed.clone();
    if stored && !wanted {
        stale.push(issue.number);
    }
    let mut rebuilt = label_changed;
    for workstream in stale {
        for blocked in copy.workstreams_with_blocker_in(name, workstream).await? {
            if !rebuilt.contains(&blocked) {
                rebuilt.push(blocked);
            }
        }
    }
    for number in added {
        for blocked in copy.workstreams_with_blocker(name, number).await? {
            if blocked != issue.number && !rebuilt.contains(&blocked) {
                rebuilt.push(blocked);
            }
        }
    }
    for workstream in rebuilt {
        let issues = tree(repository, workstream).await?;
        copy.replace_issues(name, workstream, &issues).await?;
        changed = true;
    }
    if known || !find_parent {
        return Ok(changed);
    }
    if let Some(parent) = repository.parent(issue.number).await?
        && let Some(workstream) = copy
            .workstream_of(name, parent.number, &parent.repository_url)
            .await?
    {
        let issues = tree(repository, workstream).await?;
        copy.replace_issues(name, workstream, &issues).await?;
        changed = true;
    }
    Ok(changed)
}

async fn copied_workstream(
    repository: &Repository,
    workstream: &Issue,
    autopilot: bool,
) -> Result<CopiedWorkstream, Box<dyn Error + Send + Sync>> {
    Ok(CopiedWorkstream {
        number: workstream.number,
        title: workstream.title.clone(),
        body: workstream.body.clone().unwrap_or_default(),
        autopilot,
        issues: tree(repository, workstream.number).await?,
    })
}

// The walk follows the rules of `tasks::list`, but it keeps each issue.
// A nested Workstream and an issue of another repository are leaves.
async fn tree(
    repository: &Repository,
    workstream: i64,
) -> Result<Vec<CopiedIssue>, Box<dyn Error + Send + Sync>> {
    let mut issues = Vec::new();
    let mut frames = vec![(
        workstream,
        repository.sub_issues(workstream).await?.into_iter(),
    )];
    while let Some((parent, issue)) = tasks::next_issue(&mut frames) {
        let leaf = issue.has_label(WORKSTREAM_LABEL)
            || ends::in_other_repository(&issue, &repository.full_name);
        let mut blockers = Vec::new();
        if !leaf {
            if issue.issue_dependencies_summary.blocked_by > 0 {
                blockers = blockers_of(repository, issue.number).await?;
            }
            frames.push((
                issue.number,
                repository.sub_issues(issue.number).await?.into_iter(),
            ));
        }
        issues.push(copied_issue(issue, parent, blockers));
    }
    Ok(issues)
}

async fn blockers_of(
    repository: &Repository,
    number: i64,
) -> Result<Vec<CopiedBlocker>, Box<dyn Error + Send + Sync>> {
    let mut blockers = Vec::new();
    for blocker in repository.blocked_by(number).await? {
        if blocker.state != "open" || ends::in_other_repository(&blocker, &repository.full_name) {
            continue;
        }
        let workstream = workstreams::workstream_of(repository, blocker.number).await?;
        let workstream_title = match workstream {
            Some(workstream) => repository.issue(workstream).await?.map(|issue| issue.title),
            None => None,
        };
        blockers.push(CopiedBlocker {
            number: blocker.number,
            workstream,
            workstream_title,
        });
    }
    Ok(blockers)
}

fn copied_issue(issue: Issue, parent: i64, blockers: Vec<CopiedBlocker>) -> CopiedIssue {
    CopiedIssue {
        number: issue.number,
        parent,
        title: issue.title,
        body: issue.body.unwrap_or_default(),
        state: issue.state,
        labels: issue.labels.into_iter().map(|label| label.name).collect(),
        author: issue.user.login,
        html_url: issue.html_url,
        repository_url: issue.repository_url,
        blockers,
    }
}
