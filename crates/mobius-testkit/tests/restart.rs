use std::fs;

use mobius_domain::{Harness, InboxKind, Session, TranscriptRow};
use mobius_engine::{Engine, github, inbox, workstreams};
use mobius_store::{NewSession, Store};
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::{install_fake_harness, start_with_config, wait_for};
use serde_json::{Value, json};
use tempfile::TempDir;

const REPOSITORY: &str = "owner/shop";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
const PROMPT: &str = "You are the Implementer of one task. Store plans in cents.";
const LEAD: &str = r#"
[options]
model = ["sonnet", "opus"]
thought_level = ["low", "high"]
mode = ["default", "bypassPermissions"]

[[prompts]]
when = "You are the Reviewer"
shell = "true"

[[prompts]]
when = "You are the Judge"
call = { tool = "submit_verdicts", arguments = { items = [
    { item = 1, actions = [{ verdict = "reject", text = "The API needs this name." }] },
] } }
"#;
const IMPLEMENTER: &str = r#"
[options]
model = ["swe-1.5"]
thought_level = ["high"]

[[prompts]]
shell = "echo cents > plan.txt && git add plan.txt && git commit -q -m 'Add plan model'"
"#;

fn add_issues(github: &FakeGitHub) {
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);
    github.add_label(REPOSITORY, 41, "mobius:working", "mobius-test[bot]");
}

async fn connect(data_dir: &TempDir, github: &FakeGitHub) -> Engine {
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", LEAD);
    install_fake_harness(data_dir.path(), FAKE_AGENT, "devin", IMPLEMENTER);
    let engine = start_with_config(
        data_dir.path(),
        "correct horse",
        &github.url,
        "review_quiet_period = \"200ms\"",
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

async fn prompts(engine: &Engine, session: i64) -> Vec<String> {
    let rows: Vec<TranscriptRow> = engine.store.transcript().list(session).await.unwrap();
    rows.iter()
        .filter(|row| row.kind == "prompt")
        .filter_map(|row| {
            let json: Value = serde_json::from_str(&row.json).unwrap();
            json["text"].as_str().map(str::to_string)
        })
        .collect()
}

#[tokio::test]
async fn a_restart_starts_the_implementer_again_and_the_lead_session() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    add_issues(&github);
    // The store of a server that stopped during a turn of the Implementer.
    let (old, lead) = {
        let store = Store::open(data_dir.path()).await.unwrap();
        let task = store.tasks().add(REPOSITORY, 41, 12).await.unwrap();
        store.tasks().queue(task.id, "dispatched").await.unwrap();
        store
            .tasks()
            .set_state(task.id, "queued", "working")
            .await
            .unwrap();
        let lead = store
            .sessions()
            .add(NewSession {
                role: "lead_event",
                harness: Harness::ClaudeCode,
                model: "sonnet",
                organization: "owner",
                repository: REPOSITORY,
                workstream: 12,
                issue: None,
                parent: None,
            })
            .await
            .unwrap();
        let session = store
            .sessions()
            .add(NewSession {
                role: "implementer",
                harness: Harness::Devin,
                model: "swe-1.5",
                organization: "owner",
                repository: REPOSITORY,
                workstream: 12,
                issue: Some(41),
                parent: Some(lead.id),
            })
            .await
            .unwrap();
        store
            .tasks()
            .set_worker(task.id, "implementer", Some(PROMPT))
            .await
            .unwrap();
        store
            .transcript()
            .add(session.id, "prompt", &json!({ "text": PROMPT }).to_string())
            .await
            .unwrap();
        store
            .lead_events()
            .add(
                "owner",
                REPOSITORY,
                12,
                Some(41),
                "comment",
                "A comment before the restart.",
            )
            .await
            .unwrap();
        (session.id, lead.id)
    };
    let scratch = data_dir.path().join(format!("scratch/{old}"));
    fs::create_dir_all(&scratch).unwrap();

    let engine = connect(&data_dir, &github).await;

    wait_for(async || (!github.pull_requests(REPOSITORY).is_empty()).then_some(())).await;
    let implementers = sessions(&engine, "implementer").await;
    assert_eq!(implementers.len(), 2);
    assert_eq!(implementers[0].id, old);
    assert_eq!(implementers[0].end_reason.as_deref(), Some("restart"));
    assert_eq!(implementers[1].parent, Some(lead));
    assert_eq!(prompts(&engine, implementers[1].id).await, [PROMPT]);
    assert!(!scratch.exists());
    wait_for(async || {
        for session in sessions(&engine, "lead_chat").await {
            if prompts(&engine, session.id)
                .await
                .iter()
                .any(|prompt| prompt.contains("A comment before the restart."))
            {
                return Some(());
            }
        }
        None
    })
    .await;
}

#[tokio::test]
async fn a_start_with_an_empty_store_hands_a_working_issue_to_a_human() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    add_issues(&github);

    let engine = connect(&data_dir, &github).await;

    let item = wait_for(async || {
        inbox::list(&engine)
            .await
            .unwrap()
            .into_iter()
            .find(|item| item.kind == InboxKind::Stopped)
    })
    .await;
    assert_eq!(
        item.text,
        "Mobius lost the state of this task. Add mobius:ready to start again."
    );
    assert_eq!(item.issue, 41);
    assert_eq!(item.workstream, 12);
    assert_eq!(item.link, "https://github.com/owner/shop/issues/41");
    // The Inbox item comes before the label change.
    wait_for(async || {
        (github.labels(REPOSITORY, 41) == ["mobius:needs-human".to_string()]).then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_restart_starts_the_judge_again_below_the_parent_of_the_ended_judge() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    add_issues(&github);
    github.push_commit(REPOSITORY, "mobius/41", "Add plan model");
    let pull_request = github.open_pull_request(REPOSITORY, "Add plan model", "mobius/41");
    github.add_review_comment(REPOSITORY, pull_request, None, "owner", "Use price_cents.");
    // The store of a server that stopped during a turn of the Judge.
    let (old, implementer) = {
        let store = Store::open(data_dir.path()).await.unwrap();
        let task = store.tasks().add(REPOSITORY, 41, 12).await.unwrap();
        store.tasks().queue(task.id, "dispatched").await.unwrap();
        store
            .tasks()
            .set_state(task.id, "queued", "working")
            .await
            .unwrap();
        store
            .tasks()
            .set_branch(task.id, "mobius/41")
            .await
            .unwrap();
        store
            .tasks()
            .set_pull_request(task.id, pull_request)
            .await
            .unwrap();
        store
            .tasks()
            .set_worker(task.id, "judge", Some("reviewed"))
            .await
            .unwrap();
        let lead = store
            .sessions()
            .add(NewSession {
                role: "lead_event",
                harness: Harness::ClaudeCode,
                model: "sonnet",
                organization: "owner",
                repository: REPOSITORY,
                workstream: 12,
                issue: None,
                parent: None,
            })
            .await
            .unwrap();
        let implementer = store
            .sessions()
            .add(NewSession {
                role: "implementer",
                harness: Harness::Devin,
                model: "swe-1.5",
                organization: "owner",
                repository: REPOSITORY,
                workstream: 12,
                issue: Some(41),
                parent: Some(lead.id),
            })
            .await
            .unwrap();
        let judge = store
            .sessions()
            .add(NewSession {
                role: "judge",
                harness: Harness::ClaudeCode,
                model: "sonnet",
                organization: "owner",
                repository: REPOSITORY,
                workstream: 12,
                issue: Some(41),
                parent: Some(implementer.id),
            })
            .await
            .unwrap();
        (judge.id, implementer.id)
    };

    let engine = connect(&data_dir, &github).await;

    let judges = wait_for(async || {
        let judges = sessions(&engine, "judge").await;
        (judges.len() == 2).then_some(judges)
    })
    .await;
    assert_eq!(judges[0].id, old);
    assert_eq!(judges[0].end_reason.as_deref(), Some("restart"));
    assert_eq!(judges[1].parent, Some(implementer));
}
