use std::fs;

use mobius_domain::Session;
use mobius_engine::{Engine, github, workstreams};
use mobius_testkit::fake_github::{CheckRun, CheckRunOutput, FakeGitHub};
use mobius_testkit::{git, install_fake_harness, start_with_config, wait_for};
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};

const REPOSITORY: &str = "owner/shop";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
// The prompt of a Lead holds the earlier events, so the rule of the newest dispatch comes first.
const CLAUDE: &str = r#"
[options]
model = ["sonnet", "opus", "haiku"]
thought_level = ["low", "high"]
mode = ["default", "bypassPermissions"]

[[prompts]]
when = "You are the Reviewer"
shell = "true"

[[prompts]]
when = "dispatch of #45"
call = { tool = "start_implementer", arguments = { n = 45, instructions = "Add a price page." } }

[[prompts]]
when = "dispatch of #43"
call = { tool = "start_implementer", arguments = { n = 43, instructions = "Round prices down." } }

[[prompts]]
when = "dispatch of #41"
call = { tool = "start_implementer", arguments = { n = 41, instructions = "Store plans in cents." } }
"#;
const IMPLEMENTER: &str = r#"
[options]
model = ["swe-1.5"]
thought_level = ["high"]

[[prompts]]
when = "Action: fix"
shell = "echo 'cents per month' > plan.txt && git commit -q -am 'Fix the check'"

[[prompts]]
shell = "echo cents > plan.txt && git add plan.txt && git commit -q -m 'Add plan model'"
"#;
const IMPLEMENTER_WITH_NO_FIX: &str = r#"
[options]
model = ["swe-1.5"]
thought_level = ["high"]

[[prompts]]
when = "Action: fix"
shell = "true"

[[prompts]]
shell = "echo cents > plan.txt && git add plan.txt && git commit -q -m 'Add plan model'"
"#;

async fn connect(data_dir: &TempDir, github: &FakeGitHub, extra_config: &str) -> Engine {
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    for number in [41, 43, 45] {
        github.add_issue(REPOSITORY, number, &format!("Task {number}"));
        github.add_sub_issue(REPOSITORY, 12, number);
    }
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", CLAUDE);
    install_fake_harness(data_dir.path(), FAKE_AGENT, "devin", IMPLEMENTER);
    let engine =
        start_with_config(data_dir.path(), "correct horse", &github.url, extra_config).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || (!workstreams::list(&engine).await.unwrap().is_empty()).then_some(())).await;
    engine
}

async fn task_state(engine: &Engine, issue: i64) -> Option<String> {
    engine
        .store
        .tasks()
        .live(REPOSITORY, issue)
        .await
        .unwrap()
        .map(|task| task.state)
}

async fn wait_for_state(engine: &Engine, issue: i64, state: &str) {
    wait_for(async || (task_state(engine, issue).await.as_deref() == Some(state)).then_some(()))
        .await;
}

async fn sessions(engine: &Engine, issue: i64) -> Vec<Session> {
    engine
        .store
        .sessions()
        .list("owner", REPOSITORY, 12)
        .await
        .unwrap()
        .into_iter()
        .filter(|session| session.role == "implementer" && session.issue == Some(issue))
        .collect()
}

// Gives the head of the branch of the issue and the number of its pull request.
async fn ready_for_review(engine: &Engine, github: &FakeGitHub, issue: i64) -> (String, i64) {
    github.add_label(REPOSITORY, issue, "mobius:ready", "owner");
    wait_for_state(engine, issue, "ready_for_review").await;
    let head = git(
        &github.remote(REPOSITORY),
        &["rev-parse", &format!("mobius/{issue}")],
    );
    let pull_request = github
        .pull_requests(REPOSITORY)
        .into_iter()
        .find(|pull_request| pull_request.head == format!("mobius/{issue}"))
        .unwrap();
    (head, pull_request.number)
}

fn failed_check_run(head: &str) -> CheckRun {
    CheckRun {
        name: "build".to_string(),
        head_sha: head.to_string(),
        status: "completed".to_string(),
        conclusion: Some("failure".to_string()),
        output: Some(CheckRunOutput {
            title: "build title".to_string(),
            summary: "build summary".to_string(),
        }),
    }
}

#[tokio::test]
async fn a_free_slot_goes_to_the_fix_round_of_an_old_pull_request_before_a_new_ticket() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, "max_agents = 1").await;
    let (head, pull_request) = ready_for_review(&engine, &github, 41).await;
    github.set_created_at(REPOSITORY, pull_request, 1000);
    let go = data_dir.path().join("go");
    github.set_check(
        REPOSITORY,
        &format!("while [ ! -e '{}' ]; do sleep 0.05; done", go.display()),
    );
    github.add_label(REPOSITORY, 43, "mobius:ready", "owner");
    wait_for_state(&engine, 43, "working").await;
    github.add_label(REPOSITORY, 45, "mobius:ready", "owner");
    wait_for_state(&engine, 45, "queued").await;

    github.add_check_run(REPOSITORY, failed_check_run(&head));

    wait_for_state(&engine, 41, "queued").await;
    let ticket = wait_for(async || {
        sessions(&engine, 45).await.into_iter().find(|session| {
            session
                .queue_reason
                .as_deref()
                .is_some_and(|reason| reason.starts_with("an open"))
        })
    })
    .await;
    assert_eq!(
        ticket.queue_reason.as_deref(),
        Some(format!("an open pull request has agent work ({REPOSITORY}#{pull_request})").as_str())
    );

    fs::write(&go, "").unwrap();

    wait_for_state(&engine, 41, "working").await;
    assert_eq!(task_state(&engine, 45).await.as_deref(), Some("queued"));
    wait_for_state(&engine, 45, "ready_for_review").await;
    let round = &sessions(&engine, 41).await[1];
    let ticket = &sessions(&engine, 45).await[0];
    assert!(ticket.started_at >= round.ended_at.unwrap());
    assert_ne!(
        git(&github.remote(REPOSITORY), &["rev-parse", "mobius/41"]),
        head
    );
}

#[tokio::test]
async fn a_pull_request_that_waits_for_the_owner_does_not_stop_a_new_ticket() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, "max_agents = 1").await;
    ready_for_review(&engine, &github, 41).await;

    github.add_label(REPOSITORY, 43, "mobius:ready", "owner");

    wait_for_state(&engine, 43, "ready_for_review").await;
    assert_eq!(
        task_state(&engine, 41).await.as_deref(),
        Some("ready_for_review")
    );
}

#[tokio::test]
async fn a_pull_request_in_needs_human_does_not_stop_a_new_ticket() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, "max_agents = 1").await;
    let (_, pull_request) = ready_for_review(&engine, &github, 41).await;
    github.set_created_at(REPOSITORY, pull_request, 0);
    github.set_behind(REPOSITORY, pull_request);
    github.commit_file(REPOSITORY, "price.txt", "dollars\n", "Add price");
    wait_for_state(&engine, 41, "needs_human").await;

    github.add_label(REPOSITORY, 43, "mobius:ready", "owner");

    wait_for_state(&engine, 43, "ready_for_review").await;
    assert_eq!(
        task_state(&engine, 41).await.as_deref(),
        Some("needs_human")
    );
}

#[tokio::test]
async fn a_judge_that_runs_from_needs_human_holds_a_new_ticket() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(
        &data_dir,
        &github,
        "max_agents = 2\nreview_quiet_period = \"200ms\"",
    )
    .await;
    install_fake_harness(
        data_dir.path(),
        FAKE_AGENT,
        "claude-agent-acp",
        &format!("[[prompts]]\nwhen = \"You are the Judge\"\nhang = true\n{CLAUDE}"),
    );
    let (_, pull_request) = ready_for_review(&engine, &github, 41).await;
    github.set_created_at(REPOSITORY, pull_request, 0);
    github.set_behind(REPOSITORY, pull_request);
    github.commit_file(REPOSITORY, "price.txt", "dollars\n", "Add price");
    wait_for_state(&engine, 41, "needs_human").await;
    github.add_comment(REPOSITORY, pull_request, "owner", "Continue.");
    wait_for_state(&engine, 41, "working").await;

    github.add_label(REPOSITORY, 43, "mobius:ready", "owner");

    let ticket = wait_for(async || {
        sessions(&engine, 43)
            .await
            .into_iter()
            .find_map(|session| session.queue_reason)
    })
    .await;
    assert_eq!(
        ticket,
        format!("an open pull request has agent work ({REPOSITORY}#{pull_request})")
    );
}

#[tokio::test]
async fn a_failed_check_on_a_head_that_got_its_fix_round_does_not_stop_a_new_ticket() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, "max_agents = 1").await;
    install_fake_harness(
        data_dir.path(),
        FAKE_AGENT,
        "devin",
        IMPLEMENTER_WITH_NO_FIX,
    );
    let (head, _) = ready_for_review(&engine, &github, 41).await;
    github.add_check_run(REPOSITORY, failed_check_run(&head));
    wait_for(async || (sessions(&engine, 41).await.len() == 2).then_some(())).await;
    wait_for_state(&engine, 41, "ready_for_review").await;

    github.add_label(REPOSITORY, 43, "mobius:ready", "owner");

    wait_for_state(&engine, 43, "ready_for_review").await;
    assert_eq!(sessions(&engine, 41).await.len(), 2);
}

#[tokio::test]
async fn a_reviewed_pull_request_with_an_open_thread_does_not_stop_a_new_ticket() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, "max_agents = 1").await;
    let (_, pull_request) = ready_for_review(&engine, &github, 41).await;
    github.set_created_at(REPOSITORY, pull_request, 0);
    let task = engine
        .store
        .tasks()
        .live(REPOSITORY, 41)
        .await
        .unwrap()
        .unwrap();
    engine
        .store
        .tasks()
        .set_judged_at(task.id, OffsetDateTime::now_utc() + Duration::days(1))
        .await
        .unwrap();
    github.add_review_comment(REPOSITORY, pull_request, None, "owner", "Use cents.");
    assert!(
        engine
            .store
            .tasks()
            .set_state(task.id, "ready_for_review", "reviewed")
            .await
            .unwrap()
    );
    github.set_behind(REPOSITORY, pull_request);
    github.commit_file(REPOSITORY, "price.txt", "dollars\n", "Add price");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    github.add_label(REPOSITORY, 43, "mobius:ready", "owner");

    wait_for_state(&engine, 43, "ready_for_review").await;
    assert_eq!(task_state(&engine, 41).await.as_deref(), Some("reviewed"));
}
