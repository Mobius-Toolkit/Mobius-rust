use std::error::Error;
use std::path::Path;
use std::pin::Pin;

use mobius_domain::{Live, organization};
use mobius_github::{PullRequest, Repository};
use mobius_runner::{Check, Session};
use mobius_store::Task;
use serde_json::Value;
use time::OffsetDateTime;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use crate::labels::{NEEDS_HUMAN_LABEL, WORKING_LABEL};
use crate::lead::{self, Recorder};
use crate::trust::{self, app_login};
use crate::{
    Engine, TIME_FORMAT, agents, dispatch, ends, housekeeper, issues, lead_events, limits, mcp,
    reviewer, threads, workers,
};

pub(crate) const ROLE: &str = "implementer";
const ROLE_PROMPT: &str = include_str!("prompts/implementer.md");
pub(crate) const CHECK_RUN: &str = "Mobius";
// The Worker kind of a task in a conflict round.
pub(crate) const CONFLICT_ROUND: &str = "conflict_round";
// GitHub allows a maximum of 65535 characters in the summary of a check run.
const LOG_TAIL: usize = 60_000;
const DISK_FULL: &str = "No space left on device";

#[derive(Clone)]
struct Job {
    repository: String,
    workstream: i64,
    task: i64,
    number: i64,
    title: String,
    branch: Option<String>,
    // A fix round, a conflict round, or a start after `cannot_do` in one of them works on the pull request of an earlier session.
    pull_request: Option<PullRequest>,
    conflict_round: bool,
    prompt: String,
    // The session of the agent that started the work, or of the newest session of the issue when Mobius started it.
    parent: Option<i64>,
}

enum Outcome {
    Done(Pushed),
    CannotDo(String),
    CheckFailed(String),
    NotMerged,
    PushRejected(String),
    Stopped,
}

struct Pushed {
    branch: String,
    pull_request: PullRequest,
    head: String,
    check_run: i64,
}

pub(crate) async fn start(
    engine: &Engine,
    repository: &Repository,
    workstream: i64,
    number: i64,
    instructions: &str,
    parent: Option<i64>,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    let name = &repository.full_name;
    let task = dispatch::live_task(engine, name, workstream, number).await?;
    let brief = lead::brief(repository, workstream).await?;
    let sections = lead::repository_sections(engine, repository, ROLE).await?;
    let title = repository
        .issue(number)
        .await?
        .ok_or_else(|| format!("#{number} does not exist."))?
        .title;
    let trusted = trust::trusted_authors(engine, repository);
    let issue = issues::read_issue(repository, number, &trusted).await?;
    let pull_request = match task.pull_request {
        Some(number) => Some(repository.pull_request(number).await?),
        None => None,
    };
    if !engine.store.tasks().queue(task.id, "dispatched").await? {
        return Err(format!("The task of #{number} is {}, not dispatched.", task.state).into());
    }
    let job = Job {
        repository: name.clone(),
        workstream,
        task: task.id,
        number,
        title,
        branch: task.branch,
        pull_request,
        conflict_round: false,
        prompt: format!(
            "{ROLE_PROMPT}\n{sections}# Brief\n\n{brief}\n\n# Issue\n\n{issue}\n# Lead instructions\n\n{instructions}"
        ),
        parent,
    };
    engine
        .store
        .tasks()
        .set_worker(task.id, ROLE, Some(&job.prompt))
        .await?;
    tokio::spawn(run(engine.clone(), job));
    Ok(format!("Started an Implementer for #{number}."))
}

pub(crate) struct Round {
    pub(crate) repository: String,
    pub(crate) workstream: i64,
    pub(crate) task: i64,
    pub(crate) number: i64,
    pub(crate) title: String,
    pub(crate) branch: String,
    pub(crate) pull_request: PullRequest,
    // With no id of the `Mobius` check run of the head, a stop adds a failed check run on the head.
    pub(crate) check_run: Option<i64>,
    // A round with no `fix` action does not count toward `max_fix_rounds`.
    pub(crate) counts: bool,
    // The prompt text of the open items with their actions.
    pub(crate) items: String,
    // The session of the agent whose result started the round.
    pub(crate) parent: Option<i64>,
}

// At `max_fix_rounds`, a round that counts stops the task instead.
pub(crate) async fn fix_round(
    engine: &Engine,
    repository: &Repository,
    round: Round,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let tasks = engine.store.tasks();
    if round.counts
        && !tasks
            .add_fix_round(round.task, engine.config.max_fix_rounds)
            .await?
    {
        return stop_at_limit(engine, repository, &round, "fix").await;
    }
    let brief = lead::brief(repository, round.workstream).await?;
    let sections = lead::repository_sections(engine, repository, ROLE).await?;
    let issue = repository
        .issue(round.number)
        .await?
        .ok_or_else(|| format!("#{} does not exist.", round.number))?;
    // A task that the Lead declined during the review gets no fix round.
    if !tasks.queue(round.task, "working").await? {
        return Ok(());
    }
    let job = Job {
        repository: round.repository,
        workstream: round.workstream,
        task: round.task,
        number: round.number,
        title: round.title,
        branch: Some(round.branch),
        pull_request: Some(round.pull_request),
        conflict_round: false,
        prompt: format!(
            "{ROLE_PROMPT}\n{sections}# Brief\n\n{brief}\n\n# Issue\n\n#{} {}\n\n{}\n\n# Open items\n{}",
            round.number,
            issue.title,
            issue.body.unwrap_or_default(),
            round.items
        ),
        parent: round.parent,
    };
    tasks.set_worker(job.task, ROLE, Some(&job.prompt)).await?;
    tokio::spawn(run(engine.clone(), job));
    Ok(())
}

pub(crate) async fn restart(
    engine: &Engine,
    repository: &Repository,
    task: &Task,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let Some(prompt) = task.worker_input.clone() else {
        return Ok(());
    };
    let title = repository
        .issue(task.issue)
        .await?
        .ok_or_else(|| format!("#{} does not exist.", task.issue))?
        .title;
    let pull_request = match task.pull_request {
        Some(number) => Some(repository.pull_request(number).await?),
        None => None,
    };
    if !engine.store.tasks().requeue(task.id).await? {
        return Ok(());
    }
    let job = Job {
        repository: repository.full_name.clone(),
        workstream: task.workstream,
        task: task.id,
        number: task.issue,
        title,
        branch: task.branch.clone(),
        pull_request,
        conflict_round: task.worker.as_deref() == Some(CONFLICT_ROUND),
        prompt,
        parent: lead::restart_parent(
            engine,
            &repository.full_name,
            task.workstream,
            task.issue,
            ROLE,
        )
        .await?,
    };
    tokio::spawn(run(engine.clone(), job));
    Ok(())
}

pub(crate) async fn conflict_round(
    engine: &Engine,
    repository: &Repository,
    task: &Task,
    pull_request: PullRequest,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let brief = lead::brief(repository, task.workstream).await?;
    let sections = lead::repository_sections(engine, repository, ROLE).await?;
    let issue = repository
        .issue(task.issue)
        .await?
        .ok_or_else(|| format!("#{} does not exist.", task.issue))?;
    let parent =
        lead::newest_session(engine, &repository.full_name, task.workstream, task.issue).await?;
    if !engine
        .store
        .tasks()
        .queue(task.id, "ready_for_review")
        .await?
    {
        return Ok(());
    }
    let prompt = format!(
        "{ROLE_PROMPT}\n{sections}# Brief\n\n{brief}\n\n# Issue\n\n#{} {}\n\n{}\n\n# Base branch\n\norigin/{}\n\nMerge the base branch and remove the conflicts. Make no other change.",
        task.issue,
        issue.title,
        issue.body.unwrap_or_default(),
        repository.default_branch
    );
    let job = Job {
        repository: repository.full_name.clone(),
        workstream: task.workstream,
        task: task.id,
        number: task.issue,
        title: issue.title,
        branch: task.branch.clone(),
        pull_request: Some(pull_request),
        conflict_round: true,
        prompt,
        parent,
    };
    engine
        .store
        .tasks()
        .set_worker(task.id, CONFLICT_ROUND, Some(&job.prompt))
        .await?;
    tokio::spawn(run(engine.clone(), job));
    Ok(())
}

// The future has a named type, because it and the future of `reviewer::run` start each other.
fn run(engine: Engine, job: Job) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    Box::pin(async move {
        let Err(error) = session(&engine, &job).await else {
            return;
        };
        eprintln!(
            "mobius: Implementer of {}#{}: {error}",
            job.repository, job.number
        );
        let restart = housekeeper::restart(
            &engine,
            &job.repository,
            job.workstream,
            job.task,
            job.number,
            &job.title,
            &error.to_string(),
        )
        .await;
        match restart {
            Ok(true) => match engine.store.tasks().queue(job.task, "working").await {
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

pub(crate) async fn stop_at_limit(
    engine: &Engine,
    repository: &Repository,
    round: &Round,
    limit: &str,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let max = engine.config.max_fix_rounds;
    if !hand_to_human(engine, &round.repository, round.task, round.number).await? {
        return Ok(());
    }
    let summary = format!("The pull request has open items after {max} {limit} rounds.");
    match round.check_run {
        Some(id) => repository.set_check_run_conclusion(id, "failure").await?,
        None => {
            repository
                .create_failed_check_run(
                    CHECK_RUN,
                    &round.pull_request.head.sha,
                    "Round limit",
                    &summary,
                )
                .await?
        }
    }
    let text = stop_text(
        OffsetDateTime::now_utc(),
        round.number,
        &round.title,
        &format!(
            "the pull request has open items after {max} {limit} rounds. Mobius set the Mobius check to failure and added mobius:needs-human."
        ),
    )?;
    lead_events::add(
        engine,
        &round.repository,
        round.workstream,
        Some(round.number),
        "stop",
        &text,
    )
    .await
}

// Gives `false` when the task is not queued or working, for example after a decline of the Lead.
pub(crate) async fn hand_to_human(
    engine: &Engine,
    repository: &str,
    task: i64,
    number: i64,
) -> Result<bool, Box<dyn Error + Send + Sync>> {
    let tasks = engine.store.tasks();
    if !tasks.set_state(task, "working", "needs_human").await?
        && !tasks.set_state(task, "queued", "needs_human").await?
    {
        return Ok(false);
    }
    let repository = engine.repository(repository)?;
    repository.remove_label(number, WORKING_LABEL).await?;
    repository.add_label(number, NEEDS_HUMAN_LABEL).await?;
    Ok(true)
}

async fn session(engine: &Engine, job: &Job) -> Result<(), Box<dyn Error + Send + Sync>> {
    // The subscription comes before the first state change, so the session gets each stop of the task.
    let mut stops = engine.stops.subscribe();
    let session = lead::add_session(
        engine,
        ROLE,
        &engine.config.roles.implementer,
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
    let ticket = job.pull_request.is_none();
    let slot = match workers::slot(
        engine,
        job.task,
        session,
        workers::Role::Implementer,
        ticket,
    )
    .await
    {
        Ok(slot) => slot,
        Err(error) => {
            recorder.fail(&error.to_string()).await?;
            return Err(error);
        }
    };
    // The Implementer keeps its slot until this function returns, also while its check waits and runs.
    let Some(_slot) = slot else {
        return lead::end_session(engine, session, "declined").await;
    };
    let (cannot_do, mut reasons) = mpsc::unbounded_channel();
    let (replies, mut held) = mpsc::unbounded_channel();
    let key = mcp::open(
        engine,
        mcp::Caller {
            session,
            role: ROLE,
            organization: organization(&job.repository).to_string(),
            repository: job.repository.clone(),
            workstream: job.workstream,
            cannot_do: Some(cannot_do),
            fix: job.pull_request.as_ref().map(|pull_request| mcp::Fix {
                pull_request: pull_request.number,
                replies,
            }),
            review: None,
            judge: None,
            turn: None,
        },
    )?;
    let result = tokio::select! {
        result = implement(
            engine,
            job,
            session,
            &key,
            &mut recorder,
            &mut reasons,
            &mut held,
        ) => result,
        () = ends::stopped(&mut stops, job.task) => Ok(Outcome::Stopped),
    };
    mcp::close(engine, &key);
    match result {
        Ok(Outcome::Stopped) => lead::end_session(engine, session, "stopped").await,
        Ok(Outcome::Done(pushed)) => {
            lead::end_session(engine, session, "done").await?;
            // A task that the Lead declined during the Implementer session gets no Reviewer.
            if !reviewer::queue(engine, job.task).await? {
                return Ok(());
            }
            tokio::spawn(reviewer::run(
                engine.clone(),
                reviewer::Job {
                    repository: job.repository.clone(),
                    workstream: job.workstream,
                    task: job.task,
                    number: job.number,
                    title: job.title.clone(),
                    branch: pushed.branch,
                    pull_request: pushed.pull_request,
                    head: pushed.head,
                    check_run: pushed.check_run,
                    parent: Some(session),
                },
            ));
            Ok(())
        }
        Ok(Outcome::CheckFailed(_)) => {
            lead::end_session(engine, session, "check_failed").await?;
            if !hand_to_human(engine, &job.repository, job.task, job.number).await? {
                return Ok(());
            }
            let text = stop_text(
                OffsetDateTime::now_utc(),
                job.number,
                &job.title,
                &format!(
                    ".mobius/check failed {} times. Mobius pushed the work, set the Mobius check to failure, and added mobius:needs-human.",
                    engine.config.max_check_attempts
                ),
            )?;
            lead_events::add(
                engine,
                &job.repository,
                job.workstream,
                Some(job.number),
                "stop",
                &text,
            )
            .await
        }
        Ok(Outcome::NotMerged) => {
            lead::end_session(engine, session, "not_merged").await?;
            if !hand_to_human(engine, &job.repository, job.task, job.number).await? {
                return Ok(());
            }
            let text = stop_text(
                OffsetDateTime::now_utc(),
                job.number,
                &job.title,
                "the conflict round did not merge the base branch. Mobius pushed the work, set the Mobius check to failure, and added mobius:needs-human.",
            )?;
            lead_events::add(
                engine,
                &job.repository,
                job.workstream,
                Some(job.number),
                "stop",
                &text,
            )
            .await
        }
        Ok(Outcome::PushRejected(error)) => {
            lead::end_session(engine, session, "push_rejected").await?;
            if !hand_to_human(engine, &job.repository, job.task, job.number).await? {
                return Ok(());
            }
            let text = stop_text(
                OffsetDateTime::now_utc(),
                job.number,
                &job.title,
                &format!(
                    "GitHub rejected the push. Mobius added mobius:needs-human. Git gave this error:\n\n```\n{error}\n```"
                ),
            )?;
            lead_events::add(
                engine,
                &job.repository,
                job.workstream,
                Some(job.number),
                "stop",
                &text,
            )
            .await
        }
        Ok(Outcome::CannotDo(reason)) => {
            lead::end_session(engine, session, "cannot_do").await?;
            // A task that the Lead declined during the turn gets no event.
            if !engine
                .store
                .tasks()
                .set_state(job.task, "working", "dispatched")
                .await?
            {
                return Ok(());
            }
            let text = cannot_do_text(OffsetDateTime::now_utc(), job, &reason)?;
            lead_events::add(
                engine,
                &job.repository,
                job.workstream,
                Some(job.number),
                "cannot_do",
                &text,
            )
            .await
        }
        Err(error) => {
            recorder.fail(&error.to_string()).await?;
            Err(error)
        }
    }
}

async fn implement(
    engine: &Engine,
    job: &Job,
    session_id: i64,
    session_key: &str,
    recorder: &mut Recorder,
    reasons: &mut UnboundedReceiver<String>,
    held: &mut UnboundedReceiver<mcp::Reply>,
) -> Result<Outcome, Box<dyn Error + Send + Sync>> {
    let data_dir = &engine.config.data_dir;
    let name = &job.repository;
    let worktree = mobius_runner::task_dir(data_dir, name, job.number);
    // A new branch is free on `origin`, so only a branch of an earlier session needs a pull.
    let branch = match &job.branch {
        Some(branch) => {
            let _git = engine.git.lock().await;
            let repository = engine.repository(name)?;
            mobius_runner::fetch(data_dir, name, &repository.clone_url, repository.token()).await?;
            mobius_runner::pull(data_dir, &worktree, branch).await?;
            branch.clone()
        }
        None => {
            let repository = engine.repository(name)?;
            let login = app_login(&repository.app_slug);
            let id = engine.github.user_id(&login).await?;
            let _git = engine.git.lock().await;
            mobius_runner::fetch(data_dir, name, &repository.clone_url, repository.token()).await?;
            let branch = mobius_runner::add_worktree(
                data_dir,
                name,
                job.number,
                &repository.default_branch,
                &login,
                &format!("{id}+{login}@users.noreply.github.com"),
            )
            .await?;
            engine.store.tasks().set_branch(job.task, &branch).await?;
            branch
        }
    };
    let repository = engine.repository(name)?;
    let base = format!("origin/{}", repository.default_branch);
    // All worktrees share the refs of the bare repository, so a fetch of a different task can move `base` during the round.
    let base_commit = mobius_runner::rev_parse(data_dir, &worktree, &base).await?;
    let (session, mut updates) = lead::start(
        engine,
        &engine.config.roles.implementer,
        session_id,
        &worktree,
        session_key,
        None,
    )
    .await?;
    let outcome = turns_and_checks(
        engine,
        job,
        &worktree,
        &session,
        &mut updates,
        recorder,
        reasons,
    )
    .await;
    session.close().await;
    let log = match outcome? {
        Some(Outcome::CheckFailed(log)) => Some(log),
        Some(outcome) => return Ok(outcome),
        None => None,
    };
    let repository = engine.repository(name)?;
    let merged = !job.conflict_round
        || mobius_runner::head_contains(data_dir, &worktree, &base_commit).await?;
    let sha = {
        let _git = engine.git.lock().await;
        mobius_runner::fetch(data_dir, name, &repository.clone_url, repository.token()).await?;
        mobius_runner::pull(data_dir, &worktree, &branch).await?;
        match mobius_runner::push(data_dir, &worktree, repository.token(), &branch).await {
            Ok(sha) => sha,
            Err(error) if error.contains("[remote rejected]") => {
                return Ok(Outcome::PushRejected(error));
            }
            Err(error) => return Err(error.into()),
        }
    };
    let pull_request = match &job.pull_request {
        Some(pull_request) => pull_request.clone(),
        None => {
            let pull_request = repository
                .create_draft_pull_request(
                    &job.title,
                    &branch,
                    &repository.default_branch,
                    &format!("Closes #{}", job.number),
                )
                .await?;
            engine
                .store
                .tasks()
                .set_pull_request(job.task, pull_request.number)
                .await?;
            let open = engine.store.sessions().get(session_id).await?;
            engine.broadcast(Live::Agent(agents::node(open)));
            pull_request
        }
    };
    while let Ok(reply) = held.try_recv() {
        threads::reply(&repository, pull_request.number, &reply.target, &reply.text).await?;
    }
    if log.is_none() && !merged {
        repository
            .create_failed_check_run(
                CHECK_RUN,
                &sha,
                "Conflict round failed",
                &format!("The Implementer did not merge `{base}`."),
            )
            .await?;
        return Ok(Outcome::NotMerged);
    }
    let Some(log) = log else {
        let check_run = repository
            .create_check_run(CHECK_RUN, &sha, "in_progress")
            .await?;
        return Ok(Outcome::Done(Pushed {
            branch,
            pull_request,
            head: sha,
            check_run,
        }));
    };
    let summary = format!(
        "`.mobius/check` failed {} times. The last output ends with these lines:\n\n```\n{log}\n```",
        engine.config.max_check_attempts
    );
    repository
        .create_failed_check_run(CHECK_RUN, &sha, "Local check failed", &summary)
        .await?;
    Ok(Outcome::CheckFailed(log))
}

// Gives `None` when the local check passes.
async fn turns_and_checks(
    engine: &Engine,
    job: &Job,
    worktree: &Path,
    session: &Session,
    updates: &mut UnboundedReceiver<Value>,
    recorder: &mut Recorder,
    reasons: &mut UnboundedReceiver<String>,
) -> Result<Option<Outcome>, Box<dyn Error + Send + Sync>> {
    let mut prompt = job.prompt.clone();
    let mut attempts = 0;
    loop {
        if let Some(reason) = turn(session, &prompt, updates, recorder, reasons).await? {
            return Ok(Some(Outcome::CannotDo(reason)));
        }
        let check = loop {
            let check = {
                let _check = engine.checks.acquire().await?;
                mobius_runner::check(
                    &engine.config.data_dir,
                    worktree,
                    &engine.harness_path,
                    engine.config.check_timeout,
                )
                .await?
            };
            match check {
                Check::Failed(log) if log.contains(DISK_FULL) => {
                    housekeeper::wait_for_disk(
                        engine,
                        &job.repository,
                        job.workstream,
                        job.number,
                        &job.title,
                    )
                    .await?
                }
                check => break check,
            }
        };
        let Check::Failed(log) = check else {
            return Ok(None);
        };
        attempts += 1;
        let log = tail(&log);
        if attempts >= engine.config.max_check_attempts {
            return Ok(Some(Outcome::CheckFailed(log)));
        }
        prompt = format!(
            "The local check `.mobius/check` failed. Fix the code and commit your work. The output ends with these lines:\n\n```\n{log}\n```"
        );
    }
}

// Gives the reason when the Implementer calls `cannot_do`.
async fn turn(
    session: &Session,
    prompt: &str,
    updates: &mut UnboundedReceiver<Value>,
    recorder: &mut Recorder,
    reasons: &mut UnboundedReceiver<String>,
) -> Result<Option<String>, Box<dyn Error + Send + Sync>> {
    loop {
        recorder.prompt(prompt).await?;
        let mut reason = None;
        let result = {
            let turn = session.prompt(prompt);
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    biased;
                    result = &mut turn => break result,
                    Some(update) = updates.recv() => recorder.update(update).await?,
                    Some(text) = reasons.recv() => {
                        session.cancel();
                        reason = Some(text);
                    }
                }
            }
        };
        // The connection reads each update of the turn before the answer to the prompt.
        while let Ok(update) = updates.try_recv() {
            recorder.update(update).await?;
        }
        // With `biased`, the end of the turn wins over a reason that arrived just before it.
        if let Some(reason) = reason.or_else(|| reasons.try_recv().ok()) {
            return Ok(Some(reason));
        }
        if let Err(error) = &result
            && limits::wait_out(recorder, session.harness(), error).await?
        {
            continue;
        }
        result?;
        return Ok(None);
    }
}

fn tail(log: &str) -> String {
    let count = log.chars().count();
    log.chars().skip(count.saturating_sub(LOG_TAIL)).collect()
}

fn cannot_do_text(
    time: OffsetDateTime,
    job: &Job,
    reason: &str,
) -> Result<String, time::error::Format> {
    let quoted: Vec<String> = reason.lines().map(|line| format!("> {line}")).collect();
    Ok(format!(
        "{} cannot_do on #{} \"{}\" by the Implementer:\n\n{}",
        time.format(TIME_FORMAT)?,
        job.number,
        job.title,
        quoted.join("\n")
    ))
}

fn stop_text(
    time: OffsetDateTime,
    number: i64,
    title: &str,
    reason: &str,
) -> Result<String, time::error::Format> {
    Ok(format!(
        "{} stop of #{number} \"{title}\": {reason}",
        time.format(TIME_FORMAT)?
    ))
}
