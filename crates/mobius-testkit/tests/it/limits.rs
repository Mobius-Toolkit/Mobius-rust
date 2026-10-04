use mobius_domain::{Harness, InboxKind, TranscriptRow};
use mobius_engine::{Engine, github, inbox, limits, workstreams};
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::{install_fake_harness, start, wait_for};
use serde_json::Value;
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};

const REPOSITORY: &str = "owner/shop";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
const LEAD: &str = r#"
[options]
model = ["sonnet", "opus"]
thought_level = ["low", "high"]
mode = ["default", "bypassPermissions"]

[[prompts]]
when = "You are the Reviewer"
shell = "true"

[[prompts]]
when = "dispatch of #41"
call = { tool = "start_implementer", arguments = { n = 41, instructions = "Store plans in cents." } }
"#;
// The first prompt hits the usage limit, and the same prompt again does the work.
const IMPLEMENTER: &str = r#"
[options]
model = ["swe-1.5"]
thought_level = ["high"]

[[prompts]]
error = { code = -32011, message = "Rate limited", data = { retryAfterSeconds = 3600 } }

[[prompts]]
shell = "echo cents > plan.txt && git add plan.txt && git commit -q -m 'Add plan model'"
"#;

async fn connect(data_dir: &TempDir, github: &FakeGitHub) -> Engine {
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", LEAD);
    install_fake_harness(data_dir.path(), FAKE_AGENT, "devin", IMPLEMENTER);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || (!workstreams::list(&engine).await.unwrap().is_empty()).then_some(())).await;
    engine
}

#[tokio::test]
async fn a_usage_limit_pauses_the_harness_until_resume_now_sends_the_prompt_again() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    let before = OffsetDateTime::now_utc();

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    let item = wait_for(async || {
        inbox::list(&engine)
            .await
            .unwrap()
            .into_iter()
            .find(|item| item.kind == InboxKind::UsageLimit)
    })
    .await;
    assert!(
        item.text
            .starts_with("devin reached a usage limit. Mobius sends the prompt again at "),
        "{}",
        item.text
    );
    // Mobius adds the Inbox item before the pause.
    let pause = wait_for(async || {
        engine
            .store
            .harness_pauses()
            .get(Harness::Devin)
            .await
            .unwrap()
    })
    .await;
    assert_eq!(pause.inbox_item, item.id);
    assert!(
        pause.until >= before + Duration::minutes(59),
        "{}",
        pause.until
    );
    assert!(pause.until <= OffsetDateTime::now_utc() + Duration::minutes(61));
    assert!(github.pull_requests(REPOSITORY).is_empty());

    limits::resume(&engine, item.id).await.unwrap();

    wait_for(async || (!github.pull_requests(REPOSITORY).is_empty()).then_some(())).await;
    assert_eq!(
        engine
            .store
            .harness_pauses()
            .get(Harness::Devin)
            .await
            .unwrap(),
        None
    );
    assert!(
        inbox::list(&engine)
            .await
            .unwrap()
            .iter()
            .all(|open| open.id != item.id)
    );
    let session = engine
        .store
        .sessions()
        .list("owner", REPOSITORY, 12)
        .await
        .unwrap()
        .into_iter()
        .find(|session| session.role == "implementer")
        .unwrap();
    let rows: Vec<TranscriptRow> = engine.store.transcript().list(session.id).await.unwrap();
    let prompts: Vec<String> = rows
        .iter()
        .filter(|row| row.kind == "prompt")
        .map(|row| {
            let json: Value = serde_json::from_str(&row.json).unwrap();
            json["text"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    assert_eq!(prompts[0], prompts[1]);
}
