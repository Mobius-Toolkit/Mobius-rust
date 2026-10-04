use std::fs;
use std::time::Duration;

use mobius_domain::{InboxKind, Session, TranscriptRow};
use mobius_engine::config::Config;
use mobius_engine::{Engine, github, inbox, workstreams};
use mobius_testkit::fake_github::{APP_SLUG, FakeGitHub};
use mobius_testkit::{install_fake_harness, start_with, wait_for};
use serde_json::Value;
use tempfile::TempDir;
use time::OffsetDateTime;

const REPOSITORY: &str = "owner/shop";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
const CLAUDE_OPTIONS: &str = r#"
[options]
model = ["sonnet", "opus"]
thought_level = ["low", "high"]
mode = ["default", "bypassPermissions"]

[[prompts]]
when = "You are the Reviewer"
shell = "true"
"#;
const DEVIN_OPTIONS: &str = r#"
[options]
model = ["swe-1.5"]
thought_level = ["high"]
"#;
const START: &str = "call = { tool = \"start_implementer\", arguments = { n = 41, instructions = \"Store plans in cents.\" } }";
const COMMIT: &str =
    "echo cents > plan.txt && git add plan.txt && git commit -q -m 'Add plan model'";

// The shell of `fake-agent` ends its own process, as a Harness that dies in a turn. With a flag file, only the first process dies.
fn die_once(flag: &std::path::Path, then: &str) -> String {
    format!(
        "if [ -e '{0}' ]; then {then}; else touch '{0}'; kill -9 $PPID; fi",
        flag.display()
    )
}

async fn connect(
    data_dir: &TempDir,
    github: &FakeGitHub,
    lead: &str,
    implementer: &str,
    extra_config: &str,
) -> Engine {
    connect_with(data_dir, github, lead, implementer, extra_config, |_| {}).await
}

async fn connect_with(
    data_dir: &TempDir,
    github: &FakeGitHub,
    lead: &str,
    implementer: &str,
    extra_config: &str,
    adjust: impl FnOnce(&mut Config),
) -> Engine {
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);
    install_fake_harness(
        data_dir.path(),
        FAKE_AGENT,
        "claude-agent-acp",
        &format!("{CLAUDE_OPTIONS}\n{lead}"),
    );
    install_fake_harness(
        data_dir.path(),
        FAKE_AGENT,
        "devin",
        &format!("{DEVIN_OPTIONS}\n{implementer}"),
    );
    let engine = start_with(
        data_dir.path(),
        "correct horse",
        &github.url,
        extra_config,
        adjust,
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

fn dispatch_start() -> String {
    format!("[[prompts]]\nwhen = \"dispatch of #41\"\n{START}\n")
}

#[tokio::test]
async fn a_worker_that_dies_starts_again_and_does_the_work() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let flag = data_dir.path().join("died");
    let implementer = format!(
        "[[prompts]]\nshell = \"{}\"\n",
        die_once(&flag, COMMIT).replace('"', "\\\"")
    );
    let engine = connect(&data_dir, &github, &dispatch_start(), &implementer, "").await;

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    wait_for(async || (!github.pull_requests(REPOSITORY).is_empty()).then_some(())).await;
    let implementers = wait_for(async || {
        let implementers = sessions(&engine, "implementer").await;
        implementers
            .iter()
            .all(|session| session.ended_at.is_some())
            .then_some(implementers)
    })
    .await;
    assert_eq!(implementers.len(), 2);
    assert_eq!(implementers[0].end_reason.as_deref(), Some("failed"));
    assert_eq!(implementers[1].end_reason.as_deref(), Some("done"));
}

#[tokio::test]
async fn a_worker_that_dies_after_max_worker_restarts_goes_to_a_human() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let implementer = "[[prompts]]\nshell = \"kill -9 $PPID\"\n";
    let engine = connect(
        &data_dir,
        &github,
        &dispatch_start(),
        implementer,
        "max_worker_restarts = 1",
    )
    .await;

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    wait_for(async || {
        github
            .labels(REPOSITORY, 41)
            .contains(&"mobius:needs-human".to_string())
            .then_some(())
    })
    .await;
    assert!(
        !github
            .labels(REPOSITORY, 41)
            .contains(&"mobius:working".to_string())
    );
    let implementers = sessions(&engine, "implementer").await;
    assert_eq!(implementers.len(), 2);
    assert_eq!(
        engine
            .store
            .tasks()
            .live(REPOSITORY, 41)
            .await
            .unwrap()
            .unwrap()
            .state,
        "needs_human"
    );
    assert!(github.pull_requests(REPOSITORY).is_empty());
    let prompt = wait_for(async || {
        lead_prompts(&engine)
            .await
            .into_iter()
            .find(|prompt| prompt.contains(" stop of #41 \"Add plan model\":"))
    })
    .await;
    assert!(
        prompt.contains("the Worker failed after 1 restarts. Mobius added mobius:needs-human. The last error ends with these lines:\n\n```\nIncoming transport closed"),
        "{prompt}"
    );
}

#[tokio::test]
async fn a_github_error_shows_its_status_and_message_in_the_stop_event() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let implementer = format!("[[prompts]]\nshell = \"{COMMIT}\"\n");
    let engine = connect(
        &data_dir,
        &github,
        &dispatch_start(),
        &implementer,
        "max_worker_restarts = 1",
    )
    .await;
    github.fail_pull_request_creation(REPOSITORY, 403, "API rate limit exceeded");

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    let prompt = wait_for(async || {
        lead_prompts(&engine)
            .await
            .into_iter()
            .find(|prompt| prompt.contains(" stop of #41 \"Add plan model\":"))
    })
    .await;
    assert!(
        prompt.contains(
            "The last error ends with these lines:\n\n```\nGitHub 403: API rate limit exceeded\n```"
        ),
        "{prompt}"
    );
}

#[tokio::test]
async fn a_worker_that_dies_starts_again_after_the_restart_wait() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let flag = data_dir.path().join("died");
    let implementer = format!(
        "[[prompts]]\nshell = \"{}\"\n",
        die_once(&flag, COMMIT).replace('"', "\\\"")
    );
    let engine = connect_with(
        &data_dir,
        &github,
        &dispatch_start(),
        &implementer,
        "",
        |config| config.restart_waits = vec![Duration::from_secs(3)],
    )
    .await;

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    wait_for(async || {
        let implementers = sessions(&engine, "implementer").await;
        (implementers.len() == 1 && implementers[0].ended_at.is_some()).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(sessions(&engine, "implementer").await.len(), 1);
    wait_for(async || (!github.pull_requests(REPOSITORY).is_empty()).then_some(())).await;
    assert_eq!(sessions(&engine, "implementer").await.len(), 2);
}

#[tokio::test]
async fn a_worker_that_fails_in_a_rate_limit_starts_again_after_the_reset() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let implementer = format!("[[prompts]]\nshell = \"{COMMIT}\"\n");
    let engine = connect(&data_dir, &github, &dispatch_start(), &implementer, "").await;
    github.fail_pull_request_creation(REPOSITORY, 403, "API rate limit exceeded");
    let reset = OffsetDateTime::now_utc().unix_timestamp() + 5;
    github.exhaust_rate_limit(reset);

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    let implementers = wait_for(async || {
        let implementers = sessions(&engine, "implementer").await;
        (implementers.len() == 2).then_some(implementers)
    })
    .await;
    assert!(implementers[1].started_at.unix_timestamp() >= reset);
}

#[tokio::test]
async fn a_task_in_needs_human_stays_in_needs_human_after_the_next_polls() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let implementer = "[[prompts]]\nshell = \"kill -9 $PPID\"\n";
    let engine = connect(
        &data_dir,
        &github,
        &dispatch_start(),
        implementer,
        "max_worker_restarts = 1",
    )
    .await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || {
        let labels = github.labels(REPOSITORY, 41);
        (labels.contains(&"mobius:needs-human".to_string())
            && !labels.contains(&"mobius:working".to_string()))
        .then_some(())
    })
    .await;

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let task = engine
        .store
        .tasks()
        .live(REPOSITORY, 41)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.state, "needs_human");
}

#[tokio::test]
async fn a_comment_of_the_owner_keeps_mobius_needs_human_on_a_task_in_needs_human() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let implementer = "[[prompts]]\nshell = \"kill -9 $PPID\"\n";
    let engine = connect(
        &data_dir,
        &github,
        &dispatch_start(),
        implementer,
        "max_worker_restarts = 1",
    )
    .await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || {
        github
            .labels(REPOSITORY, 41)
            .contains(&"mobius:needs-human".to_string())
            .then_some(())
    })
    .await;

    github.add_comment(REPOSITORY, 41, "owner", "I will look at it.");
    wait_for(async || {
        lead_prompts(&engine)
            .await
            .into_iter()
            .find(|prompt| prompt.contains("comment on #41"))
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    assert!(
        github
            .labels(REPOSITORY, 41)
            .contains(&"mobius:needs-human".to_string())
    );
}

#[tokio::test]
async fn mobius_ready_on_a_task_in_needs_human_with_no_pull_request_starts_the_implementer_on_the_same_branch()
 {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let go = data_dir.path().join("go");
    let implementer = format!(
        "[[prompts]]\nshell = \"if [ -e '{}' ]; then {COMMIT}; else kill -9 $PPID; fi\"\n",
        go.display()
    );
    let engine = connect(
        &data_dir,
        &github,
        &dispatch_start(),
        &implementer,
        "max_worker_restarts = 1",
    )
    .await;
    github.set_body(REPOSITORY, 41, "Plans have a price.");
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    let stopped = wait_for(async || {
        engine
            .store
            .tasks()
            .live(REPOSITORY, 41)
            .await
            .unwrap()
            .filter(|task| task.state == "needs_human")
    })
    .await;
    wait_for(async || {
        github
            .labels(REPOSITORY, 41)
            .contains(&"mobius:needs-human".to_string())
            .then_some(())
    })
    .await;
    assert!(github.pull_requests(REPOSITORY).is_empty());
    fs::write(&go, "").unwrap();

    github.remove_label(REPOSITORY, 41, "mobius:needs-human", "owner");
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    let task = wait_for(async || {
        engine
            .store
            .tasks()
            .live(REPOSITORY, 41)
            .await
            .unwrap()
            .filter(|task| task.pull_request.is_some())
    })
    .await;
    assert_eq!(task.id, stopped.id);
    assert_eq!(task.branch, stopped.branch);
    assert_eq!(github.pull_requests(REPOSITORY).len(), 1);
    assert!(stopped.branch.is_some());
    assert_eq!(
        Some(github.pull_requests(REPOSITORY)[0].head.clone()),
        stopped.branch
    );
    wait_for(async || {
        (!github
            .labels(REPOSITORY, 41)
            .contains(&"mobius:ready".to_string()))
        .then_some(())
    })
    .await;
    let labels = github.labels(REPOSITORY, 41);
    assert!(labels.contains(&"mobius:working".to_string()), "{labels:?}");
    assert!(
        !labels.contains(&"mobius:needs-human".to_string()),
        "{labels:?}"
    );
    wait_for(async || {
        let implementers = sessions(&engine, "implementer").await;
        (implementers.last().unwrap().end_reason.as_deref() == Some("done")).then_some(())
    })
    .await;
}

#[tokio::test]
async fn mobius_ready_of_the_app_on_a_task_in_needs_human_has_no_effect_when_autopilot_is_off() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let implementer = "[[prompts]]\nshell = \"kill -9 $PPID\"\n";
    let engine = connect(
        &data_dir,
        &github,
        &dispatch_start(),
        implementer,
        "max_worker_restarts = 1",
    )
    .await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || {
        github
            .labels(REPOSITORY, 41)
            .contains(&"mobius:needs-human".to_string())
            .then_some(())
    })
    .await;
    let implementers = sessions(&engine, "implementer").await.len();

    github.remove_label(REPOSITORY, 41, "mobius:needs-human", "owner");
    github.add_label(REPOSITORY, 41, "mobius:ready", &format!("{APP_SLUG}[bot]"));
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let task = engine
        .store
        .tasks()
        .live(REPOSITORY, 41)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.state, "needs_human");
    assert_eq!(sessions(&engine, "implementer").await.len(), implementers);
    assert!(
        github
            .labels(REPOSITORY, 41)
            .contains(&"mobius:ready".to_string())
    );
}

#[tokio::test]
async fn a_lead_that_crashes_gets_the_same_event_in_a_new_session() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let flag = data_dir.path().join("died");
    let lead = format!(
        "[[prompts]]\nwhen = \"dispatch of #41\"\nshell = \"{}\"\n{START}\n",
        die_once(&flag, "true").replace('"', "\\\"")
    );
    let engine = connect(
        &data_dir,
        &github,
        &lead,
        &format!("[[prompts]]\nshell = \"{COMMIT}\"\n"),
        "",
    )
    .await;

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    wait_for(async || (!sessions(&engine, "implementer").await.is_empty()).then_some(())).await;
    let leads = sessions(&engine, "lead_chat").await;
    assert!(leads.len() >= 2, "{leads:?}");
    assert_eq!(leads[0].end_reason.as_deref(), Some("failed"));
}

#[tokio::test]
async fn a_lead_that_always_crashes_sends_the_event_to_the_inbox() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let lead = "[[prompts]]\nwhen = \"dispatch of #41\"\nshell = \"kill -9 $PPID\"\n";
    let engine = connect(&data_dir, &github, lead, "", "").await;

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    let item = wait_for(async || {
        inbox::list(&engine)
            .await
            .unwrap()
            .into_iter()
            .find(|item| item.kind == InboxKind::LeadFailed)
    })
    .await;
    assert!(
        item.text
            .contains(" dispatch of #41 \"Add plan model\" by @owner:"),
        "{}",
        item.text
    );
    assert_eq!(sessions(&engine, "lead_chat").await.len(), 4);
}

#[tokio::test]
async fn the_housekeeper_removes_the_directories_that_nothing_owns() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let worktrees = data_dir.path().join("worktrees/owner/shop");
    let stale = [
        worktrees.join("task-99"),
        worktrees.join("review-12345"),
        data_dir.path().join("scratch/777"),
    ];
    let lead = data_dir.path().join("leads/owner/shop/12");
    for dir in stale.iter().chain([&lead]) {
        fs::create_dir_all(dir).unwrap();
    }

    connect(
        &data_dir,
        &github,
        "",
        "",
        "housekeeper_interval = \"100ms\"",
    )
    .await;

    wait_for(async || stale.iter().all(|dir| !dir.exists()).then_some(())).await;
    assert!(lead.exists());
}
