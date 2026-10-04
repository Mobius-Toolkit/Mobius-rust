use mobius_engine::{Engine, github, workstreams};
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::{install_fake_harness, start_with_config, wait_for};
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
reply = ["Seen"]

[[prompts]]
reply = ["Seen"]

[[prompts]]
reply = ["Seen"]
"#;

const DECLINING_LEAD: &str = r#"
[options]
model = ["sonnet", "opus"]
thought_level = ["low", "high"]
mode = ["default", "bypassPermissions"]

[[prompts]]
call = { tool = "decline", arguments = { n = 41, reason = "Split it into a model and an API." } }
"#;

// Workstream #12 exists before the first poll, and the sub-issues of the test too.
fn prepare(github: &FakeGitHub, autopilot: bool) {
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    if autopilot {
        github.add_label(REPOSITORY, 12, "mobius:autopilot", "owner");
    }
}

async fn connect(data_dir: &TempDir, github: &FakeGitHub, extra_config: &str) -> Engine {
    connect_with_lead(data_dir, github, extra_config, LEAD).await
}

async fn connect_with_lead(
    data_dir: &TempDir,
    github: &FakeGitHub,
    extra_config: &str,
    lead: &str,
) -> Engine {
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", lead);
    let engine =
        start_with_config(data_dir.path(), "correct horse", &github.url, extra_config).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || (!workstreams::list(&engine).await.unwrap().is_empty()).then_some(())).await;
    engine
}

fn add_task_issue(github: &FakeGitHub, number: i64, title: &str) {
    github.add_issue(REPOSITORY, number, title);
    github.add_sub_issue(REPOSITORY, 12, number);
}

async fn started(engine: &Engine, issue: i64) -> i64 {
    wait_for(async || {
        engine
            .store
            .tasks()
            .live(REPOSITORY, issue)
            .await
            .unwrap()
            .map(|task| task.id)
    })
    .await
}

// Gives the Mobius poll at least one full pass after the call.
async fn wait_for_polls(github: &FakeGitHub) {
    let before = github.not_modified_count();
    wait_for(async || (github.not_modified_count() >= before + 4).then_some(())).await;
}

#[tokio::test]
async fn with_autopilot_a_free_worker_and_no_blocker_a_task_starts() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    prepare(&github, true);
    add_task_issue(&github, 41, "Add plan model");
    let engine = connect(&data_dir, &github, "").await;

    started(&engine, 41).await;

    let task = engine
        .store
        .tasks()
        .live(REPOSITORY, 41)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.workstream, 12);
    assert!(
        github
            .labels(REPOSITORY, 41)
            .contains(&"mobius:working".to_string())
    );
    let dispatched = wait_for(async || {
        engine
            .store
            .events()
            .latest(100)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.text == "Dispatched \"Add plan model\"")
    })
    .await;
    assert_eq!(dispatched.actor, APP);
}

#[tokio::test]
async fn with_no_autopilot_no_task_starts() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    prepare(&github, false);
    add_task_issue(&github, 41, "Add plan model");
    let engine = connect(&data_dir, &github, "").await;

    wait_for_polls(&github).await;

    assert_eq!(
        engine.store.tasks().live(REPOSITORY, 41).await.unwrap(),
        None
    );
    assert_eq!(engine.store.tasks().active_count().await.unwrap(), 0);
}

#[tokio::test]
async fn with_as_many_active_tasks_as_max_agents_no_task_starts() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    prepare(&github, true);
    add_task_issue(&github, 41, "Add plan model");
    add_task_issue(&github, 42, "Add plan price");
    let engine = connect(&data_dir, &github, "max_agents = 1").await;

    started(&engine, 41).await;
    wait_for_polls(&github).await;

    assert_eq!(
        engine.store.tasks().live(REPOSITORY, 42).await.unwrap(),
        None
    );
    assert_eq!(engine.store.tasks().active_count().await.unwrap(), 1);
}

#[tokio::test]
async fn an_open_blocker_holds_the_task_until_it_closes() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    prepare(&github, true);
    add_task_issue(&github, 41, "Add plan model");
    github.add_issue(REPOSITORY, 88, "Invoice totals");
    github.add_blocker(REPOSITORY, 41, 88);
    let engine = connect(&data_dir, &github, "").await;

    wait_for_polls(&github).await;

    assert_eq!(
        engine.store.tasks().live(REPOSITORY, 41).await.unwrap(),
        None
    );

    github.close_issue(REPOSITORY, 88);

    started(&engine, 41).await;
}

#[tokio::test]
async fn tasks_start_in_the_order_of_the_sub_issues() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    prepare(&github, true);
    for (number, title) in [
        (41, "Add plan model"),
        (42, "Add plan price"),
        (43, "Add plan name"),
    ] {
        github.add_issue(REPOSITORY, number, title);
    }
    for number in [43, 41, 42] {
        github.add_sub_issue(REPOSITORY, 12, number);
    }
    let engine = connect(&data_dir, &github, "").await;

    let first = started(&engine, 43).await;
    let second = started(&engine, 41).await;
    let third = started(&engine, 42).await;

    assert!(first < second && second < third);
}

#[tokio::test]
async fn a_declined_task_does_not_start_again() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    prepare(&github, true);
    add_task_issue(&github, 41, "Add plan model");
    let engine = connect_with_lead(&data_dir, &github, "", DECLINING_LEAD).await;

    wait_for(async || {
        (github.comments(REPOSITORY, 41).len() == 1
            && engine
                .store
                .tasks()
                .live(REPOSITORY, 41)
                .await
                .unwrap()
                .is_none())
        .then_some(())
    })
    .await;
    wait_for_polls(&github).await;

    assert_eq!(github.comments(REPOSITORY, 41).len(), 1);
    assert_eq!(
        engine.store.tasks().live(REPOSITORY, 41).await.unwrap(),
        None
    );
    assert_eq!(engine.store.tasks().active_count().await.unwrap(), 0);
}
