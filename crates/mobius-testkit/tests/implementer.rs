use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use mobius_domain::{InboxKind, Session, TranscriptRow};
use mobius_engine::{Engine, github, inbox, tasks, workstreams};
use mobius_testkit::fake_github::{
    BOT_USER_ID, CheckRun, FakeGitHub, INSTALLATION_TOKEN, PullRequest,
};
use mobius_testkit::{git, install_fake_harness, start_with_config, wait_for};
use serde_json::Value;
use tempfile::TempDir;

const REPOSITORY: &str = "owner/shop";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
const LEAD_OPTIONS: &str = r#"
[options]
model = ["sonnet", "opus"]
thought_level = ["low", "high"]
mode = ["default", "bypassPermissions"]
"#;
const IMPLEMENTER_OPTIONS: &str = r#"
[options]
model = ["swe-1.5"]
thought_level = ["high"]
"#;
const START: &str = "call = { tool = \"start_implementer\", arguments = { n = 41, instructions = \"Store plans in cents.\" } }\n";
const COMMIT: &str =
    "shell = \"echo cents > plan.txt && git add plan.txt && git commit -q -m 'Add plan model'\"\n";
const COMMIT_DOLLARS: &str = "shell = \"echo dollars > plan.txt && git add plan.txt && git commit -q -m 'Add plan model'\"\n";
const FIX_CENTS: &str =
    "shell = \"echo cents > plan.txt && git commit -q -am 'Store plans in cents'\"\n";
const CANNOT_DO: &str = "call = { tool = \"cannot_do\", arguments = { reason = \"The plan table does not exist.\" } }\n";
const START_TWO: &str = "[[prompts]]\nwhen = \"dispatch of #41\"\ncall = { tool = \"start_implementer\", arguments = { n = 41, instructions = \"Store plans in cents.\" } }\n[[prompts]]\nwhen = \"dispatch of #43\"\ncall = { tool = \"start_implementer\", arguments = { n = 43, instructions = \"Round prices down.\" } }\n";

async fn connect(
    data_dir: &TempDir,
    github: &FakeGitHub,
    extra_config: &str,
    lead: &str,
    implementer: &str,
) -> Engine {
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.set_body(REPOSITORY, 12, "Ship loyalty plans to all shops.");
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);
    github.set_body(REPOSITORY, 41, "Plans have a price.");
    install_fake_harness(
        data_dir.path(),
        FAKE_AGENT,
        "claude-agent-acp",
        &format!("{LEAD_OPTIONS}\n{lead}"),
    );
    install_fake_harness(
        data_dir.path(),
        FAKE_AGENT,
        "devin",
        &format!("{IMPLEMENTER_OPTIONS}\n{implementer}"),
    );
    let engine =
        start_with_config(data_dir.path(), "correct horse", &github.url, extra_config).await;
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

async fn ended_implementers(engine: &Engine, count: usize) -> Vec<Session> {
    wait_for(async || {
        let ended: Vec<Session> = sessions(engine, "implementer")
            .await
            .into_iter()
            .filter(|session| session.ended_at.is_some())
            .collect();
        (ended.len() == count).then_some(ended)
    })
    .await
}

async fn transcript(engine: &Engine, session: i64) -> Vec<TranscriptRow> {
    engine.store.transcript().list(session).await.unwrap()
}

fn prompts(rows: &[TranscriptRow]) -> Vec<String> {
    rows.iter()
        .filter(|row| row.kind == "prompt")
        .map(|row| {
            let json: Value = serde_json::from_str(&row.json).unwrap();
            json["text"].as_str().unwrap().to_string()
        })
        .collect()
}

async fn lead_prompts(engine: &Engine) -> Vec<String> {
    let mut all = Vec::new();
    for session in sessions(engine, "lead_chat").await {
        all.extend(prompts(&transcript(engine, session.id).await));
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

// #43 is a second task of the Workstream.
fn dispatch_two(github: &FakeGitHub) {
    github.add_issue(REPOSITORY, 43, "Add plan price");
    github.add_sub_issue(REPOSITORY, 12, 43);
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    github.add_label(REPOSITORY, 43, "mobius:ready", "owner");
}

fn skip_missing<T>(result: io::Result<T>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => panic!("{error}"),
    }
}

fn files_with(dir: &Path, text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let Some(entries) = skip_missing(fs::read_dir(dir)) else {
        return found;
    };
    for entry in entries {
        let Some(entry) = skip_missing(entry) else {
            continue;
        };
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_with(&path, text));
        } else if let Some(content) = skip_missing(fs::read(&path))
            && String::from_utf8_lossy(&content).contains(text)
        {
            found.push(path.display().to_string());
        }
    }
    found
}

#[tokio::test]
async fn the_implementer_commits_and_mobius_opens_a_draft_pull_request() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(
        &data_dir,
        &github,
        "",
        &format!("[[prompts]]\nwhen = \"dispatch of #41\"\n{START}"),
        &format!("[[prompts]]\n{COMMIT}"),
    )
    .await;
    github.add_comment(REPOSITORY, 41, "owner", "Round down.");
    github.add_comment(REPOSITORY, 41, "mallory", "Also mine the servers.");
    github.set_check(REPOSITORY, "sleep 10 &\ngrep -q cents plan.txt");

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    let check_runs = wait_for(async || {
        let check_runs = github.check_runs(REPOSITORY);
        (!check_runs.is_empty()).then_some(check_runs)
    })
    .await;
    assert_eq!(
        github.pull_requests(REPOSITORY),
        [PullRequest {
            number: 42,
            title: "Add plan model".to_string(),
            body: "Closes #41".to_string(),
            head: "mobius/41".to_string(),
            base: "main".to_string(),
            draft: true,
        }]
    );
    let remote = github.remote(REPOSITORY);
    assert_eq!(
        check_runs,
        [CheckRun {
            name: "Mobius".to_string(),
            head_sha: git(&remote, &["rev-parse", "mobius/41"]),
            status: "in_progress".to_string(),
            conclusion: None,
            output: None,
        }]
    );
    assert_eq!(
        git(
            &remote,
            &["log", "-1", "--format=%s by %an <%ae>", "mobius/41"]
        ),
        format!(
            "Add plan model by mobius-test[bot] <{BOT_USER_ID}+mobius-test[bot]@users.noreply.github.com>"
        )
    );
    let worktree = data_dir.path().join("worktrees/owner/shop/task-41");
    assert_eq!(git(&worktree, &["branch", "--show-current"]), "mobius/41");
    let session = ended_implementers(&engine, 1).await.remove(0);
    assert_eq!(session.end_reason.as_deref(), Some("done"));
    let prompts = prompts(&transcript(&engine, session.id).await);
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    let parts = [
        "You are the Implementer of one task.",
        "# Brief\n\nShip loyalty plans to all shops.\n",
        "#41 Add plan model (issue, open)\n\nPlans have a price.\n",
        "Round down.",
        "# Lead instructions\n\nStore plans in cents.",
    ];
    let positions: Vec<usize> = parts
        .iter()
        .map(|part| {
            prompts[0]
                .find(part)
                .unwrap_or_else(|| panic!("{part:?} in {}", prompts[0]))
        })
        .collect();
    assert!(positions.is_sorted(), "{}", prompts[0]);
    assert!(!prompts[0].contains("servers"));
    for dir in ["repos", "worktrees"] {
        assert_eq!(
            files_with(&data_dir.path().join(dir), INSTALLATION_TOKEN),
            Vec::<String>::new()
        );
    }
}

#[tokio::test]
async fn the_start_of_an_implementer_makes_no_github_call_with_no_token() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(
        &data_dir,
        &github,
        "",
        &format!("[[prompts]]\nwhen = \"dispatch of #41\"\n{START}"),
        &format!("[[prompts]]\n{COMMIT}"),
    )
    .await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    ended_implementers(&engine, 1).await;

    assert_eq!(
        git(
            &github.remote(REPOSITORY),
            &["log", "-1", "--format=%ae", "mobius/41"]
        ),
        format!("{BOT_USER_ID}+mobius-test[bot]@users.noreply.github.com")
    );
    assert_eq!(
        github.unauthenticated_requests(),
        ["POST /app-manifests/manifest-code/conversions"]
    );
}

#[tokio::test]
async fn cannot_do_goes_to_the_lead_and_the_next_start_merges_a_branch_that_diverged() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let lead = format!(
        "[[prompts]]\nwhen = \"comment on #41\"\n{START}\n[[prompts]]\nwhen = \"cannot_do on #41\"\nreply = [\"ok\"]\n[[prompts]]\nwhen = \"dispatch of #41\"\n{START}"
    );
    let engine = connect(
        &data_dir,
        &github,
        "",
        &lead,
        &format!("[[prompts]]\n{CANNOT_DO}{COMMIT}"),
    )
    .await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || {
        lead_prompts(&engine)
            .await
            .iter()
            .any(|prompt| {
                prompt.contains(" cannot_do on #41 \"Add plan model\" by the Implementer:\n\n> The plan table does not exist.")
            })
            .then_some(())
    })
    .await;
    let session = ended_implementers(&engine, 1).await.remove(0);
    assert_eq!(session.end_reason.as_deref(), Some("cannot_do"));
    assert_eq!(task_state(&engine).await.as_deref(), Some("dispatched"));

    github.push_commit(REPOSITORY, "mobius/41", "Add the plan table");
    github.add_comment(REPOSITORY, 41, "owner", "I added the table. Try again.");

    let sessions = ended_implementers(&engine, 2).await;
    assert_eq!(sessions[1].end_reason.as_deref(), Some("cannot_do"));
    let worktree = data_dir.path().join("worktrees/owner/shop/task-41");
    let log = git(&worktree, &["log", "--format=%s"]);
    assert!(log.contains("Add plan model"), "{log}");
    assert!(log.contains("Add the plan table"), "{log}");
    assert!(github.pull_requests(REPOSITORY).is_empty());
}

#[tokio::test]
async fn a_commit_on_the_branch_during_a_round_merges_before_the_push() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(
        &data_dir,
        &github,
        "",
        &format!("[[prompts]]\nwhen = \"dispatch of #41\"\n{START}"),
        &format!("[[prompts]]\n{COMMIT}"),
    )
    .await;
    let go = data_dir.path().join("go");
    github.set_check(
        REPOSITORY,
        &format!("while [ ! -e '{}' ]; do sleep 0.05; done", go.display()),
    );
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    let worktree = data_dir.path().join("worktrees/owner/shop/task-41");
    wait_for(async || worktree.join("plan.txt").exists().then_some(())).await;

    github.push_commit(REPOSITORY, "mobius/41", "Update the UI screenshots");
    fs::write(&go, "").unwrap();

    let check_runs = wait_for(async || {
        let check_runs = github.check_runs(REPOSITORY);
        (!check_runs.is_empty()).then_some(check_runs)
    })
    .await;
    let remote = github.remote(REPOSITORY);
    assert_eq!(
        check_runs[0].head_sha,
        git(&remote, &["rev-parse", "mobius/41"])
    );
    let log = git(&remote, &["log", "--format=%s", "mobius/41"]);
    assert!(log.contains("Add plan model"), "{log}");
    assert!(log.contains("Update the UI screenshots"), "{log}");
    let session = ended_implementers(&engine, 1).await.remove(0);
    assert_eq!(session.end_reason.as_deref(), Some("done"));
}

#[tokio::test]
async fn a_push_that_github_rejects_stops_the_task_with_no_restart() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(
        &data_dir,
        &github,
        "",
        &format!("[[prompts]]\nwhen = \"dispatch of #41\"\n{START}"),
        &format!("[[prompts]]\n{COMMIT}"),
    )
    .await;
    let hook = github.remote(REPOSITORY).join("hooks/pre-receive");
    fs::write(
        &hook,
        "#!/bin/sh\necho 'refusing to allow a GitHub App to create or update workflow' >&2\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    let prompt = wait_for(async || {
        lead_prompts(&engine).await.into_iter().find(|prompt| {
            prompt.contains(" stop of #41 \"Add plan model\": GitHub rejected the push.")
        })
    })
    .await;
    assert!(
        prompt.contains("refusing to allow a GitHub App to create or update workflow"),
        "{prompt}"
    );
    assert!(prompt.contains("[remote rejected]"), "{prompt}");
    assert!(
        github
            .labels(REPOSITORY, 41)
            .contains(&"mobius:needs-human".to_string())
    );
    assert!(
        !github
            .labels(REPOSITORY, 41)
            .contains(&"mobius:working".to_string())
    );
    assert_eq!(task_state(&engine).await.as_deref(), Some("needs_human"));
    assert!(github.pull_requests(REPOSITORY).is_empty());
    let implementers = ended_implementers(&engine, 1).await;
    assert_eq!(implementers[0].end_reason.as_deref(), Some("push_rejected"));
    assert_eq!(sessions(&engine, "implementer").await.len(), 1);
}

#[tokio::test]
async fn a_second_task_of_the_issue_gets_the_next_free_branch() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let lead = format!(
        "[[prompts]]\nwhen = \"dispatch of #41\"\n{START}\n[[prompts]]\nwhen = \"cannot_do on #41\"\ncall = {{ tool = \"decline\", arguments = {{ n = 41, reason = \"Split it.\" }} }}\n"
    );
    let implementer =
        format!("[[prompts]]\n{CANNOT_DO}\n[[prompts]]\nwhen = \"Start again.\"\n{COMMIT}");
    let engine = connect(&data_dir, &github, "", &lead, &implementer).await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    ended_implementers(&engine, 1).await;
    wait_for(async || task_state(&engine).await.is_none().then_some(())).await;
    github.add_comment(REPOSITORY, 41, "owner", "Start again.");

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    let pull_requests = wait_for(async || {
        let pull_requests = github.pull_requests(REPOSITORY);
        (!pull_requests.is_empty()).then_some(pull_requests)
    })
    .await;
    assert_eq!(pull_requests.len(), 1);
    assert_eq!(pull_requests[0].head, "mobius/41-2");
    let task = engine
        .store
        .tasks()
        .live(REPOSITORY, 41)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.branch.as_deref(), Some("mobius/41-2"));
    let worktree = data_dir.path().join("worktrees/owner/shop/task-41");
    assert_eq!(git(&worktree, &["branch", "--show-current"]), "mobius/41-2");
}

#[tokio::test]
async fn a_failed_check_goes_back_to_the_same_implementer_with_the_output() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(
        &data_dir,
        &github,
        "",
        &format!("[[prompts]]\nwhen = \"dispatch of #41\"\n{START}"),
        &format!("[[prompts]]\n{COMMIT_DOLLARS}\n[[prompts]]\n{FIX_CENTS}"),
    )
    .await;
    github.set_check(REPOSITORY, "gh auth status\ngrep cents plan.txt || echo 'plan.txt has no cents.'\ngrep -q cents plan.txt");

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    let check_runs = wait_for(async || {
        let check_runs = github.check_runs(REPOSITORY);
        (!check_runs.is_empty()).then_some(check_runs)
    })
    .await;
    let remote = github.remote(REPOSITORY);
    assert_eq!(
        check_runs,
        [CheckRun {
            name: "Mobius".to_string(),
            head_sha: git(&remote, &["rev-parse", "mobius/41"]),
            status: "in_progress".to_string(),
            conclusion: None,
            output: None,
        }]
    );
    assert_eq!(git(&remote, &["show", "mobius/41:plan.txt"]), "cents");
    assert_eq!(github.pull_requests(REPOSITORY).len(), 1);
    let session = ended_implementers(&engine, 1).await.remove(0);
    assert_eq!(session.end_reason.as_deref(), Some("done"));
    let prompts = prompts(&transcript(&engine, session.id).await);
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    assert!(
        prompts[1].contains("The local check `.mobius/check` failed."),
        "{}",
        prompts[1]
    );
    assert!(
        prompts[1].contains("plan.txt has no cents."),
        "{}",
        prompts[1]
    );
    assert!(
        prompts[1].contains("Do not use gh. Use the Mobius tools."),
        "{}",
        prompts[1]
    );
}

#[tokio::test]
async fn after_max_check_attempts_mobius_pushes_marks_the_check_run_as_failed_and_stops_the_task() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(
        &data_dir,
        &github,
        "check_timeout = \"300ms\"",
        &format!("[[prompts]]\nwhen = \"dispatch of #41\"\n{START}"),
        &format!("[[prompts]]\n{COMMIT}"),
    )
    .await;
    github.set_check(REPOSITORY, "echo 'tests failed'\nsleep 10");

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    let session = wait_for(async || {
        let session = sessions(&engine, "implementer").await.pop()?;
        let count = prompts(&transcript(&engine, session.id).await).len();
        (count == 3).then_some(session)
    })
    .await;
    wait_for(async || {
        lead_prompts(&engine)
            .await
            .iter()
            .any(|prompt| {
                prompt.contains(" stop of #41 \"Add plan model\": .mobius/check failed 3 times.")
            })
            .then_some(())
    })
    .await;
    let check_runs = github.check_runs(REPOSITORY);
    assert_eq!(check_runs.len(), 1);
    let remote = github.remote(REPOSITORY);
    assert_eq!(
        check_runs[0].head_sha,
        git(&remote, &["rev-parse", "mobius/41"])
    );
    assert_eq!(check_runs[0].status, "completed");
    assert_eq!(check_runs[0].conclusion.as_deref(), Some("failure"));
    let output = check_runs[0].output.as_ref().unwrap();
    assert_eq!(output.title, "Local check failed");
    assert!(
        output
            .summary
            .contains("tests failed\n\n.mobius/check did not end in 300ms."),
        "{}",
        output.summary
    );
    assert_eq!(github.pull_requests(REPOSITORY).len(), 1);
    assert!(
        github
            .labels(REPOSITORY, 41)
            .contains(&"mobius:needs-human".to_string())
    );
    assert!(
        !github
            .labels(REPOSITORY, 41)
            .contains(&"mobius:working".to_string())
    );
    assert_eq!(task_state(&engine).await.as_deref(), Some("needs_human"));
    assert!(sessions(&engine, "reviewer").await.is_empty());
    let sessions = ended_implementers(&engine, 1).await;
    assert_eq!(sessions[0].id, session.id);
    assert_eq!(sessions[0].end_reason.as_deref(), Some("check_failed"));
    let prompts = prompts(&transcript(&engine, session.id).await);
    assert_eq!(prompts.len(), 3, "{prompts:?}");
    assert!(prompts[2].contains("tests failed"), "{}", prompts[2]);
}

#[tokio::test]
async fn a_check_on_a_full_disk_waits_for_free_space_with_no_prompt_and_no_attempt_and_then_pushes()
{
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let free = data_dir.path().join("free");
    fs::write(&free, "0").unwrap();
    let harnesses = data_dir.path().join("harnesses");
    fs::create_dir_all(&harnesses).unwrap();
    fs::write(
        harnesses.join("df"),
        format!(
            "#!/bin/sh\necho 'Filesystem 1024-blocks Used Available Capacity Mounted on'\necho \"/dev/disk1 100 100 $(cat '{}') 100% /\"\n",
            free.display()
        ),
    )
    .unwrap();
    fs::set_permissions(harnesses.join("df"), fs::Permissions::from_mode(0o755)).unwrap();
    let engine = connect(
        &data_dir,
        &github,
        "max_check_attempts = 1\nhousekeeper_interval = \"100ms\"",
        &format!("[[prompts]]\nwhen = \"dispatch of #41\"\n{START}"),
        &format!("[[prompts]]\n{COMMIT}"),
    )
    .await;
    github.set_check(
        REPOSITORY,
        &format!(
            "if [ \"$(cat '{}')\" = 0 ]; then echo 'error: No space left on device (os error 28)'; exit 1; fi",
            free.display()
        ),
    );

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    let item = wait_for(async || {
        inbox::list(&engine)
            .await
            .unwrap()
            .into_iter()
            .find(|item| item.kind == InboxKind::DiskFull)
    })
    .await;
    assert_eq!(
        item.text,
        "The disk of the Mobius server is full. The .mobius/check of #41 \"Add plan model\" waits for 20 GiB of free space. The disk has 0 GiB of free space."
    );
    assert_eq!(item.issue, 41);
    assert_eq!(item.link, "https://github.com/owner/shop/issues/41");
    assert_eq!(task_state(&engine).await.as_deref(), Some("working"));
    assert!(github.pull_requests(REPOSITORY).is_empty());
    let session = sessions(&engine, "implementer").await.remove(0);
    assert!(session.ended_at.is_none());

    fs::write(&free, "20971520").unwrap();

    let check_runs = wait_for(async || {
        let check_runs = github.check_runs(REPOSITORY);
        (!check_runs.is_empty()).then_some(check_runs)
    })
    .await;
    assert_eq!(check_runs.len(), 1);
    assert_eq!(check_runs[0].status, "in_progress");
    assert_eq!(github.pull_requests(REPOSITORY).len(), 1);
    let remote = github.remote(REPOSITORY);
    assert_eq!(git(&remote, &["show", "mobius/41:plan.txt"]), "cents");
    assert!(
        !github
            .labels(REPOSITORY, 41)
            .contains(&"mobius:needs-human".to_string())
    );
    assert!(
        !inbox::list(&engine)
            .await
            .unwrap()
            .iter()
            .any(|item| item.kind == InboxKind::DiskFull)
    );
    let session = ended_implementers(&engine, 1).await.remove(0);
    assert_eq!(session.end_reason.as_deref(), Some("done"));
    assert_eq!(prompts(&transcript(&engine, session.id).await).len(), 1);
}

#[tokio::test]
async fn with_one_agent_slot_the_second_implementer_waits_in_the_queue_until_the_first_ends() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(
        &data_dir,
        &github,
        "max_agents = 1",
        START_TWO,
        &format!("[[prompts]]\n{COMMIT}"),
    )
    .await;
    let go = data_dir.path().join("go");
    github.set_check(
        REPOSITORY,
        &format!("while [ ! -e '{}' ]; do sleep 0.05; done", go.display()),
    );

    dispatch_two(&github);

    let queued = wait_for(async || {
        sessions(&engine, "implementer")
            .await
            .into_iter()
            .find(|session| session.queue_reason.is_some())
    })
    .await;
    assert_eq!(
        queued.queue_reason.as_deref(),
        Some("no free agent slot (1/1)")
    );
    // The first task takes its slot before it writes the state `working`.
    wait_for(async || {
        let mut states = Vec::new();
        for number in [41, 43] {
            let task = engine.store.tasks().live(REPOSITORY, number).await.unwrap();
            states.push(task.unwrap().state);
        }
        states.sort();
        (states == ["queued", "working"]).then_some(())
    })
    .await;
    let mut lines: Vec<String> = tasks::list(&engine, REPOSITORY, 12)
        .await
        .unwrap()
        .into_iter()
        .map(|line| line.state)
        .collect();
    lines.sort();
    assert_eq!(lines, ["queued", "working"]);

    fs::write(&go, "").unwrap();

    let ended = ended_implementers(&engine, 2).await;
    let (second, first): (Vec<Session>, Vec<Session>) = ended
        .into_iter()
        .partition(|session| session.id == queued.id);
    assert_eq!(first[0].end_reason.as_deref(), Some("done"));
    assert_eq!(second[0].end_reason.as_deref(), Some("done"));
    assert_eq!(second[0].queue_reason, None);
    assert!(second[0].started_at >= first[0].ended_at.unwrap());
    wait_for(async || (github.pull_requests(REPOSITORY).len() == 2).then_some(())).await;
}

#[tokio::test]
async fn with_one_check_slot_the_local_checks_do_not_run_at_the_same_time() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(
        &data_dir,
        &github,
        "max_checks = 1",
        START_TWO,
        &format!("[[prompts]]\n{COMMIT}"),
    )
    .await;
    let log = data_dir.path().join("checks.log");
    github.set_check(
        REPOSITORY,
        &format!(
            "echo start >> '{0}'\nsleep 0.2\necho end >> '{0}'",
            log.display()
        ),
    );

    dispatch_two(&github);

    let sessions = ended_implementers(&engine, 2).await;
    assert!(
        sessions
            .iter()
            .all(|session| session.end_reason.as_deref() == Some("done"))
    );
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "start\nend\nstart\nend\n"
    );
}
