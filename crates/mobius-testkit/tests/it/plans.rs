use mobius_domain::{Blocker, Live, TaskLine, TranscriptRow};
use mobius_engine::{Engine, activity, github, tasks, workstreams};
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::{install_fake_harness, start, wait_for, wait_for_first_poll};
use serde_json::Value;
use tempfile::TempDir;

const REPOSITORY: &str = "owner/shop";
const APP: &str = "mobius-test[bot]";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
const LEAD: &str = r#"
[options]
model = ["sonnet", "opus"]
thought_level = ["low", "high"]
mode = ["default", "bypassPermissions"]

[[prompts]]
when = "creation of Workstream #12"
call = { tool = "create_issue", arguments = { title = "Add plan model", body = "Plans have a price.", parent = 12, blocked_by = [88] } }

[[prompts]]
when = "creation of Workstream #13"
call = { tool = "mark_ready", arguments = { n = 30 } }
"#;

// The Workstream "Billing" exists before the first poll. Each test adds its Workstream after it, so the Lead gets a creation event.
async fn connect(data_dir: &TempDir, github: &FakeGitHub) -> Engine {
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 20, "Billing");
    github.add_label(REPOSITORY, 20, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 88, "Invoice totals");
    github.add_sub_issue(REPOSITORY, 20, 88);
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", LEAD);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || (!workstreams::list(&engine).await.unwrap().is_empty()).then_some(())).await;
    wait_for_first_poll(&engine, REPOSITORY).await;
    engine
}

async fn lead_replies(engine: &Engine, workstream: i64) -> String {
    let mut replies = String::new();
    for session in engine
        .store
        .sessions()
        .list("owner", REPOSITORY, workstream)
        .await
        .unwrap()
    {
        let rows: Vec<TranscriptRow> = engine.store.transcript().list(session.id).await.unwrap();
        for row in rows.iter().filter(|row| row.kind == "update") {
            let json: Value = serde_json::from_str(&row.json).unwrap();
            if let Some(text) = json["update"]["content"]["text"].as_str() {
                replies.push_str(text);
            }
        }
    }
    replies
}

#[tokio::test]
async fn the_lead_creates_a_sub_issue_with_a_blocker_in_another_workstream() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");

    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");

    wait_for(async || (lead_replies(&engine, 12).await == "Created #89.").then_some(())).await;
    assert_eq!(github.sub_issue_numbers(REPOSITORY, 12), [89]);
    assert_eq!(
        github.issue(REPOSITORY, 89),
        (
            "Add plan model".to_string(),
            "Plans have a price.".to_string()
        )
    );
    assert_eq!(github.blocker_numbers(REPOSITORY, 89), [88]);
    assert!(github.labels(REPOSITORY, 89).is_empty());
    assert_eq!(
        tasks::list(&engine, REPOSITORY, 12).await.unwrap(),
        [TaskLine {
            number: 89,
            title: "Add plan model".to_string(),
            state: "open".to_string(),
            url: "https://github.com/owner/shop/issues/89".to_string(),
            depth: 0,
            blocked_by: vec![Blocker {
                number: 88,
                workstream_title: Some("Billing".to_string()),
            }],
        }]
    );
}

#[tokio::test]
async fn mark_ready_adds_the_ready_label_when_the_workstream_has_no_autopilot() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    github.add_issue(REPOSITORY, 13, "Plan prices");
    github.add_issue(REPOSITORY, 30, "Add plan price");
    github.add_sub_issue(REPOSITORY, 13, 30);

    github.add_label(REPOSITORY, 13, "mobius:workstream", "owner");

    wait_for(async || (lead_replies(&engine, 13).await == "Marked #30 ready.").then_some(())).await;
    assert!(
        github
            .labels(REPOSITORY, 30)
            .contains(&"mobius:ready".to_string())
    );
    let polls = github.not_modified_count();
    wait_for(async || (github.not_modified_count() >= polls + 4).then_some(())).await;
    assert_eq!(
        engine.store.tasks().live(REPOSITORY, 30).await.unwrap(),
        None
    );
    assert!(
        !github
            .labels(REPOSITORY, 13)
            .contains(&"mobius:autopilot".to_string())
    );
}

async fn authorize_owner(engine: &Engine, github: &FakeGitHub) {
    github.add_user_code("user-code", "owner");
    assert!(github::authorize_user(engine, "user-code").await.unwrap());
}

#[tokio::test]
async fn set_autopilot_on_adds_the_label_as_the_owner() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    authorize_owner(&engine, &github).await;
    let mut feed = activity::feed(&engine, None).await.unwrap();

    workstreams::set_autopilot(&engine, REPOSITORY, 20, true)
        .await
        .unwrap();

    assert!(
        github
            .labels(REPOSITORY, 20)
            .contains(&"mobius:autopilot".to_string())
    );
    assert_eq!(
        github
            .label_actor(REPOSITORY, 20, "mobius:autopilot")
            .as_deref(),
        Some("owner")
    );
    let list = workstreams::list(&engine).await.unwrap();
    assert!(
        list.iter()
            .any(|workstream| workstream.number == 20 && workstream.autopilot)
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !matches!(feed.next().await, Some(Live::Workstreams)) {}
    })
    .await
    .unwrap();
}

// GitHub records no `labeled` event when the issue already has the label, so an
// add alone would keep the App bot as the last actor. The switch removes first.
#[tokio::test]
async fn set_autopilot_on_replaces_a_label_of_the_app() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    authorize_owner(&engine, &github).await;
    github.add_label(REPOSITORY, 20, "mobius:autopilot", APP);

    workstreams::set_autopilot(&engine, REPOSITORY, 20, true)
        .await
        .unwrap();

    assert_eq!(
        github
            .label_actor(REPOSITORY, 20, "mobius:autopilot")
            .as_deref(),
        Some("owner")
    );
    let list = workstreams::list(&engine).await.unwrap();
    assert!(
        list.iter()
            .any(|workstream| workstream.number == 20 && workstream.autopilot)
    );
}

#[tokio::test]
async fn set_autopilot_off_removes_the_label() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    authorize_owner(&engine, &github).await;
    github.add_label(REPOSITORY, 20, "mobius:autopilot", "owner");

    workstreams::set_autopilot(&engine, REPOSITORY, 20, false)
        .await
        .unwrap();

    assert!(
        !github
            .labels(REPOSITORY, 20)
            .contains(&"mobius:autopilot".to_string())
    );
    let list = workstreams::list(&engine).await.unwrap();
    assert!(
        list.iter()
            .all(|workstream| workstream.number != 20 || !workstream.autopilot)
    );
}

#[tokio::test]
async fn set_autopilot_needs_an_authorized_owner() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;

    let error = workstreams::set_autopilot(&engine, REPOSITORY, 20, true)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("authorize the Mobius App"));
    assert!(
        !github
            .labels(REPOSITORY, 20)
            .contains(&"mobius:autopilot".to_string())
    );
}
