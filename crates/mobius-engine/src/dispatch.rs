use std::error::Error;

use mobius_domain::InboxKind;
use mobius_github::{Comment, Issue, IssueEvent, PullRequest, Repository};
use mobius_store::Task;
use time::OffsetDateTime;

use crate::config::Config;
use crate::labels::{NEEDS_HUMAN_LABEL, NO_WORKSTREAM_LABEL, READY_LABEL, WORKING_LABEL};
use crate::trust::{self, app_login, trusted_author};
use crate::{
    Engine, TIME_FORMAT, activity, conflicts, implementer, inbox, issues, lead, lead_events,
    reviewer, triager, workstreams,
};

pub(crate) const READY_CURSOR: &str = "ready";

// An issue with no Workstream in the parent chain goes to the Triager, also when it has open blockers.
pub(crate) async fn dispatch_ready(
    engine: &Engine,
    app_slug: &str,
    repository: &Repository,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    // The drain holds each new dispatch and Triager, and it does not read the ready list: the first poll after a cancel gets the same page again.
    if engine.drain.on() {
        return Ok(());
    }
    let name = &repository.full_name;
    let cursor = engine.store.sync_cursors().get(name, READY_CURSOR).await?;
    let Some(page) = repository
        .labeled_issues(READY_LABEL, cursor.etag.as_deref())
        .await?
    else {
        return Ok(());
    };
    let mut held = false;
    for issue in &page.issues {
        if issue.pull_request.is_some() {
            continue;
        }
        let events = repository.issue_events(issue.number).await?;
        let Some(actor) = ready_actor(&events, &app_login(app_slug)) else {
            continue;
        };
        if !trusted_author(&engine.config, app_slug, actor) {
            continue;
        }
        let live = engine.store.tasks().live(name, issue.number).await?;
        if let Some(task) = live.as_ref().filter(|task| task.state == "needs_human") {
            if actor.eq_ignore_ascii_case(&app_login(app_slug))
                && !workstreams::autopilot(engine, repository, task.workstream).await?
            {
                continue;
            }
            resume(engine, repository, issue, task, actor).await?;
            continue;
        }
        if let Some(task) = live.as_ref().filter(|task| task.state != "stopped") {
            repository.remove_label(issue.number, READY_LABEL).await?;
            activity::add(
                engine,
                name,
                task.workstream,
                issue.number,
                actor,
                &format!("No effect: \"{}\" has a live task", issue.title),
                &issue.html_url,
            )
            .await?;
            continue;
        }
        let Some(workstream) = workstreams::workstream_of(repository, issue.number).await? else {
            held |= !triager::triage(engine, repository, issue.number).await?;
            continue;
        };
        if issue.issue_dependencies_summary.blocked_by > 0 {
            continue;
        }
        // A `mobius:ready` of the Mobius App needs Autopilot.
        if actor.eq_ignore_ascii_case(&app_login(app_slug))
            && !workstreams::autopilot(engine, repository, workstream).await?
        {
            continue;
        }
        if let Some(task) = live {
            engine.store.tasks().end(task.id).await?;
        }
        dispatch(engine, repository, issue, workstream, actor).await?;
    }
    // A drain that starts during the loop holds a Triager. The saved ETag would hide the issue from the next poll.
    if held {
        return Ok(());
    }
    engine
        .store
        .sync_cursors()
        .set(name, READY_CURSOR, None, page.etag.as_deref())
        .await
}

// `mobius:ready` goes off last, so a failure before the round starts leaves the issue in the ready list.
async fn resume(
    engine: &Engine,
    repository: &Repository,
    issue: &Issue,
    task: &Task,
    actor: &str,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let name = &repository.full_name;
    let tasks = engine.store.tasks();
    repository.add_label(issue.number, WORKING_LABEL).await?;
    repository
        .remove_label(issue.number, NEEDS_HUMAN_LABEL)
        .await?;
    tasks.reset_counters(task.id).await?;
    let pull_request = match task.pull_request {
        Some(number) => Some(repository.pull_request(number).await?),
        None => None,
    };
    let conflict = pull_request.as_ref().is_some_and(|pull_request| {
        pull_request.mergeable == Some(false) || conflicts::behind(pull_request)
    });
    let to = match (&pull_request, conflict) {
        (Some(_), true) => "ready_for_review",
        (Some(_), false) => "working",
        (None, _) => "dispatched",
    };
    let parent = lead::newest_session(engine, name, task.workstream, issue.number).await?;
    let items = match &pull_request {
        Some(pull_request) if !conflict => continue_items(engine, repository, pull_request).await?,
        _ => String::new(),
    };
    if !tasks.set_state(task.id, "needs_human", to).await? {
        return Ok(());
    }
    let started = match pull_request {
        Some(pull_request) if conflict => {
            implementer::conflict_round(engine, repository, task, pull_request).await
        }
        Some(pull_request) => {
            implementer::fix_round(
                engine,
                repository,
                implementer::Round {
                    repository: name.clone(),
                    workstream: task.workstream,
                    task: task.id,
                    number: issue.number,
                    title: issue.title.clone(),
                    branch: task.branch.clone().unwrap_or_default(),
                    pull_request,
                    check_run: None,
                    counts: true,
                    items,
                    parent,
                },
            )
            .await
        }
        None => implementer::start(
            engine,
            repository,
            task.workstream,
            issue.number,
            issue.body.as_deref().unwrap_or_default(),
            parent,
        )
        .await
        .map(|_| ()),
    };
    if let Err(error) = started {
        tasks.set_state(task.id, to, "needs_human").await?;
        return Err(error);
    }
    repository.remove_label(issue.number, READY_LABEL).await?;
    activity::add(
        engine,
        name,
        task.workstream,
        issue.number,
        actor,
        &format!("Continued \"{}\"", issue.title),
        &issue.html_url,
    )
    .await
}

async fn continue_items(
    engine: &Engine,
    repository: &Repository,
    pull_request: &PullRequest,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    let trusted = trust::trusted_authors(engine, repository);
    let app_login = app_login(&repository.app_slug);
    let open: Vec<i64> = repository
        .review_threads(pull_request.number)
        .await?
        .iter()
        .filter(|thread| reviewer::is_open(thread, &trusted, &app_login))
        .map(|thread| thread.comment)
        .collect();
    if open.is_empty() {
        return Ok(
            "\nThe human continued the task. Finish the issue and make `.mobius/check` pass.\n"
                .to_string(),
        );
    }
    issues::fix_threads(repository, pull_request.number, &open, &trusted).await
}

// `mobius:working` goes on before `mobius:ready` goes off, so a failure between the two leaves the issue in the ready list.
pub(crate) async fn dispatch(
    engine: &Engine,
    repository: &Repository,
    issue: &Issue,
    workstream: i64,
    actor: &str,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let name = &repository.full_name;
    repository.add_label(issue.number, WORKING_LABEL).await?;
    repository.remove_label(issue.number, READY_LABEL).await?;
    engine
        .store
        .tasks()
        .add(name, issue.number, workstream)
        .await?;
    activity::add(
        engine,
        name,
        workstream,
        issue.number,
        actor,
        &format!("Dispatched \"{}\"", issue.title),
        &issue.html_url,
    )
    .await?;
    let text = event_text(
        OffsetDateTime::now_utc(),
        "dispatch of",
        issue,
        actor,
        issue.body.as_deref().unwrap_or_default(),
    )?;
    lead_events::add(
        engine,
        name,
        workstream,
        Some(issue.number),
        "dispatch",
        &text,
    )
    .await
}

pub(crate) async fn comment_events(
    engine: &Engine,
    app_slug: &str,
    repository: &Repository,
    issue: &Issue,
    since: Option<OffsetDateTime>,
    until: Option<OffsetDateTime>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let name = &repository.full_name;
    let Some(task) = engine.store.tasks().live(name, issue.number).await? else {
        return Ok(());
    };
    let comments = repository.issue_comments(issue.number).await?;
    let authors = comments
        .iter()
        .filter(|comment| until.is_none_or(|until| comment.created_at <= until))
        .map(|comment| (comment.user.login.as_str(), comment.created_at));
    if new_trusted_user_comment(&engine.config, authors, since) {
        engine.store.tasks().reset_counters(task.id).await?;
    }
    let (replies, answered) = replies(&engine.config, app_slug, &comments, since, until);
    if answered && task.state != "needs_human" && issue.has_label(NEEDS_HUMAN_LABEL) {
        repository
            .remove_label(issue.number, NEEDS_HUMAN_LABEL)
            .await?;
    }
    for comment in replies {
        let text = event_text(
            comment.created_at,
            "comment on",
            issue,
            &comment.user.login,
            &comment.body,
        )?;
        lead_events::add(
            engine,
            name,
            task.workstream,
            Some(issue.number),
            "comment",
            &text,
        )
        .await?;
    }
    Ok(())
}

pub(crate) async fn pull_request_comments(
    engine: &Engine,
    repository: &Repository,
    number: i64,
    since: Option<OffsetDateTime>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let tasks = engine.store.tasks();
    let Some(task) = tasks
        .live_by_pull_request(&repository.full_name, number)
        .await?
    else {
        return Ok(());
    };
    let comments = repository.issue_comments(number).await?;
    let review_comments = repository.review_comments(number).await?;
    let authors = comments
        .iter()
        .map(|comment| (comment.user.login.as_str(), comment.created_at))
        .chain(
            review_comments
                .iter()
                .map(|comment| (comment.user.login.as_str(), comment.created_at)),
        );
    if new_trusted_user_comment(&engine.config, authors, since) {
        tasks.reset_counters(task.id).await?;
    }
    Ok(())
}

fn new_trusted_user_comment<'a>(
    config: &Config,
    authors: impl IntoIterator<Item = (&'a str, OffsetDateTime)>,
    since: Option<OffsetDateTime>,
) -> bool {
    authors.into_iter().any(|(login, created_at)| {
        since.is_none_or(|since| created_at > since)
            && config
                .trusted_users
                .iter()
                .any(|user| user.eq_ignore_ascii_case(login))
    })
}

// A comment after `until` came after the list of issues. The next poll reads it, because its cursor is `until`.
// Gives the new comments that are Lead events. The `bool` is `true` when the newest of them is newer than the last comment of the Mobius App, the question.
fn replies<'a>(
    config: &Config,
    app_slug: &str,
    comments: &'a [Comment],
    since: Option<OffsetDateTime>,
    until: Option<OffsetDateTime>,
) -> (Vec<&'a Comment>, bool) {
    let asked_at = comments
        .iter()
        .filter(|comment| {
            comment
                .user
                .login
                .eq_ignore_ascii_case(&app_login(app_slug))
        })
        .map(|comment| comment.created_at)
        .max();
    let replies: Vec<&Comment> = comments
        .iter()
        .filter(|comment| {
            since.is_none_or(|since| comment.created_at > since)
                && until.is_none_or(|until| comment.created_at <= until)
                && comment_is_event(config, app_slug, comment)
        })
        .collect();
    let answered = replies.iter().map(|comment| comment.created_at).max() > asked_at;
    (replies, answered)
}

pub(crate) async fn live_task(
    engine: &Engine,
    repository: &str,
    workstream: i64,
    number: i64,
) -> Result<Task, Box<dyn Error + Send + Sync>> {
    Ok(engine
        .store
        .tasks()
        .live(repository, number)
        .await?
        .filter(|task| task.workstream == workstream)
        .ok_or_else(|| format!("#{number} has no live task in this Workstream."))?)
}

pub(crate) async fn ask(
    engine: &Engine,
    repository: &Repository,
    workstream: i64,
    number: i64,
    text: &str,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    let name = &repository.full_name;
    live_task(engine, name, workstream, number).await?;
    let issue = repository
        .issue(number)
        .await?
        .ok_or_else(|| format!("#{number} does not exist."))?;
    repository.add_comment(number, text).await?;
    repository.add_label(number, NEEDS_HUMAN_LABEL).await?;
    inbox::add(
        engine,
        InboxKind::Question,
        name,
        workstream,
        number,
        text,
        &issue.html_url,
    )
    .await?;
    Ok(format!("Asked on #{number}."))
}

pub(crate) async fn decline(
    engine: &Engine,
    app_slug: &str,
    repository: &Repository,
    workstream: i64,
    number: i64,
    reason: &str,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    let name = &repository.full_name;
    let task = live_task(engine, name, workstream, number).await?;
    repository.add_comment(number, reason).await?;
    repository.remove_label(number, WORKING_LABEL).await?;
    engine.store.tasks().end(task.id).await?;
    engine.workers.changed.notify_waiters();
    let issue = repository
        .issue(number)
        .await?
        .ok_or_else(|| format!("#{number} does not exist."))?;
    activity::add(
        engine,
        name,
        workstream,
        number,
        &app_login(app_slug),
        &format!("Declined \"{}\"", issue.title),
        &issue.html_url,
    )
    .await?;
    Ok(format!("Declined #{number}."))
}

pub(crate) async fn fix_round(
    engine: &Engine,
    repository: &Repository,
    workstream: i64,
    number: i64,
    findings: &str,
    parent: i64,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    let name = &repository.full_name;
    let task = live_task(engine, name, workstream, number).await?;
    let (Some(pull_request), Some(branch)) = (task.pull_request, task.branch) else {
        return Err(format!("The task of #{number} has no pull request.").into());
    };
    let pull_request = repository.pull_request(pull_request).await?;
    let title = repository
        .issue(number)
        .await?
        .ok_or_else(|| format!("#{number} does not exist."))?
        .title;
    let tasks = engine.store.tasks();
    if !tasks
        .set_state(task.id, "ready_for_review", "working")
        .await?
    {
        return Err(format!(
            "The task of #{number} is {}, not ready_for_review.",
            task.state
        )
        .into());
    }
    let round = implementer::Round {
        repository: name.clone(),
        workstream,
        task: task.id,
        number,
        title,
        branch,
        pull_request,
        check_run: None,
        counts: true,
        items: format!("\nFindings of the Lead:\n{findings}\n"),
        parent: Some(parent),
    };
    if let Err(error) = implementer::fix_round(engine, repository, round).await {
        tasks
            .set_state(task.id, "working", "ready_for_review")
            .await?;
        return Err(error);
    }
    Ok(format!(
        "Sent the findings to a fix round of #{number}. At max_fix_rounds, Mobius stops the task instead."
    ))
}

// After a `move_issue` of the Triager, the Mobius App removes `mobius:no-workstream` and adds `mobius:ready` again. Only this sequence, after a triage of the Mobius App with no other `mobius:ready` between, counts with the actor of the `mobius:ready` before the triage.
fn ready_actor<'a>(events: &'a [IssueEvent], app_login: &str) -> Option<&'a str> {
    let is = |event: &IssueEvent, kind: &str, name: &str| {
        event.event == kind && event.label.as_ref().is_some_and(|label| label.name == name)
    };
    let actor = |event: &'a IssueEvent| event.actor.as_ref().map(|actor| actor.login.as_str());
    let by_app = |event: &IssueEvent| {
        event
            .actor
            .as_ref()
            .is_some_and(|actor| actor.login.eq_ignore_ascii_case(app_login))
    };
    let last = events
        .iter()
        .rposition(|event| is(event, "labeled", READY_LABEL))?;
    let login = actor(&events[last])?;
    if !by_app(&events[last]) || last == 0 {
        return Some(login);
    }
    let moved = &events[last - 1];
    if !(is(moved, "unlabeled", NO_WORKSTREAM_LABEL) && by_app(moved)) {
        return Some(login);
    }
    let Some(triage) = events[..last - 1]
        .iter()
        .rposition(|event| is(event, "labeled", NO_WORKSTREAM_LABEL))
    else {
        return Some(login);
    };
    if !by_app(&events[triage])
        || events[triage..last]
            .iter()
            .any(|event| is(event, "labeled", READY_LABEL))
    {
        return Some(login);
    }
    events[..triage]
        .iter()
        .rev()
        .find(|event| is(event, "labeled", READY_LABEL))
        .and_then(actor)
}

// A comment of the Lead session has a trusted user as author and the Mobius App in `performed_via_github_app`.
fn comment_is_event(config: &Config, app_slug: &str, comment: &Comment) -> bool {
    config
        .trusted_users
        .iter()
        .any(|user| user.eq_ignore_ascii_case(&comment.user.login))
        && comment
            .performed_via_github_app
            .as_ref()
            .is_none_or(|app| app.slug != app_slug)
}

pub(crate) fn event_text(
    time: OffsetDateTime,
    what: &str,
    issue: &Issue,
    actor: &str,
    body: &str,
) -> Result<String, time::error::Format> {
    let quoted: Vec<String> = body.lines().map(|line| format!("> {line}")).collect();
    Ok(format!(
        "{} {what} #{} \"{}\" by @{actor}:\n\n{}",
        time.format(TIME_FORMAT)?,
        issue.number,
        issue.title,
        quoted.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use mobius_github::{AppRef, Label, User};

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

    fn comment(author: &str, app: Option<&str>) -> Comment {
        Comment {
            id: 1,
            user: User {
                login: author.to_string(),
            },
            body: "Use cents.".to_string(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            performed_via_github_app: app.map(|slug| AppRef {
                slug: slug.to_string(),
            }),
        }
    }

    #[test]
    fn the_ready_actor_is_the_actor_of_the_last_ready_label() {
        let events = [
            event("labeled", "mobius:ready", "mallory"),
            event("unlabeled", "mobius:ready", "mallory"),
            event("labeled", "mobius:ready", "owner"),
            event("labeled", "bug", "mallory"),
            event("unlabeled", "mobius:ready", "mallory"),
        ];

        assert_eq!(ready_actor(&events, "mobius-app[bot]"), Some("owner"));
    }

    #[test]
    fn after_a_triage_the_actor_of_the_first_ready_label_counts() {
        let events = [
            event("labeled", "mobius:ready", "owner"),
            event("labeled", "mobius:no-workstream", "mobius-app[bot]"),
            event("unlabeled", "mobius:ready", "mobius-app[bot]"),
            event("unlabeled", "mobius:no-workstream", "mobius-app[bot]"),
            event("labeled", "mobius:ready", "mobius-app[bot]"),
        ];

        assert_eq!(ready_actor(&events, "mobius-app[bot]"), Some("owner"));
    }

    #[test]
    fn an_old_triage_or_a_triage_of_a_person_does_not_count() {
        let old = [
            event("labeled", "mobius:ready", "owner"),
            event("labeled", "mobius:no-workstream", "mobius-app[bot]"),
            event("unlabeled", "mobius:no-workstream", "mobius-app[bot]"),
            event("labeled", "mobius:ready", "mobius-app[bot]"),
            event("unlabeled", "mobius:ready", "mobius-app[bot]"),
            event("labeled", "mobius:ready", "mobius-app[bot]"),
        ];
        let person = [
            event("labeled", "mobius:ready", "owner"),
            event("labeled", "mobius:no-workstream", "mallory"),
            event("unlabeled", "mobius:no-workstream", "mobius-app[bot]"),
            event("labeled", "mobius:ready", "mobius-app[bot]"),
        ];

        assert_eq!(
            ready_actor(&old, "mobius-app[bot]"),
            Some("mobius-app[bot]")
        );
        assert_eq!(
            ready_actor(&person, "mobius-app[bot]"),
            Some("mobius-app[bot]")
        );
    }

    #[test]
    fn a_ready_label_of_the_mobius_app_with_no_triage_has_the_mobius_app_as_actor() {
        let events = [event("labeled", "mobius:ready", "mobius-app[bot]")];

        assert_eq!(
            ready_actor(&events, "mobius-app[bot]"),
            Some("mobius-app[bot]")
        );
    }

    #[test]
    fn an_issue_with_no_ready_label_event_has_no_ready_actor() {
        assert_eq!(
            ready_actor(&[event("labeled", "bug", "owner")], "mobius-app[bot]"),
            None
        );
    }

    #[test]
    fn a_comment_of_a_trusted_user_is_an_event() {
        assert!(comment_is_event(
            &config(),
            "mobius-app",
            &comment("Owner", None)
        ));
        assert!(comment_is_event(
            &config(),
            "mobius-app",
            &comment("owner", Some("other-app"))
        ));
    }

    #[test]
    fn a_comment_of_the_chat_session_a_bot_or_a_stranger_is_not_an_event() {
        assert!(!comment_is_event(
            &config(),
            "mobius-app",
            &comment("owner", Some("mobius-app"))
        ));
        assert!(!comment_is_event(
            &config(),
            "mobius-app",
            &comment("coderabbitai[bot]", None)
        ));
        assert!(!comment_is_event(
            &config(),
            "mobius-app",
            &comment("mobius-app[bot]", None)
        ));
        assert!(!comment_is_event(
            &config(),
            "mobius-app",
            &comment("mallory", None)
        ));
    }

    fn comment_at(author: &str, seconds: i64) -> Comment {
        Comment {
            created_at: OffsetDateTime::from_unix_timestamp(seconds).unwrap(),
            ..comment(author, None)
        }
    }

    #[test]
    fn a_reply_after_the_last_comment_of_the_mobius_app_answers_it() {
        let comments = [
            comment_at("owner", 1),
            comment_at("mobius-app[bot]", 2),
            comment_at("owner", 3),
        ];

        let (replies, answered) = replies(&config(), "mobius-app", &comments, None, None);

        assert_eq!(replies.len(), 2);
        assert!(answered);
    }

    #[test]
    fn a_reply_before_the_last_comment_of_the_mobius_app_does_not_answer_it() {
        let comments = [
            comment_at("owner", 1),
            comment_at("mobius-app[bot]", 2),
            comment_at("mallory", 3),
        ];

        let (replies, answered) = replies(&config(), "mobius-app", &comments, None, None);

        assert_eq!(replies.len(), 1);
        assert!(!answered);
    }

    #[test]
    fn only_a_comment_after_the_cursor_is_a_reply() {
        let comments = [comment_at("owner", 1), comment_at("owner", 3)];
        let since = OffsetDateTime::from_unix_timestamp(2).ok();

        let (replies, answered) = replies(&config(), "mobius-app", &comments, since, None);

        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].created_at.unix_timestamp(), 3);
        assert!(answered);
    }

    #[test]
    fn a_question_after_the_end_of_the_poll_keeps_the_reply_before_it_unanswered() {
        let comments = [
            comment_at("mobius-app[bot]", 1),
            comment_at("owner", 2),
            comment_at("mobius-app[bot]", 4),
        ];
        let until = OffsetDateTime::from_unix_timestamp(3).ok();

        let (replies, answered) = replies(&config(), "mobius-app", &comments, None, until);

        assert_eq!(replies.len(), 1);
        assert!(!answered);
    }

    #[test]
    fn a_new_comment_of_a_trusted_user_resets_the_counters() {
        let since = OffsetDateTime::from_unix_timestamp(2).ok();
        let at = |seconds| OffsetDateTime::from_unix_timestamp(seconds).unwrap();

        assert!(new_trusted_user_comment(
            &config(),
            [("mallory", at(3)), ("Owner", at(3))],
            since
        ));
        assert!(new_trusted_user_comment(
            &config(),
            [("owner", at(1))],
            None
        ));
    }

    #[test]
    fn an_old_comment_or_a_comment_of_a_bot_or_a_stranger_does_not_reset_the_counters() {
        let since = OffsetDateTime::from_unix_timestamp(2).ok();
        let at = |seconds| OffsetDateTime::from_unix_timestamp(seconds).unwrap();

        assert!(!new_trusted_user_comment(
            &config(),
            [
                ("owner", at(2)),
                ("coderabbitai[bot]", at(3)),
                ("mobius-app[bot]", at(3)),
                ("mallory", at(3))
            ],
            since
        ));
    }

    #[test]
    fn the_event_text_has_the_time_the_kind_the_issue_and_the_quoted_text() {
        let issue: Issue = serde_json::from_value(serde_json::json!({
            "id": 100_042,
            "number": 42,
            "title": "Plan API",
            "body": null,
            "state": "open",
            "html_url": "https://github.com/owner/shop/issues/42",
            "repository_url": "https://api.github.com/repos/owner/shop",
            "updated_at": "2026-09-27T14:02:00Z",
            "labels": [],
            "pull_request": null,
            "user": { "login": "owner" },
            "issue_dependencies_summary": { "blocked_by": 0 }
        }))
        .unwrap();
        let time = time::macros::datetime!(2026-09-27 14:02 UTC);

        assert_eq!(
            event_text(
                time,
                "comment on",
                &issue,
                "owner",
                "Use cents.\nRound down."
            )
            .unwrap(),
            "2026-09-27 14:02 UTC comment on #42 \"Plan API\" by @owner:\n\n> Use cents.\n> Round down."
        );
    }
}
