use mobius_domain::{InboxKind, Session, TranscriptRow};
use mobius_engine::{Engine, github, inbox, workstreams};
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::{git, install_fake_harness, start_with_config, wait_for};
use serde_json::Value;
use tempfile::TempDir;

const REPOSITORY: &str = "owner/shop";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
const CLAUDE_OPTIONS: &str = r#"
[options]
model = ["sonnet", "opus"]
thought_level = ["low", "high"]
mode = ["default", "bypassPermissions"]
"#;
const IMPLEMENTER: &str = r#"
[options]
model = ["swe-1.5"]
thought_level = ["high"]

[[prompts]]
shell = "echo cents > plan.txt && git add plan.txt && git commit -q -m 'Add plan model'"
"#;
const APP_LOGIN: &str = "mobius-test[bot]";
const START: &str = "[[prompts]]\nwhen = \"dispatch of #41\"\ncall = { tool = \"start_implementer\", arguments = { n = 41, instructions = \"Store plans in cents.\" } }\n";

// The Lead and the Reviewer share the Harness `claude-agent-acp`, so `reviewer` gets a `when` for the Reviewer prompt. Each Implementer session is a new process, so `conflict` gets a `when` for the prompt of a conflict round.
async fn connect(
    data_dir: &TempDir,
    github: &FakeGitHub,
    reviewer: &str,
    conflict: &str,
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
        &format!("{CLAUDE_OPTIONS}\n{reviewer}\n{START}"),
    );
    install_fake_harness(
        data_dir.path(),
        FAKE_AGENT,
        "devin",
        &format!("{IMPLEMENTER}\n{conflict}"),
    );
    let engine = start_with_config(data_dir.path(), "correct horse", &github.url, "").await;
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

async fn lead_prompts(engine: &Engine) -> Vec<String> {
    let mut all = Vec::new();
    for session in sessions(engine, "lead_chat").await {
        all.extend(prompts(engine, session.id).await);
    }
    all
}

async fn task_state(engine: &Engine, number: i64) -> Option<String> {
    engine
        .store
        .tasks()
        .live(REPOSITORY, number)
        .await
        .unwrap()
        .map(|task| task.state)
}

const NO_FINDING: &str = "[[prompts]]\nwhen = \"You are the Reviewer\"\nshell = \"true\"\n";

async fn ready_for_review_events(engine: &Engine) -> usize {
    lead_prompts(engine)
        .await
        .iter()
        .filter(|prompt| {
            prompt.rsplit_once("# Event\n\n").is_some_and(|(_, event)| {
                event.contains(" ready for review of #41 \"Add plan model\"")
            })
        })
        .count()
}

#[tokio::test]
async fn a_merge_conflict_starts_a_conflict_round_that_merges_the_base_branch() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(
        &data_dir,
        &github,
        NO_FINDING,
        "[[prompts]]\nwhen = \"Merge the base branch and remove the conflicts.\"\nshell = \"git merge -q origin/main; echo cents > plan.txt && git add plan.txt && git commit -q --no-edit\"\n",
    )
    .await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || (ready_for_review_events(&engine).await == 1).then_some(())).await;
    let remote = github.remote(REPOSITORY);
    let first = git(&remote, &["rev-parse", "mobius/41"]);

    github.commit_file(REPOSITORY, "plan.txt", "dollars\n", "Use dollars");

    wait_for(async || (ready_for_review_events(&engine).await == 2).then_some(())).await;
    let head = git(&remote, &["rev-parse", "mobius/41"]);
    let parents = git(&remote, &["rev-list", "--parents", "-n", "1", &head]);
    assert_eq!(parents.split(' ').count(), 3, "{parents}");
    git(&remote, &["merge-base", "--is-ancestor", &first, &head]);
    git(&remote, &["merge-base", "--is-ancestor", "main", &head]);
    assert_eq!(git(&remote, &["show", "mobius/41:plan.txt"]), "cents");
    let check_runs: Vec<(String, Option<String>)> = github
        .check_runs(REPOSITORY)
        .into_iter()
        .map(|check_run| (check_run.head_sha, check_run.conclusion))
        .collect();
    assert_eq!(
        check_runs,
        [
            (first, Some("success".to_string())),
            (head, Some("success".to_string()))
        ]
    );
    let pull_requests = github.pull_requests(REPOSITORY);
    assert_eq!(pull_requests.len(), 1);
    assert!(!pull_requests[0].draft);
    let task = engine
        .store
        .tasks()
        .live(REPOSITORY, 41)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.fix_rounds, 0);
    let implementers = sessions(&engine, "implementer").await;
    assert_eq!(implementers.len(), 2);
    let prompts = prompts(&engine, implementers[1].id).await;
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    let parts = [
        "You are the Implementer",
        "# Brief\n\nShip loyalty plans to all shops.\n",
        "# Issue\n\n#41 Add plan model\n\nPlans have a price.\n\n# Base branch\n\norigin/main\n\nMerge the base branch and remove the conflicts. Make no other change.",
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
}

#[tokio::test]
async fn a_conflict_round_merges_when_the_base_branch_moves_during_the_round() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(
        &data_dir,
        &github,
        NO_FINDING,
        "[[prompts]]\nwhen = \"Merge the base branch and remove the conflicts.\"\nshell = \"git merge -q origin/main; echo cents > plan.txt && git add plan.txt && git commit -q --no-edit && git update-ref refs/remotes/origin/main $(git commit-tree -p origin/main -m 'Use euros' origin/main^{tree})\"\n",
    )
    .await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || (ready_for_review_events(&engine).await == 1).then_some(())).await;

    github.commit_file(REPOSITORY, "plan.txt", "dollars\n", "Use dollars");

    wait_for(async || (ready_for_review_events(&engine).await == 2).then_some(())).await;
    let implementers = sessions(&engine, "implementer").await;
    assert_eq!(implementers.len(), 2);
    assert_eq!(implementers[1].end_reason.as_deref(), Some("done"));
    let head = git(&github.remote(REPOSITORY), &["rev-parse", "mobius/41"]);
    let check_runs = github.check_runs(REPOSITORY);
    assert_eq!(check_runs.len(), 2);
    assert_eq!(check_runs[1].head_sha, head);
    assert_eq!(check_runs[1].conclusion.as_deref(), Some("success"));
    assert!(
        !github
            .labels(REPOSITORY, 41)
            .contains(&"mobius:needs-human".to_string())
    );
}

#[tokio::test]
async fn a_stale_pull_request_with_a_merge_conflict_goes_to_a_human() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(
        &data_dir,
        &github,
        &format!(
            "{NO_FINDING}[[prompts]]\nwhen = \"stale pull request #42\"\ncall = {{ tool = \"comment_pull_request\", arguments = {{ n = 42, text = \"This pull request is old and has a conflict. Close it?\" }} }}\n"
        ),
        "",
    )
    .await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || (ready_for_review_events(&engine).await == 1).then_some(())).await;
    github.set_created_at(REPOSITORY, 42, 0);

    github.commit_file(REPOSITORY, "plan.txt", "dollars\n", "Use dollars");

    wait_for(async || {
        github
            .comments(REPOSITORY, 42)
            .contains(&(
                APP_LOGIN.to_string(),
                "This pull request is old and has a conflict. Close it?".to_string(),
            ))
            .then_some(())
    })
    .await;
    assert!(
        lead_prompts(&engine).await.iter().any(|prompt| prompt.contains(
            " stale pull request #42 of #41 \"Add plan model\": it has a merge conflict and is older than 7days. https://github.com/owner/shop/pull/42"
        ))
    );
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
    let stale: Vec<_> = inbox::list(&engine)
        .await
        .unwrap()
        .into_iter()
        .filter(|item| item.kind == InboxKind::StalePullRequest)
        .collect();
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].issue, 41);
    assert_eq!(stale[0].link, "https://github.com/owner/shop/pull/42");
    assert_eq!(
        stale[0].text,
        "Pull request #42 of #41 \"Add plan model\" has a merge conflict and is older than 7days."
    );
    assert_eq!(
        task_state(&engine, 41).await.as_deref(),
        Some("needs_human")
    );
    assert_eq!(sessions(&engine, "implementer").await.len(), 1);
}

#[tokio::test]
async fn mobius_ready_on_a_task_in_needs_human_with_a_merge_conflict_starts_a_conflict_round_on_the_same_pull_request()
 {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(
        &data_dir,
        &github,
        NO_FINDING,
        "[[prompts]]\nwhen = \"Merge the base branch and remove the conflicts.\"\nshell = \"git merge -q origin/main; echo cents > plan.txt && git add plan.txt && git commit -q --no-edit\"\n",
    )
    .await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || (ready_for_review_events(&engine).await == 1).then_some(())).await;
    github.set_created_at(REPOSITORY, 42, 0);
    github.commit_file(REPOSITORY, "plan.txt", "dollars\n", "Use dollars");
    wait_for(async || {
        let labels = github.labels(REPOSITORY, 41);
        (task_state(&engine, 41).await.as_deref() == Some("needs_human")
            && labels.contains(&"mobius:needs-human".to_string()))
        .then_some(())
    })
    .await;
    let stopped = engine
        .store
        .tasks()
        .live(REPOSITORY, 41)
        .await
        .unwrap()
        .unwrap();
    let remote = github.remote(REPOSITORY);
    let first = git(&remote, &["rev-parse", "mobius/41"]);

    github.remove_label(REPOSITORY, 41, "mobius:needs-human", "owner");
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    wait_for(async || (ready_for_review_events(&engine).await == 2).then_some(())).await;
    let head = git(&remote, &["rev-parse", "mobius/41"]);
    git(&remote, &["merge-base", "--is-ancestor", &first, &head]);
    git(&remote, &["merge-base", "--is-ancestor", "main", &head]);
    let task = engine
        .store
        .tasks()
        .live(REPOSITORY, 41)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.id, stopped.id);
    assert_eq!(task.pull_request, stopped.pull_request);
    assert_eq!(github.pull_requests(REPOSITORY).len(), 1);
    assert_eq!(sessions(&engine, "implementer").await.len(), 2);
    let labels = github.labels(REPOSITORY, 41);
    assert!(labels.contains(&"mobius:working".to_string()), "{labels:?}");
    assert!(!labels.contains(&"mobius:ready".to_string()), "{labels:?}");
    assert!(
        !labels.contains(&"mobius:needs-human".to_string()),
        "{labels:?}"
    );
}

#[tokio::test]
async fn a_conflict_round_that_does_not_merge_the_base_branch_stops_the_task() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(
        &data_dir,
        &github,
        NO_FINDING,
        "[[prompts]]\nwhen = \"Merge the base branch and remove the conflicts.\"\nshell = \"true\"\n",
    )
    .await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || (ready_for_review_events(&engine).await == 1).then_some(())).await;

    github.commit_file(REPOSITORY, "plan.txt", "dollars\n", "Use dollars");

    wait_for(async || {
        lead_prompts(&engine)
            .await
            .iter()
            .any(|prompt| {
                prompt.contains(
                    " stop of #41 \"Add plan model\": the conflict round did not merge the base branch.",
                )
            })
            .then_some(())
    })
    .await;
    let head = git(&github.remote(REPOSITORY), &["rev-parse", "mobius/41"]);
    let check_runs = github.check_runs(REPOSITORY);
    assert_eq!(check_runs.len(), 2);
    assert_eq!(check_runs[1].head_sha, head);
    assert_eq!(check_runs[1].conclusion.as_deref(), Some("failure"));
    assert_eq!(
        check_runs[1].output.as_ref().unwrap().summary,
        "The Implementer did not merge `origin/main`."
    );
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
    assert_eq!(
        task_state(&engine, 41).await.as_deref(),
        Some("needs_human")
    );
    let implementers = sessions(&engine, "implementer").await;
    assert_eq!(implementers.len(), 2);
    assert_eq!(implementers[1].end_reason.as_deref(), Some("not_merged"));
}

#[tokio::test]
async fn a_pull_request_behind_its_base_starts_one_conflict_round_that_merges_the_base_branch() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(
        &data_dir,
        &github,
        NO_FINDING,
        "[[prompts]]\nwhen = \"Merge the base branch and remove the conflicts.\"\nshell = \"git merge -q --no-edit origin/main\"\n",
    )
    .await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || (ready_for_review_events(&engine).await == 1).then_some(())).await;
    let remote = github.remote(REPOSITORY);
    let first = git(&remote, &["rev-parse", "mobius/41"]);
    github.set_behind(REPOSITORY, 42);

    github.commit_file(REPOSITORY, "price.txt", "dollars\n", "Add price");

    wait_for(async || (ready_for_review_events(&engine).await == 2).then_some(())).await;
    let head = git(&remote, &["rev-parse", "mobius/41"]);
    git(&remote, &["merge-base", "--is-ancestor", &first, &head]);
    git(&remote, &["merge-base", "--is-ancestor", "main", &head]);
    assert_eq!(git(&remote, &["show", "mobius/41:price.txt"]), "dollars");
    assert_eq!(sessions(&engine, "implementer").await.len(), 2);
    let task = engine
        .store
        .tasks()
        .live(REPOSITORY, 41)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.fix_rounds, 0);
}

#[tokio::test]
async fn a_stale_pull_request_behind_its_base_goes_to_a_human_with_the_behind_reason() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, NO_FINDING, "").await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || (ready_for_review_events(&engine).await == 1).then_some(())).await;
    github.set_created_at(REPOSITORY, 42, 0);
    github.set_behind(REPOSITORY, 42);

    github.commit_file(REPOSITORY, "price.txt", "dollars\n", "Add price");

    let stale = wait_for(async || {
        inbox::list(&engine)
            .await
            .unwrap()
            .into_iter()
            .find(|item| item.kind == InboxKind::StalePullRequest)
    })
    .await;
    assert_eq!(
        task_state(&engine, 41).await.as_deref(),
        Some("needs_human")
    );
    assert_eq!(
        stale.text,
        "Pull request #42 of #41 \"Add plan model\" is behind its base branch and is older than 7days."
    );
    assert_eq!(sessions(&engine, "implementer").await.len(), 1);
}
