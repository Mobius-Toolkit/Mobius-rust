use mobius_domain::{Session, TranscriptRow};
use mobius_engine::{Engine, github, inbox, workstreams};
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::{git, install_fake_harness, start_with_config, wait_for};
use serde_json::Value;
use tempfile::TempDir;

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

async fn connect(data_dir: &TempDir, github: &FakeGitHub, implementer: &str) -> Engine {
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", LEAD);
    install_fake_harness(data_dir.path(), FAKE_AGENT, "devin", implementer);
    let engine = start_with_config(data_dir.path(), "correct horse", &github.url, "").await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || (!workstreams::list(&engine).await.unwrap().is_empty()).then_some(())).await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
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

async fn lead_prompts(engine: &Engine) -> Vec<String> {
    let mut all = Vec::new();
    for session in sessions(engine, "lead_chat").await {
        let rows: Vec<TranscriptRow> = engine.store.transcript().list(session.id).await.unwrap();
        all.extend(
            rows.iter()
                .filter(|row| row.kind == "prompt")
                .filter_map(|row| {
                    let json: Value = serde_json::from_str(&row.json).unwrap();
                    json["text"].as_str().map(str::to_string)
                }),
        );
    }
    all
}

async fn task_state(engine: &Engine) -> Option<String> {
    engine
        .store
        .tasks()
        .live(REPOSITORY, 41)
        .await
        .unwrap()
        .map(|task| task.state)
}

// Mobius adds the Inbox item after the state change.
async fn ready_for_review(engine: &Engine) {
    wait_for(async || {
        (task_state(engine).await.as_deref() == Some("ready_for_review")
            && !inbox::list(engine).await.unwrap().is_empty())
        .then_some(())
    })
    .await;
}

// Mobius removes the labels after it changes the state of the task.
async fn no_mobius_labels(github: &FakeGitHub) {
    wait_for(async || {
        (!github
            .labels(REPOSITORY, 41)
            .iter()
            .any(|label| label == "mobius:working" || label == "mobius:needs-human"))
        .then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_merge_ends_the_task_and_keeps_the_branch() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, IMPLEMENTER).await;
    ready_for_review(&engine).await;
    let worktree = data_dir.path().join("worktrees/owner/shop/task-41");
    assert!(worktree.exists());

    github.merge_pull_request(REPOSITORY, 42);

    wait_for(async || task_state(&engine).await.is_none().then_some(())).await;
    no_mobius_labels(&github).await;
    assert!(!worktree.exists());
    git(&github.remote(REPOSITORY), &["rev-parse", "mobius/41"]);
    wait_for(async || {
        lead_prompts(&engine)
            .await
            .iter()
            .any(|prompt| {
                prompt.contains(" end of #41 \"Add plan model\": pull request #42 merged.")
            })
            .then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_merge_closes_the_open_issue_as_completed() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, IMPLEMENTER).await;
    ready_for_review(&engine).await;

    github.merge_pull_request(REPOSITORY, 42);

    wait_for(async || task_state(&engine).await.is_none().then_some(())).await;
    assert_eq!(
        github.state(REPOSITORY, 41),
        ("closed".to_string(), Some("completed".to_string()))
    );
}

#[tokio::test]
async fn a_close_of_the_issue_before_a_pull_request_stops_the_implementer_and_cancels_the_task() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, HANG).await;
    wait_for(async || {
        sessions(&engine, "implementer")
            .await
            .first()
            .and_then(|session| session.acp_session_id.as_ref().map(|_| ()))
    })
    .await;

    github.close_issue(REPOSITORY, 41);

    wait_for(async || task_state(&engine).await.is_none().then_some(())).await;
    let session = wait_for(async || {
        sessions(&engine, "implementer")
            .await
            .into_iter()
            .find(|session| session.ended_at.is_some())
    })
    .await;
    assert_eq!(session.end_reason.as_deref(), Some("stopped"));
    no_mobius_labels(&github).await;
    assert!(
        !data_dir
            .path()
            .join("worktrees/owner/shop/task-41")
            .exists()
    );
    assert!(github.pull_requests(REPOSITORY).is_empty());
}

#[tokio::test]
async fn a_removal_of_the_working_label_stops_the_task() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, IMPLEMENTER).await;
    ready_for_review(&engine).await;
    let inbox_items = inbox::list(&engine).await.unwrap().len();

    github.remove_label(REPOSITORY, 41, "mobius:working", "mallory");

    let rows = wait_for(async || {
        let rows = engine.store.events().latest(100).await.unwrap();
        rows.last()
            .is_some_and(|row| row.text.starts_with("Stopped"))
            .then_some(rows)
    })
    .await;
    assert_eq!(task_state(&engine).await.as_deref(), Some("stopped"));
    let remote = github.remote(REPOSITORY);
    let head = git(&remote, &["rev-parse", "mobius/41"]);
    let check_runs = github.check_runs(REPOSITORY);
    assert_eq!(check_runs.len(), 2);
    assert_eq!(check_runs[1].head_sha, head);
    assert_eq!(check_runs[1].conclusion.as_deref(), Some("failure"));
    assert_eq!(
        check_runs[1].output.as_ref().unwrap().summary,
        "Stopped by a label removal."
    );
    no_mobius_labels(&github).await;
    assert!(
        !data_dir
            .path()
            .join("worktrees/owner/shop/task-41")
            .exists()
    );
    assert_eq!(github.pull_requests(REPOSITORY).len(), 1);
    assert_eq!(inbox::list(&engine).await.unwrap().len(), inbox_items);
    let last = rows.last().unwrap();
    assert_eq!(last.actor, "mallory");
    assert_eq!(last.issue, 41);
    assert_eq!(
        last.text,
        "Stopped \"Add plan model\" after a removal of mobius:working"
    );
    assert_eq!(last.link, "https://github.com/owner/shop/issues/41");
}

#[tokio::test]
async fn a_ready_label_after_a_stop_starts_a_new_task_on_a_new_branch() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, IMPLEMENTER).await;
    ready_for_review(&engine).await;
    github.remove_label(REPOSITORY, 41, "mobius:working", "mallory");
    wait_for(async || (task_state(&engine).await.as_deref() == Some("stopped")).then_some(()))
        .await;

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    let pull_requests =
        wait_for(async || Some(github.pull_requests(REPOSITORY)).filter(|all| all.len() == 2))
            .await;
    assert_eq!(pull_requests[1].head, "mobius/41-2");
    let task = engine
        .store
        .tasks()
        .live(REPOSITORY, 41)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.branch.as_deref(), Some("mobius/41-2"));
    assert_eq!(github.state(REPOSITORY, 42).0, "open");
}
