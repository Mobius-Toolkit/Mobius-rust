use mobius_domain::{Session, TranscriptRow};
use mobius_engine::{Engine, chat, github, inbox, workstreams};
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::{git, install_fake_harness, start, wait_for};
use serde_json::Value;
use tempfile::TempDir;

const REPOSITORY: &str = "owner/shop";
const APP: &str = "mobius-test[bot]";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
const CLAUDE: &str = r##"
[options]
model = ["sonnet", "opus"]
thought_level = ["low", "high"]
mode = ["default", "bypassPermissions"]

[[prompts]]
when = "# Owner message"
hang = true

[[prompts]]
when = "You are the Reviewer"
shell = "true"

[[prompts]]
when = "dispatch of #41"
call = { tool = "start_implementer", arguments = { n = 41, instructions = "Store plans in cents." } }
"##;
const IMPLEMENTER: &str = r#"
[options]
model = ["swe-1.5"]
thought_level = ["high"]

[[prompts]]
shell = "echo cents > plan.txt && git add plan.txt && git commit -q -m 'Add plan model'"
"#;
const HANG: &str = r#"
[options]
model = ["swe-1.5"]
thought_level = ["high"]

[[prompts]]
hang = true
"#;

// The feed row of the Workstream comes from the first poll, so later events are new.
async fn connect(data_dir: &TempDir, github: &FakeGitHub, implementer: &str) -> Engine {
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", CLAUDE);
    install_fake_harness(data_dir.path(), FAKE_AGENT, "devin", implementer);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || (!workstreams::list(&engine).await.unwrap().is_empty()).then_some(())).await;
    wait_for(async || (!engine.store.events().latest(1).await.unwrap().is_empty()).then_some(()))
        .await;
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

async fn ended(engine: &Engine, role: &str) -> Session {
    wait_for(async || {
        sessions(engine, role)
            .await
            .into_iter()
            .find(|session| session.ended_at.is_some())
    })
    .await
}

async fn stopped(engine: &Engine, role: &str) -> Session {
    wait_for(async || {
        sessions(engine, role)
            .await
            .into_iter()
            .find(|session| session.end_reason.as_deref() == Some("stopped"))
    })
    .await
}

async fn started(engine: &Engine, role: &str) {
    wait_for(async || {
        sessions(engine, role)
            .await
            .into_iter()
            .find_map(|session| session.acp_session_id)
    })
    .await;
}

fn no_mobius_labels(github: &FakeGitHub, number: i64) {
    let labels = github.labels(REPOSITORY, number);
    assert!(
        !labels.iter().any(|label| label.starts_with("mobius:")),
        "{labels:?}"
    );
}

#[tokio::test]
async fn a_close_stops_the_lead_and_closes_the_pull_requests_and_issues_below() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, IMPLEMENTER).await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || (!inbox::list(&engine).await.unwrap().is_empty()).then_some(())).await;
    github.add_issue(REPOSITORY, 43, "Add plan price");
    github.add_sub_issue(REPOSITORY, 12, 43);
    github.add_issue(REPOSITORY, 44, "Price table");
    github.add_blocker(REPOSITORY, 43, 44);
    github.add_label(REPOSITORY, 43, "mobius:ready", "owner");
    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the next step.")
        .await
        .unwrap();
    started(&engine, "lead_chat").await;

    github.close_issue(REPOSITORY, 12);

    wait_for(async || (github.state(REPOSITORY, 43).0 == "closed").then_some(())).await;
    for number in [41, 43] {
        assert_eq!(
            github.state(REPOSITORY, number),
            ("closed".to_string(), Some("not_planned".to_string()))
        );
        assert!(
            github
                .comments(REPOSITORY, number)
                .contains(&(APP.to_string(), "Workstream closed".to_string()))
        );
        no_mobius_labels(&github, number);
    }
    assert_eq!(github.state(REPOSITORY, 42).0, "closed");
    assert!(
        github
            .comments(REPOSITORY, 42)
            .contains(&(APP.to_string(), "Workstream closed".to_string()))
    );
    assert_eq!(github.state(REPOSITORY, 44).0, "open");
    assert_eq!(
        engine.store.tasks().live(REPOSITORY, 41).await.unwrap(),
        None
    );
    git(&github.remote(REPOSITORY), &["rev-parse", "mobius/41"]);
    assert!(data_dir.path().join("leads/owner/shop/12").exists());
    stopped(&engine, "lead_chat").await;
}

#[tokio::test]
async fn a_completion_stops_the_lead() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, IMPLEMENTER).await;
    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the next step.")
        .await
        .unwrap();
    started(&engine, "lead_chat").await;
    github.close_issue(REPOSITORY, 41);

    workstreams::complete(&engine, REPOSITORY, 12)
        .await
        .unwrap();

    stopped(&engine, "lead_chat").await;
}

#[tokio::test]
async fn a_completion_closes_the_workstream_and_ends_the_live_task() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, IMPLEMENTER).await;
    let hanging_reviewer = CLAUDE
        .replace(
            "when = \"You are the Reviewer\"\nshell = \"true\"",
            "when = \"You are the Reviewer\"\nhang = true",
        )
        .replace(
            "when = \"dispatch of #41\"\n",
            "when = \"dispatch of #41\"\nhang = true\n",
        );
    assert_eq!(hanging_reviewer.matches("hang = true").count(), 3);
    install_fake_harness(
        data_dir.path(),
        FAKE_AGENT,
        "claude-agent-acp",
        &hanging_reviewer,
    );
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    started(&engine, "lead_chat").await;
    started(&engine, "reviewer").await;
    assert!(!workstreams::list(&engine).await.unwrap()[0].all_tasks_closed);

    // The pull request is open and the Reviewer works, so only the completion ends them.
    github.close_issue(REPOSITORY, 41);

    wait_for(async || {
        workstreams::list(&engine).await.unwrap()[0]
            .all_tasks_closed
            .then_some(())
    })
    .await;
    assert!(
        engine
            .store
            .tasks()
            .live(REPOSITORY, 41)
            .await
            .unwrap()
            .is_some()
    );
    workstreams::complete(&engine, REPOSITORY, 12)
        .await
        .unwrap();
    assert_eq!(
        github.state(REPOSITORY, 12),
        ("closed".to_string(), Some("completed".to_string()))
    );
    assert_eq!(
        engine.store.tasks().live(REPOSITORY, 41).await.unwrap(),
        None
    );
    for role in ["reviewer", "lead_chat"] {
        assert_eq!(
            ended(&engine, role).await.end_reason.as_deref(),
            Some("stopped")
        );
    }
}

#[tokio::test]
async fn a_failed_completion_keeps_the_lead_running() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, IMPLEMENTER).await;
    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the next step.")
        .await
        .unwrap();
    started(&engine, "lead_chat").await;
    github.close_issue(REPOSITORY, 41);
    github.fail_close(REPOSITORY, 12);

    let result = workstreams::complete(&engine, REPOSITORY, 12).await;

    assert!(result.is_err());
    assert!(
        sessions(&engine, "lead_chat")
            .await
            .iter()
            .all(|session| session.ended_at.is_none())
    );
}

#[tokio::test]
async fn a_removal_of_the_workstream_label_ends_the_tasks_and_closes_nothing() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, HANG).await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    started(&engine, "implementer").await;

    github.remove_label(REPOSITORY, 12, "mobius:workstream", "mallory");

    let implementer = ended(&engine, "implementer").await;
    assert_eq!(implementer.end_reason.as_deref(), Some("stopped"));
    wait_for(async || {
        engine
            .store
            .tasks()
            .live(REPOSITORY, 41)
            .await
            .unwrap()
            .is_none()
            .then_some(())
    })
    .await;
    wait_for(async || {
        (!github
            .labels(REPOSITORY, 41)
            .contains(&"mobius:working".to_string()))
        .then_some(())
    })
    .await;
    assert_eq!(github.state(REPOSITORY, 41), ("open".to_string(), None));
    assert_eq!(github.state(REPOSITORY, 12), ("open".to_string(), None));
}

#[tokio::test]
async fn a_reopen_starts_a_lead_and_reopens_no_issue() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, IMPLEMENTER).await;
    github.close_issue(REPOSITORY, 12);
    wait_for(async || (github.state(REPOSITORY, 41).0 == "closed").then_some(())).await;

    github.reopen_issue(REPOSITORY, 12);

    let prompt = wait_for(async || {
        for session in sessions(&engine, "lead_chat").await {
            let rows: Vec<TranscriptRow> =
                engine.store.transcript().list(session.id).await.unwrap();
            for row in rows.iter().filter(|row| row.kind == "prompt") {
                let json: Value = serde_json::from_str(&row.json).unwrap();
                if let Some(text) = json["text"].as_str()
                    && text.contains(
                        " reopen of Workstream #12 \"Integrate loyalty plans\" by @owner:",
                    )
                {
                    return Some(text.to_string());
                }
            }
        }
        None
    })
    .await;
    assert!(prompt.contains("# Event\n\n"), "{prompt}");
    assert_eq!(
        github.state(REPOSITORY, 41),
        ("closed".to_string(), Some("not_planned".to_string()))
    );
}
