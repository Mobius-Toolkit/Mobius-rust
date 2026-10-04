use std::collections::BTreeMap;
use std::error::Error;

use mobius_domain::Live;
use mobius_github::Repository;
use time::OffsetDateTime;

use crate::labels::{
    self, AUTOPILOT_LABEL, NEEDS_HUMAN_LABEL, NO_WORKSTREAM_LABEL, WORKING_LABEL, WORKSTREAM_LABEL,
};
use crate::trust::trusted_author;
use crate::workers::Work;
use crate::{
    Engine, activity, autopilot, copy, dispatch, ends, lead_events, recovery, triager, workstreams,
};

const ISSUES: &str = "issues";

pub(crate) fn spawn(engine: Engine) {
    tokio::spawn(async move {
        loop {
            if let Err(error) = poll(&engine).await {
                eprintln!("mobius: GitHub poll: {error}");
            }
            tokio::time::sleep(engine.config.poll_interval).await;
        }
    });
}

async fn poll(engine: &Engine) -> Result<(), Box<dyn Error + Send + Sync>> {
    let apps = engine.store.github_apps().list().await?;
    if apps.is_empty() {
        return Ok(());
    }
    let mut repositories = Vec::new();
    let mut listed_all = true;
    for app in apps {
        match engine
            .github
            .repositories(app.app_id, &app.slug, &app.private_key)
            .await
        {
            Ok(found) => repositories.extend(found),
            Err(error) => {
                listed_all = false;
                eprintln!("mobius: GitHub poll of the App {}: {error}", app.slug);
                // A failed list keeps the last known repositories of the App, so that `lost_access` ends none of their tasks.
                repositories.extend(
                    engine
                        .repositories
                        .read()
                        .unwrap()
                        .iter()
                        .filter(|repository| repository.app_id == app.app_id)
                        .cloned(),
                );
            }
        }
    }
    *engine.repositories.write().unwrap() = repositories.clone();
    ends::lost_access(engine, &repositories).await?;
    if listed_all {
        let names: Vec<String> = repositories
            .iter()
            .map(|repository| repository.full_name.clone())
            .collect();
        engine.store.workstream_copy().forget_except(&names).await?;
    }
    // The set is complete only after the poll reads all repositories.
    let mut work = BTreeMap::new();
    for repository in &repositories {
        if engine
            .labels_fixed
            .lock()
            .unwrap()
            .insert(repository.full_name.clone())
            && let Err(error) = labels::fix(repository).await
        {
            eprintln!("mobius: label fix of {}: {error}", repository.full_name);
        }
        if !engine
            .copied
            .lock()
            .unwrap()
            .contains(&repository.full_name)
        {
            match copy::sync(engine, repository).await {
                Ok(()) => {
                    engine
                        .copied
                        .lock()
                        .unwrap()
                        .insert(repository.full_name.clone());
                }
                Err(error) => eprintln!("mobius: full sync of {}: {error}", repository.full_name),
            }
        }
        if let Err(error) =
            poll_repository(engine, &repository.app_slug, repository, &mut work).await
        {
            eprintln!("mobius: GitHub poll of {}: {error}", repository.full_name);
            // A failed poll keeps the old work of the repository, so that it does not unblock a ticket.
            for (task, old) in engine.workers.work() {
                if old.repository == repository.full_name {
                    work.entry(task).or_insert(old);
                }
            }
        }
    }
    engine.workers.replace_work(work);
    Ok(())
}

async fn poll_repository(
    engine: &Engine,
    app_slug: &str,
    repository: &Repository,
    work: &mut BTreeMap<i64, Work>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    recovery::repository(engine, repository).await?;
    changed_issues(engine, app_slug, repository).await?;
    dispatch::dispatch_ready(engine, app_slug, repository).await?;
    autopilot::start(engine, app_slug, repository).await?;
    ends::check(engine, app_slug, repository, work).await
}

async fn changed_issues(
    engine: &Engine,
    app_slug: &str,
    repository: &Repository,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let name = &repository.full_name;
    let cursor = engine.store.sync_cursors().get(name, ISSUES).await?;
    let Some(page) = repository
        .issues_since(cursor.since, cursor.etag.as_deref())
        .await?
    else {
        return Ok(());
    };
    // At the first poll of a repository, Mobius cannot see which event is new.
    let first_poll = cursor.since.is_none();
    let mut copied = engine.copied.lock().unwrap().contains(name);
    let mut workstreams_changed = false;
    let until = page
        .issues
        .iter()
        .map(|issue| issue.updated_at)
        .max()
        .or(cursor.since);
    for issue in &page.issues {
        if issue.pull_request.is_some() {
            dispatch::pull_request_comments(engine, repository, issue.number, cursor.since).await?;
            continue;
        }
        if issue.has_label(WORKING_LABEL) || issue.has_label(NEEDS_HUMAN_LABEL) {
            dispatch::comment_events(engine, app_slug, repository, issue, cursor.since, until)
                .await?;
        }
        if !issue.has_label(NO_WORKSTREAM_LABEL) {
            triager::stop(engine, app_slug, repository, issue.number).await?;
        }
        let labeled = issue.has_label(WORKSTREAM_LABEL);
        let events = if labeled || workstreams::has_work(engine, name, issue.number).await? {
            repository.issue_events(issue.number).await?
        } else {
            Vec::new()
        };
        // After a failed update, the next poll replaces the copy with a full sync.
        if copied {
            match copy::update(engine, repository, issue, &events, !first_poll).await {
                Ok(changed) => workstreams_changed |= changed,
                Err(error) => {
                    eprintln!("mobius: update of the copy of {name}: {error}");
                    engine.copied.lock().unwrap().remove(name);
                    copied = false;
                }
            }
        }
        // Without a copy, the Workstreams screen reads GitHub, so it must read again.
        workstreams_changed |= !copied;
        for event in events {
            let Some(actor) = event.actor else {
                continue;
            };
            if cursor.since.is_some_and(|since| event.created_at <= since) {
                continue;
            }
            let trusted = trusted_author(&engine.config, app_slug, &actor.login);
            match (
                event.event.as_str(),
                event.label.as_ref().map(|label| label.name.as_str()),
            ) {
                // With a new Autopilot, a `mobius:ready` of the Mobius App can dispatch, so the ready list must not answer `304`.
                (_, Some(AUTOPILOT_LABEL)) => {
                    engine
                        .store
                        .sync_cursors()
                        .set(name, dispatch::READY_CURSOR, None, None)
                        .await?;
                }
                ("labeled", Some(WORKSTREAM_LABEL)) if trusted => {
                    activity::add(
                        engine,
                        name,
                        issue.number,
                        issue.number,
                        &actor.login,
                        &format!("New Workstream \"{}\"", issue.title),
                        &issue.html_url,
                    )
                    .await?;
                    if !first_poll {
                        let text = dispatch::event_text(
                            OffsetDateTime::now_utc(),
                            "creation of Workstream",
                            issue,
                            &actor.login,
                            issue.body.as_deref().unwrap_or_default(),
                        )?;
                        lead_events::add(engine, name, issue.number, None, "creation", &text)
                            .await?;
                    }
                }
                // A removal from any author counts, because it only stops work.
                ("unlabeled", Some(WORKSTREAM_LABEL)) if !first_poll => {
                    workstreams::stop(engine, repository, issue.number).await?;
                }
                // The label can go away before the poll sees the close.
                ("closed", _) if trusted && !first_poll => {
                    workstreams::close(engine, repository, issue.number).await?;
                }
                ("reopened", _) if trusted && labeled && !first_poll => {
                    workstreams::reopen(engine, repository, issue, &actor.login).await?;
                }
                _ => {}
            }
        }
    }
    if workstreams_changed {
        engine.broadcast(Live::Workstreams);
    }
    engine
        .store
        .sync_cursors()
        .set(name, ISSUES, until, page.etag.as_deref())
        .await
}
