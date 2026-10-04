use std::fs;
use std::path::Path;
use std::time::Duration;

use mobius_domain::{Author, ChatMessage, DrainEnd, Live, Session, TranscriptRow};
use mobius_engine::drain;
use mobius_engine::{Engine, activity, chat, github, workstreams};
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::{install_fake_harness, start_with_config, wait_for};
use serde_json::Value;
use tempfile::TempDir;

const REPOSITORY: &str = "owner/shop";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
const SAVE: &str = "Save in the Workstream memory what the next session needs.";
const DRAIN_REASON: &str = "Mobius prepares an upgrade";

const CLAUDE: &str = r#"
[options]
model = ["sonnet", "opus", "haiku"]
thought_level = ["low", "medium", "high"]
mode = ["default", "bypassPermissions"]

[[prompts]]
when = "Research the plan flow"
call = { tool = "start_researcher", arguments = { question = "How do plans work?" } }

[[prompts]]
when = "Plan the loyalty API"
reply = ["Hello"]

[[prompts]]
when = "dispatch of #41"
call = { tool = "start_implementer", arguments = { n = 41, instructions = "Store plans in cents." } }

[[prompts]]
when = "dispatch of #43"
call = { tool = "start_implementer", arguments = { n = 43, instructions = "Round prices down." } }

[[prompts]]
when = "You are the Judge"
reply = ["Judged."]

[[prompts]]
when = "You are the Reviewer"
shell = "true"

[[prompts]]
when = "You are the Triager"
reply = ["A proposal."]
"#;

fn implementer(go: &Path) -> String {
    format!(
        r#"
[options]
model = ["swe-1.5"]
thought_level = ["high"]

[[prompts]]
when = "Implementer"
shell = "while [ ! -e '{}' ]; do sleep 0.05; done; echo cents > plan.txt && git add plan.txt && git commit -q -m 'Add plan model'"
"#,
        go.display()
    )
}

async fn connect(data_dir: &TempDir, github: &FakeGitHub, go: &Path) -> Engine {
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.set_body(REPOSITORY, 12, "Ship loyalty plans to all shops.");
    for (number, title, body) in [
        (41, "Add plan model", "Plans have a price."),
        (43, "Add plan price", "Prices round down."),
    ] {
        github.add_issue(REPOSITORY, number, title);
        github.add_sub_issue(REPOSITORY, 12, number);
        github.set_body(REPOSITORY, number, body);
    }
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", CLAUDE);
    install_fake_harness(data_dir.path(), FAKE_AGENT, "devin", &implementer(go));
    let engine = start_with_config(
        data_dir.path(),
        "correct horse",
        &github.url,
        "max_agents = 1\nreview_quiet_period = \"50ms\"",
    )
    .await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || (!workstreams::list(&engine).await.unwrap().is_empty()).then_some(())).await;
    engine
}

async fn sessions(engine: &Engine, role: &str) -> Vec<Session> {
    engine
        .store
        .sessions()
        .list("owner", REPOSITORY, 12)
        .await
        .unwrap()
        .into_iter()
        .filter(|session| session.role == role)
        .collect()
}

async fn triagers(engine: &Engine) -> Vec<Session> {
    engine.store.sessions().with_role("triager").await.unwrap()
}

async fn messages(engine: &Engine) -> Vec<ChatMessage> {
    engine
        .store
        .chat_messages()
        .list("owner", REPOSITORY, 12)
        .await
        .unwrap()
}

async fn prompts(engine: &Engine, session: i64) -> Vec<String> {
    engine
        .store
        .transcript()
        .list(session)
        .await
        .unwrap()
        .iter()
        .filter(|row| row.kind == "prompt")
        .map(|row: &TranscriptRow| {
            let json: Value = serde_json::from_str(&row.json).unwrap();
            json["text"].as_str().unwrap().to_string()
        })
        .collect()
}

async fn role_prompts(engine: &Engine, role: &str) -> Vec<String> {
    let mut all = Vec::new();
    for session in sessions(engine, role).await {
        all.extend(prompts(engine, session.id).await);
    }
    all
}

async fn lead_text(engine: &Engine, text: &str) -> bool {
    messages(engine)
        .await
        .iter()
        .any(|message| message.author == Author::Lead && message.text.contains(text))
}

// A running session has an ACP id. A queued Worker has a session and no Harness process.
async fn no_harness_process(engine: &Engine) -> bool {
    let mut all = engine
        .store
        .sessions()
        .list("owner", REPOSITORY, 12)
        .await
        .unwrap();
    all.extend(triagers(engine).await);
    all.iter()
        .all(|session| session.acp_session_id.is_none() || session.ended_at.is_some())
}

#[tokio::test]
async fn the_drain_holds_new_agents_waits_for_the_running_work_and_a_cancel_releases_it() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let go = data_dir.path().join("go");
    let check_started = data_dir.path().join("check_started");
    let check_go = data_dir.path().join("check_go");
    let engine = connect(&data_dir, &github, &go).await;
    let mut feed = activity::feed(&engine, None).await.unwrap();
    github.set_check(
        REPOSITORY,
        &format!(
            "touch '{}'\nwhile [ ! -e '{}' ]; do sleep 0.05; done",
            check_started.display(),
            check_go.display()
        ),
    );

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    github.add_label(REPOSITORY, 43, "mobius:ready", "owner");

    // The first Implementer runs a turn and the second waits for the one agent slot.
    wait_for(async || {
        sessions(&engine, "implementer")
            .await
            .iter()
            .any(|session| session.queue_reason.is_some())
            .then_some(())
    })
    .await;

    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the loyalty API")
        .await
        .unwrap();
    wait_for(async || lead_text(&engine, "Hello").await.then_some(())).await;

    let drain = {
        let engine = engine.clone();
        tokio::spawn(async move { drain::start(&engine).await })
    };
    // The drain is on when its first count arrives.
    loop {
        match feed.next().await {
            Some(Live::Drain { .. }) => break,
            Some(_) => continue,
            None => panic!("the live feed closed"),
        }
    }

    // The queued Worker gets the drain reason.
    let queued = wait_for(async || {
        sessions(&engine, "implementer")
            .await
            .into_iter()
            .find(|session| session.queue_reason.as_deref() == Some(DRAIN_REASON))
    })
    .await;

    // A comment on the working task stays an undelivered event: no new Lead turn starts.
    github.add_comment(REPOSITORY, 41, "owner", "One more thing.");
    wait_for(async || {
        (!engine
            .store
            .lead_events()
            .undelivered(REPOSITORY, 12)
            .await
            .unwrap()
            .is_empty())
        .then_some(())
    })
    .await;

    // The Triager does not start: the issue keeps `mobius:ready`.
    github.add_issue(REPOSITORY, 50, "Change request");
    github.add_label(REPOSITORY, 50, "mobius:ready", "owner");

    // A chat Lead turn still runs, and `start_researcher` refuses during the drain.
    chat::send(&engine, "owner", REPOSITORY, 12, "Research the plan flow")
        .await
        .unwrap();
    wait_for(async || lead_text(&engine, DRAIN_REASON).await.then_some(())).await;

    // The running turn ends, and then `.mobius/check` runs: the drain still waits.
    fs::write(&go, "").unwrap();
    wait_for(async || check_started.exists().then_some(())).await;
    assert!(!drain.is_finished());
    fs::write(&check_go, "").unwrap();

    // The work ends: the pull request exists, and the chained Reviewer waits for an agent slot.
    let pull_request = wait_for(async || {
        github
            .pull_requests(REPOSITORY)
            .first()
            .map(|pull_request| pull_request.number)
    })
    .await;
    wait_for(async || {
        sessions(&engine, "reviewer")
            .await
            .iter()
            .any(|session| session.queue_reason.as_deref() == Some(DRAIN_REASON))
            .then_some(())
    })
    .await;

    // A new comment on the pull request does not start the Judge.
    let task = engine
        .store
        .tasks()
        .live(REPOSITORY, 41)
        .await
        .unwrap()
        .unwrap();
    assert!(
        engine
            .store
            .tasks()
            .set_state(task.id, "queued", "reviewed")
            .await
            .unwrap()
    );
    github.add_comment(REPOSITORY, pull_request, "owner", "Rename the field.");

    // The polls see the issue and the comment, but nothing starts.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(sessions(&engine, "judge").await.is_empty());
    assert!(triagers(&engine).await.is_empty());
    assert_eq!(github.labels(REPOSITORY, 50), ["mobius:ready"]);

    // Each Lead saved its memory and closed, so the drain completes.
    assert_eq!(drain.await.unwrap(), DrainEnd::Drained);
    assert!(no_harness_process(&engine).await);
    assert!(
        role_prompts(&engine, "lead_chat")
            .await
            .iter()
            .any(|prompt| prompt == SAVE),
        "The Lead did not save its memory"
    );
    // The held Worker's session shows the drain reason and has no Harness process.
    let second = sessions(&engine, "implementer")
        .await
        .into_iter()
        .find(|session| session.id == queued.id)
        .unwrap();
    assert_eq!(second.queue_reason.as_deref(), Some(DRAIN_REASON));
    assert!(second.acp_session_id.is_none());
    // The live count reached zero, and the drain still shows as on.
    let mut waiting = Vec::new();
    let _ = tokio::time::timeout(Duration::from_millis(300), async {
        while let Some(live) = feed.next().await {
            if let Live::Drain { waiting: count } = live {
                waiting.push(count);
            }
        }
    })
    .await;
    assert_eq!(waiting.last(), Some(&Some(0)));
    assert!(
        waiting
            .iter()
            .any(|count| matches!(count, Some(n) if *n > 0))
    );

    drain::cancel(&engine).await.unwrap();

    // The held Worker starts and finishes.
    wait_for(async || {
        sessions(&engine, "implementer")
            .await
            .iter()
            .filter(|session| session.id == queued.id)
            .any(|session| session.end_reason.as_deref() == Some("done"))
            .then_some(())
    })
    .await;
    let second = sessions(&engine, "implementer")
        .await
        .into_iter()
        .find(|session| session.id == queued.id)
        .unwrap();
    assert_eq!(second.queue_reason, None);
    // The waiting event goes to the Lead.
    wait_for(async || {
        role_prompts(&engine, "lead_chat")
            .await
            .iter()
            .any(|prompt| prompt.contains("One more thing."))
            .then_some(())
    })
    .await;
    // The next poll starts the Judge and the Triager.
    wait_for(async || (!sessions(&engine, "judge").await.is_empty()).then_some(())).await;
    wait_for(async || (!triagers(&engine).await.is_empty()).then_some(())).await;
}

#[tokio::test]
async fn a_triager_that_a_drain_held_during_a_poll_starts_in_the_poll_after_a_cancel() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, &data_dir.path().join("go")).await;
    github.add_issue(REPOSITORY, 50, "Change request");
    let hold = github.hold_issue_events(REPOSITORY, 50);
    github.add_label(REPOSITORY, 50, "mobius:ready", "owner");

    // The poll has read the ready list and waits for the events of the issue.
    hold.reached.notified().await;
    let drain = {
        let engine = engine.clone();
        tokio::spawn(async move { drain::start(&engine).await })
    };
    wait_for(async || drain::waiting(&engine).map(|_| ())).await;
    hold.release.notify_one();

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(triagers(&engine).await.is_empty());
    assert_eq!(github.labels(REPOSITORY, 50), ["mobius:ready"]);

    assert_eq!(drain.await.unwrap(), DrainEnd::Drained);
    drain::cancel(&engine).await.unwrap();
    wait_for(async || (!triagers(&engine).await.is_empty()).then_some(())).await;
}
