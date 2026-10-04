use std::error::Error;
use std::pin::Pin;

use mobius_domain::{InboxKind, organization};
use mobius_github::{PullRequest, Repository, ReviewThread};
use mobius_store::Task;
use time::OffsetDateTime;

use crate::lead::{self, Recorder};
use crate::trust::{self, app_login};
use crate::{
    Engine, TIME_FORMAT, ends, housekeeper, implementer, inbox, issues, lead_events, mcp, workers,
};

pub(crate) const ROLE: &str = "reviewer";
const ROLE_PROMPT: &str = include_str!("prompts/reviewer.md");

#[derive(Clone)]
pub(crate) struct Job {
    pub(crate) repository: String,
    pub(crate) workstream: i64,
    pub(crate) task: i64,
    pub(crate) number: i64,
    pub(crate) title: String,
    pub(crate) branch: String,
    pub(crate) pull_request: PullRequest,
    pub(crate) head: String,
    pub(crate) check_run: i64,
    // The session of the Implementer that pushed the head.
    pub(crate) parent: Option<i64>,
}

impl Job {
    fn round(&self, items: String, parent: Option<i64>) -> implementer::Round {
        implementer::Round {
            repository: self.repository.clone(),
            workstream: self.workstream,
            task: self.task,
            number: self.number,
            title: self.title.clone(),
            branch: self.branch.clone(),
            pull_request: self.pull_request.clone(),
            check_run: Some(self.check_run),
            counts: true,
            items,
            parent,
        }
    }
}

// Gives `false` when the task is not `working`, for example after a decline of the Lead.
pub(crate) async fn queue(
    engine: &Engine,
    task: i64,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let tasks = engine.store.tasks();
    if !tasks.queue(task, "working").await? {
        return Ok(false);
    }
    tasks.set_worker(task, ROLE, None).await?;
    Ok(true)
}

// The head of the pull request gets a new `Mobius` check run, because the store has no id of the old one.
pub(crate) async fn restart(
    engine: &Engine,
    repository: &Repository,
    task: &Task,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let (Some(number), Some(branch)) = (task.pull_request, task.branch.clone()) else {
        return Ok(());
    };
    if let Err(error) = abandon_round(
        engine,
        repository,
        task,
        "Mobius restarted before the run ended.",
    )
    .await
    {
        eprintln!(
            "mobius: round comment of {}#{}: {error}",
            repository.full_name, task.issue
        );
    }
    let pull_request = repository.pull_request(number).await?;
    let title = repository
        .issue(task.issue)
        .await?
        .ok_or_else(|| format!("#{} does not exist.", task.issue))?
        .title;
    let head = pull_request.head.sha.clone();
    if !engine.store.tasks().requeue(task.id).await? {
        return Ok(());
    }
    let check_run = repository
        .create_check_run(implementer::CHECK_RUN, &head, "in_progress")
        .await?;
    tokio::spawn(run(
        engine.clone(),
        Job {
            repository: repository.full_name.clone(),
            workstream: task.workstream,
            task: task.id,
            number: task.issue,
            title,
            branch,
            pull_request,
            head,
            check_run,
            parent: lead::restart_parent(
                engine,
                &repository.full_name,
                task.workstream,
                task.issue,
                ROLE,
            )
            .await?,
        },
    ));
    Ok(())
}

// The future has a named type, because it starts itself again after a failure.
pub(crate) fn run(engine: Engine, job: Job) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    Box::pin(async move {
        let Err(error) = session(&engine, &job).await else {
            return;
        };
        eprintln!(
            "mobius: Reviewer of {}#{}: {error}",
            job.repository, job.number
        );
        match housekeeper::restart(
            &engine,
            &job.repository,
            job.workstream,
            job.task,
            job.number,
            &job.title,
            &error.to_string(),
        )
        .await
        {
            Ok(true) => match queue(&engine, job.task).await {
                Ok(true) => {
                    tokio::spawn(run(engine.clone(), job));
                }
                Ok(false) => {}
                Err(failure) => eprintln!(
                    "mobius: restart of {}#{}: {failure}",
                    job.repository, job.number
                ),
            },
            Ok(false) => {}
            Err(failure) => eprintln!(
                "mobius: restart of {}#{}: {failure}",
                job.repository, job.number
            ),
        }
    })
}

async fn session(engine: &Engine, job: &Job) -> Result<(), Box<dyn Error + Send + Sync>> {
    let Some(task) = engine
        .store
        .tasks()
        .live(&job.repository, job.number)
        .await?
    else {
        return Ok(());
    };
    if task.review_rounds >= i64::from(engine.config.max_fix_rounds) {
        let repository = engine.repository(&job.repository)?;
        implementer::stop_at_limit(
            engine,
            &repository,
            &job.round(String::new(), job.parent),
            "review",
        )
        .await?;
        let max = engine.config.max_fix_rounds;
        repository
            .add_comment(
                job.pull_request.number,
                &format!(
                    "Review not started. Limit reached ({max} of {max}). Mobius added mobius:needs-human. Add a comment on this pull request to continue."
                ),
            )
            .await?;
        return Ok(());
    }
    // The subscription comes before the first state change, so the session gets each stop of the task.
    let mut stops = engine.stops.subscribe();
    let binding = &engine.config.roles.reviewer;
    let session = lead::add_session(
        engine,
        ROLE,
        binding,
        organization(&job.repository),
        &job.repository,
        job.workstream,
        lead::Links {
            issue: Some(job.number),
            parent: job.parent,
        },
    )
    .await?;
    let mut recorder = Recorder::new(
        engine,
        session,
        organization(&job.repository),
        &job.repository,
        job.workstream,
        None,
    );
    let slot = match workers::slot(engine, job.task, session, workers::Role::Reviewer, false).await
    {
        Ok(slot) => slot,
        Err(error) => {
            recorder.fail(&error.to_string()).await?;
            return Err(error);
        }
    };
    let Some(_slot) = slot else {
        return lead::end_session(engine, session, "declined").await;
    };
    // A comment of a trusted user during the wait for the slot resets the counters of the task.
    let Some(task) = engine
        .store
        .tasks()
        .live(&job.repository, job.number)
        .await?
    else {
        return lead::end_session(engine, session, "stopped").await;
    };
    let key = mcp::open(
        engine,
        mcp::Caller {
            session,
            role: ROLE,
            organization: organization(&job.repository).to_string(),
            repository: job.repository.clone(),
            workstream: job.workstream,
            cannot_do: None,
            fix: None,
            review: Some(mcp::Review {
                pull_request: job.pull_request.number,
                head: job.head.clone(),
            }),
            judge: None,
            turn: None,
        },
    )?;
    let result = tokio::select! {
        result = async {
            let comment = start_round(engine, job, &task).await?;
            review(engine, job, comment, session, &key, &mut recorder).await
        } => result.map(|()| "done"),
        () = ends::stopped(&mut stops, job.task) => Ok("stopped"),
    };
    mcp::close(engine, &key);
    let reason = match &result {
        Ok("stopped") => Some("The task stopped.".to_string()),
        Err(error) => Some(format!("The run failed: {error}")),
        Ok(_) => None,
    };
    if let Some(reason) = reason {
        let repository = engine.repository(&job.repository)?;
        if let Err(error) = abandon_round(engine, &repository, &task, &reason).await {
            eprintln!(
                "mobius: round comment of {}#{}: {error}",
                job.repository, job.number
            );
        }
    }
    if let Ok("stopped") = result {
        let data_dir = &engine.config.data_dir;
        let dir = mobius_runner::review_dir(data_dir, &job.repository, session);
        let _git = engine.git.lock().await;
        if dir.exists() {
            mobius_runner::remove_worktree(data_dir, &job.repository, &dir).await?;
        }
    }
    match result {
        Ok(reason) => lead::end_session(engine, session, reason).await,
        Err(error) => {
            recorder.fail(&error.to_string()).await?;
            Err(error)
        }
    }
}

async fn review(
    engine: &Engine,
    job: &Job,
    comment: i64,
    session_id: i64,
    session_key: &str,
    recorder: &mut Recorder,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let data_dir = &engine.config.data_dir;
    let name = &job.repository;
    let repository = engine.repository(name)?;
    let dir = mobius_runner::review_dir(data_dir, name, session_id);
    let base = {
        let _git = engine.git.lock().await;
        mobius_runner::fetch(data_dir, name, &repository.clone_url, repository.token()).await?;
        mobius_runner::add_detached_worktree(data_dir, name, &dir, &job.head).await?;
        mobius_runner::merge_base(
            data_dir,
            name,
            &format!("origin/{}", repository.default_branch),
            &job.head,
        )
        .await?
    };
    let brief = lead::brief(&repository, job.workstream).await?;
    let sections = lead::repository_sections(engine, &repository, ROLE).await?;
    let issue = repository
        .issue(job.number)
        .await?
        .ok_or_else(|| format!("#{} does not exist.", job.number))?;
    let trusted = trust::trusted_authors(engine, &repository);
    let threads = issues::review_threads(&repository, job.pull_request.number, &trusted).await?;
    let prompt = format!(
        "{ROLE_PROMPT}\n{sections}# Brief\n\n{brief}\n\n# Issue\n\n#{} {}\n\n{}\n\n# Commits\n\nBase commit: {base}\nHead commit: {}\n\nThe changes are `git diff {base} {}`.\n\n# Review threads\n{threads}",
        job.number,
        issue.title,
        issue.body.unwrap_or_default(),
        job.head,
        job.head
    );
    let (session, mut updates) = lead::start(
        engine,
        &engine.config.roles.reviewer,
        session_id,
        &dir,
        session_key,
        None,
    )
    .await?;
    let result = lead_events::turn(&session, &prompt, recorder, &mut updates).await;
    session.close().await;
    result?;
    {
        let _git = engine.git.lock().await;
        mobius_runner::remove_worktree(data_dir, name, &dir).await?;
    }
    // A comment of a trusted user during the turn resets the counters of the task.
    let Some(task) = engine
        .store
        .tasks()
        .live(&job.repository, job.number)
        .await?
    else {
        return Ok(());
    };
    let app_login = app_login(&repository.app_slug);
    let threads = repository.review_threads(job.pull_request.number).await?;
    let open: Vec<&ReviewThread> = threads
        .iter()
        .filter(|thread| is_open(thread, &trusted, &app_login))
        .collect();
    let max = i64::from(engine.config.max_fix_rounds);
    let round = task.review_rounds + 1;
    if open.is_empty() {
        end_round(
            engine,
            &repository,
            job,
            round,
            comment,
            "Ready for review.",
            &open,
        )
        .await?;
        return ready_for_review(engine, &repository, job, "working").await;
    }
    // A finding of the Reviewer has only the first comment. A thread with a reply of a trusted user or bot goes to the Judge.
    let findings: Vec<i64> = open
        .iter()
        .filter(|thread| {
            thread
                .authors
                .iter()
                .filter(|author| trusted(author))
                .all(|author| author.eq_ignore_ascii_case(&app_login))
        })
        .map(|thread| thread.comment)
        .collect();
    if findings.is_empty() {
        end_round(
            engine,
            &repository,
            job,
            round,
            comment,
            "The Judge takes the open threads.",
            &open,
        )
        .await?;
        engine
            .store
            .tasks()
            .set_state(job.task, "working", "reviewed")
            .await?;
        return Ok(());
    }
    let review_limit = round >= max;
    let at_limit = review_limit || task.fix_rounds >= max;
    let items = match at_limit {
        true => String::new(),
        false => {
            issues::fix_threads(&repository, job.pull_request.number, &findings, &trusted).await?
        }
    };
    let result = match (review_limit, at_limit) {
        (true, _) => format!(
            "Limit reached ({max} of {max}). Mobius added mobius:needs-human. Add a comment on this pull request to continue."
        ),
        (false, true) => format!(
            "Fix round limit reached ({max} of {max}). Mobius added mobius:needs-human. Add a comment on this pull request to continue."
        ),
        (false, false) => "A fix round started.".to_string(),
    };
    end_round(engine, &repository, job, round, comment, &result, &open).await?;
    match at_limit {
        true => {
            let limit = if review_limit { "review" } else { "fix" };
            implementer::stop_at_limit(
                engine,
                &repository,
                &job.round(items, Some(session_id)),
                limit,
            )
            .await
        }
        false => {
            implementer::fix_round(engine, &repository, job.round(items, Some(session_id))).await
        }
    }
}

// The run gets a new comment, also a restart with the same round number.
async fn start_round(
    engine: &Engine,
    job: &Job,
    task: &Task,
) -> Result<i64, Box<dyn Error + Send + Sync>> {
    let comment = engine
        .repository(&job.repository)?
        .add_comment(
            job.pull_request.number,
            &format!(
                "Review started, round {} of {}",
                task.review_rounds + 1,
                engine.config.max_fix_rounds
            ),
        )
        .await?;
    engine
        .store
        .tasks()
        .set_review_comment(job.task, Some(comment))
        .await?;
    Ok(comment)
}

async fn end_round(
    engine: &Engine,
    repository: &Repository,
    job: &Job,
    round: i64,
    comment: i64,
    result: &str,
    open: &[&ReviewThread],
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let links: String = open
        .iter()
        .map(|thread| {
            format!(
                "- {}#discussion_r{}\n",
                job.pull_request.html_url, thread.comment
            )
        })
        .collect();
    let body = format!(
        "Review ended, round {round} of {}\n\nResult: {result}\nOpen findings: {}\n\n{links}",
        engine.config.max_fix_rounds,
        open.len()
    );
    repository.update_comment(comment, body.trim_end()).await?;
    let tasks = engine.store.tasks();
    tasks.set_review_comment(job.task, None).await?;
    tasks.add_review_round(job.task).await?;
    Ok(())
}

// A task with no comment of a run in progress needs no update.
// The id leaves the task also when the update fails, so the failure does not repeat.
async fn abandon_round(
    engine: &Engine,
    repository: &Repository,
    task: &Task,
    reason: &str,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let tasks = engine.store.tasks();
    let Some(comment) = tasks.review_comment(task.id).await? else {
        return Ok(());
    };
    let updated = repository
        .update_comment(
            comment,
            &format!(
                "Review stopped, round {} of {}\n\n{reason}",
                task.review_rounds + 1,
                engine.config.max_fix_rounds
            ),
        )
        .await;
    tasks.set_review_comment(task.id, None).await?;
    Ok(updated?)
}

// A task that is not in the state `from`, for example after a decline of the Lead, stays a draft.
pub(crate) async fn ready_for_review(
    engine: &Engine,
    repository: &Repository,
    job: &Job,
    from: &str,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    if !engine
        .store
        .tasks()
        .set_state(job.task, from, "ready_for_review")
        .await?
    {
        return Ok(());
    }
    repository
        .set_check_run_conclusion(job.check_run, "success")
        .await?;
    if job.pull_request.draft {
        repository
            .mark_ready_for_review(&job.pull_request.node_id)
            .await?;
    }
    inbox::add(
        engine,
        InboxKind::ReadyForReview,
        &job.repository,
        job.workstream,
        job.number,
        &format!(
            "Pull request #{} of #{} \"{}\" is ready for review.",
            job.pull_request.number, job.number, job.title
        ),
        &job.pull_request.html_url,
    )
    .await?;
    let text = ready_text(OffsetDateTime::now_utc(), job)?;
    lead_events::add(
        engine,
        &job.repository,
        job.workstream,
        Some(job.number),
        "ready_for_review",
        &text,
    )
    .await
}

fn ready_text(time: OffsetDateTime, job: &Job) -> Result<String, time::error::Format> {
    Ok(format!(
        "{} ready for review of #{} \"{}\": pull request #{} {}.",
        time.format(TIME_FORMAT)?,
        job.number,
        job.title,
        job.pull_request.number,
        job.pull_request.html_url
    ))
}

// A thread is open when it is unresolved, a trusted author started it, and its last trusted comment is not a reply of the Mobius App. The first comment of the Mobius App is a finding of the Reviewer.
pub(crate) fn is_open(
    thread: &ReviewThread,
    trusted: impl Fn(&str) -> bool,
    app_login: &str,
) -> bool {
    let trusted_authors: Vec<&String> = thread
        .authors
        .iter()
        .filter(|author| trusted(author))
        .collect();
    !thread.resolved
        && thread.authors.first() == trusted_authors.first().copied()
        && match trusted_authors.as_slice() {
            [] => false,
            [_] => true,
            [.., last] => !last.eq_ignore_ascii_case(app_login),
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(resolved: bool, authors: &[&str]) -> ReviewThread {
        ReviewThread {
            id: "RT_1".to_string(),
            comment: 1,
            resolved,
            authors: authors.iter().map(|author| author.to_string()).collect(),
        }
    }

    fn open(thread: &ReviewThread) -> bool {
        is_open(thread, |login| login != "mallory", "mobius-app[bot]")
    }

    #[test]
    fn a_finding_of_the_reviewer_with_no_reply_is_open() {
        assert!(open(&thread(false, &["mobius-app[bot]"])));
    }

    #[test]
    fn a_thread_with_a_last_reply_of_the_mobius_app_is_not_open() {
        assert!(!open(&thread(
            false,
            &["mobius-app[bot]", "mobius-app[bot]"]
        )));
        assert!(!open(&thread(false, &["owner", "Mobius-App[bot]"])));
    }

    #[test]
    fn a_comment_of_a_trusted_user_after_a_reply_of_the_mobius_app_is_open() {
        assert!(open(&thread(false, &["owner", "mobius-app[bot]", "owner"])));
    }

    #[test]
    fn a_resolved_thread_is_not_open() {
        assert!(!open(&thread(true, &["owner"])));
    }

    #[test]
    fn a_thread_that_an_untrusted_author_started_is_not_open() {
        assert!(!open(&thread(false, &["mallory", "owner"])));
    }

    #[test]
    fn a_reply_of_an_untrusted_author_does_not_count() {
        assert!(!open(&thread(
            false,
            &["owner", "mobius-app[bot]", "mallory"]
        )));
    }
}
