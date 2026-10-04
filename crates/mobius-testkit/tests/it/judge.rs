use mobius_domain::{Session, TranscriptRow};
use mobius_engine::{Engine, github, inbox, workstreams};
use mobius_testkit::fake_github::{FakeGitHub, Thread};
use mobius_testkit::{git, install_fake_harness, start_with_config, wait_for};
use serde_json::Value;
use tempfile::TempDir;

const REPOSITORY: &str = "owner/shop";
const APP: &str = "mobius-test[bot]";
const BOT: &str = "coderabbitai[bot]";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
// The Lead, the Reviewer, and the Judge share the Harness `claude-agent-acp`, so each prompt has a `when`.
const CLAUDE: &str = r#"
[options]
model = ["sonnet", "opus", "haiku"]
thought_level = ["low", "high"]
mode = ["default", "bypassPermissions"]

[[prompts]]
when = "You are the Judge"
{verdicts}

[[prompts]]
when = "You are the Reviewer"
shell = "true"

[[prompts]]
when = "follow-up on pull request #42"
call = { tool = "reply_thread", arguments = { thread = 4, text = "Follow-up: #99." } }

[[prompts]]
when = "dispatch of #41"
call = { tool = "start_implementer", arguments = { n = 41, instructions = "Store plans in cents." } }
"#;
const VERDICTS: &str = r#"call = { tool = "submit_verdicts", arguments = { items = [
    { item = 2, actions = [{ verdict = "fix", text = "Rename the field." }] },
    { item = 3, actions = [{ verdict = "question", text = "Explain why the plan stores cents." }] },
    { item = 4, actions = [{ verdict = "follow-up", text = "Move the parser to its own crate." }] },
    { item = 5, actions = [{ verdict = "reject", text = "The API needs this name." }] },
] } }"#;
const IMPLEMENTER: &str = r#"
[options]
model = ["swe-1.5"]
thought_level = ["high"]

[[prompts]]
when = "Action: fix: Rename the field."
shell = "echo 'price_cents' > plan.txt && git commit -q -am 'Rename the field' && git rev-parse HEAD"
call = { tool = "reply_thread", arguments = { thread = 2, text = "Fixed in {shell}." } }

[[prompts]]
shell = "echo cents > plan.txt && git add plan.txt && git commit -q -m 'Add plan model'"
"#;

async fn connect(
    data_dir: &TempDir,
    github: &FakeGitHub,
    verdicts: &str,
    implementer: &str,
) -> Engine {
    connect_with(data_dir, github, "", verdicts, implementer).await
}

async fn connect_with(
    data_dir: &TempDir,
    github: &FakeGitHub,
    extra_config: &str,
    verdicts: &str,
    implementer: &str,
) -> Engine {
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);
    github.set_body(REPOSITORY, 41, "Plans have a price.");
    install_fake_harness(
        data_dir.path(),
        FAKE_AGENT,
        "claude-agent-acp",
        &CLAUDE.replace("{verdicts}", verdicts),
    );
    install_fake_harness(data_dir.path(), FAKE_AGENT, "devin", implementer);
    let engine = start_with_config(
        data_dir.path(),
        "correct horse",
        &github.url,
        &format!("trusted_bots = [\"{BOT}\"]\nreview_quiet_period = \"200ms\"\n{extra_config}"),
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

async fn prompts(engine: &Engine, role: &str) -> Vec<String> {
    let mut all = Vec::new();
    for session in sessions(engine, role).await {
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

async fn reply(github: &FakeGitHub, thread: i64) -> Thread {
    wait_for(async || {
        let found = github.review_thread(REPOSITORY, 42, thread);
        (found.comments.len() == 2).then_some(found)
    })
    .await
}

async fn resolved(github: &FakeGitHub, thread: i64) -> Thread {
    wait_for(async || {
        let found = github.review_thread(REPOSITORY, 42, thread);
        found.resolved.then_some(found)
    })
    .await
}

#[tokio::test]
async fn the_judge_routes_one_item_of_each_verdict() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, VERDICTS, IMPLEMENTER).await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || (!inbox::list(&engine).await.unwrap().is_empty()).then_some(())).await;

    let fix = github.add_review_comment(REPOSITORY, 42, None, "owner", "Use price_cents.");
    let question = github.add_comment(REPOSITORY, 42, "owner", "Why cents?");
    let follow_up = github.add_review_comment(REPOSITORY, 42, None, "owner", "Split the parser.");
    let reject = github.add_review_comment(REPOSITORY, 42, None, BOT, "Rename plan to tier.");
    assert_eq!([fix, question, follow_up, reject], [2, 3, 4, 5]);

    let rejected = reply(&github, reject).await;
    assert_eq!(
        rejected.comments[1],
        (APP.to_string(), "The API needs this name.".to_string())
    );
    resolved(&github, reject).await;
    let judge = prompts(&engine, "judge").await;
    assert_eq!(judge.len(), 1, "{judge:?}");
    let parts = [
        "You are the Judge",
        "# Issue\n\n#41 Add plan model\n\nPlans have a price.\n",
        "# Items\n",
        "Thread 2, src/plan.rs line 12:\n\n@owner, ",
        "Thread 4, src/plan.rs line 12:",
        "Thread 5, src/plan.rs line 12:\n\n@coderabbitai[bot], ",
        "Comment 3:\n\n@owner, ",
    ];
    for part in parts {
        assert!(judge[0].contains(part), "{part:?} in {}", judge[0]);
    }
    let followed = reply(&github, follow_up).await;
    assert_eq!(
        followed.comments[1],
        (APP.to_string(), "Follow-up: #99.".to_string())
    );
    resolved(&github, follow_up).await;
    assert!(prompts(&engine, "lead_chat").await.iter().any(|prompt| prompt.contains(
        " follow-up on pull request #42 of #41 \"Add plan model\", item 4:\n\n> Move the parser to its own crate.\n"
    )));
    let fixed = resolved(&github, fix).await;
    let head = git(&github.remote(REPOSITORY), &["rev-parse", "mobius/41"]);
    assert_eq!(
        fixed,
        Thread {
            resolved: true,
            comments: vec![
                ("owner".to_string(), "Use price_cents.".to_string()),
                (APP.to_string(), format!("Fixed in {head}.")),
            ],
        }
    );
    let round = prompts(&engine, "implementer").await.pop().unwrap();
    for part in [
        "# Open items\n",
        "Thread 2, src/plan.rs line 12:",
        "Action: fix: Rename the field.\n",
        "Comment 3:",
        "Action: question: Explain why the plan stores cents.\n",
    ] {
        assert!(round.contains(part), "{part:?} in {round}");
    }
    assert!(!round.contains("Thread 4"), "{round}");
    assert!(!round.contains("Thread 5"), "{round}");
    let task = engine
        .store
        .tasks()
        .live(REPOSITORY, 41)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.fix_rounds, 1);
}

#[tokio::test]
async fn an_implementer_reply_with_an_answer_and_no_commit_resolves_the_thread() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(
        &data_dir,
        &github,
        r#"call = { tool = "submit_verdicts", arguments = { items = [
    { item = 2, actions = [{ verdict = "question", text = "Explain why the plan stores cents." }] },
] } }"#,
        &IMPLEMENTER.replace(
            "Action: fix: Rename the field.\"\nshell = \"echo 'price_cents' > plan.txt && git commit -q -am 'Rename the field' && git rev-parse HEAD\"\ncall = { tool = \"reply_thread\", arguments = { thread = 2, text = \"Fixed in {shell}.\" } }",
            "Action: question: Explain why the plan stores cents.\"\ncall = { tool = \"reply_thread\", arguments = { thread = 2, text = \"Cents avoid rounding.\" } }",
        ),
    )
    .await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || (!inbox::list(&engine).await.unwrap().is_empty()).then_some(())).await;

    let question = github.add_review_comment(REPOSITORY, 42, None, "owner", "Why cents?");

    let answered = resolved(&github, question).await;
    assert_eq!(
        answered,
        Thread {
            resolved: true,
            comments: vec![
                ("owner".to_string(), "Why cents?".to_string()),
                (APP.to_string(), "Cents avoid rounding.".to_string()),
            ],
        }
    );
}

#[tokio::test]
async fn an_implementer_reply_to_a_conversation_comment_resolves_nothing() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(
        &data_dir,
        &github,
        r#"call = { tool = "submit_verdicts", arguments = { items = [
    { item = 2, actions = [{ verdict = "question", text = "Explain why the plan stores cents." }] },
] } }"#,
        &IMPLEMENTER.replace(
            "Action: fix: Rename the field.\"\nshell = \"echo 'price_cents' > plan.txt && git commit -q -am 'Rename the field' && git rev-parse HEAD\"\ncall = { tool = \"reply_thread\", arguments = { thread = 2, text = \"Fixed in {shell}.\" } }",
            "Action: question: Explain why the plan stores cents.\"\ncall = { tool = \"reply_thread\", arguments = { thread = 2, text = \"Cents avoid rounding.\" } }",
        ),
    )
    .await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || (!inbox::list(&engine).await.unwrap().is_empty()).then_some(())).await;

    let question = github.add_comment(REPOSITORY, 42, "owner", "Why cents?");

    assert_eq!(question, 2);
    let answer = (
        APP.to_string(),
        "> Why cents?\n\nCents avoid rounding.".to_string(),
    );
    wait_for(async || {
        github
            .comments(REPOSITORY, 42)
            .contains(&answer)
            .then_some(())
    })
    .await;
    assert!(!github.review_thread(REPOSITORY, 42, question).resolved);
}

#[tokio::test]
async fn a_comment_in_an_unresolved_thread_makes_the_judge_run_again() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(
        &data_dir,
        &github,
        r#"call = { tool = "submit_verdicts", arguments = { items = [
    { item = 2, actions = [{ verdict = "reject", text = "The API needs this name." }] },
] } }"#,
        IMPLEMENTER,
    )
    .await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || (!inbox::list(&engine).await.unwrap().is_empty()).then_some(())).await;

    let thread = github.add_review_comment(REPOSITORY, 42, None, BOT, "Rename plan to tier.");
    resolved(&github, thread).await;
    assert_eq!(prompts(&engine, "judge").await.len(), 1);

    github.unresolve_review_thread(thread);
    github.add_review_comment(REPOSITORY, 42, Some(thread), "owner", "Rename it anyway.");

    wait_for(async || (prompts(&engine, "judge").await.len() == 2).then_some(())).await;
    let judge = prompts(&engine, "judge").await;
    assert!(judge[1].contains("Rename it anyway."), "{}", judge[1]);
}

#[tokio::test]
async fn a_fix_round_of_the_judge_from_needs_human_puts_the_working_label_back() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect_with(
        &data_dir,
        &github,
        "max_fix_rounds = 1",
        "shell = \"true\"",
        &format!(
            "{IMPLEMENTER}\n[[prompts]]\nwhen = \"Action: fix\"\nshell = \"echo x >> plan.txt && git commit -q -am Fix\"\n"
        ),
    )
    .await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || (!inbox::list(&engine).await.unwrap().is_empty()).then_some(())).await;

    github.add_review_comment(REPOSITORY, 42, None, BOT, "Rename plan to tier.");
    wait_for(async || {
        (task_state(&engine).await.as_deref() == Some("ready_for_review")).then_some(())
    })
    .await;
    github.add_review_comment(REPOSITORY, 42, None, BOT, "Rename tier to plan.");
    wait_for(async || (task_state(&engine).await.as_deref() == Some("needs_human")).then_some(()))
        .await;
    wait_for(async || {
        let labels = github.labels(REPOSITORY, 41);
        (labels.contains(&"mobius:needs-human".to_string())
            && !labels.contains(&"mobius:working".to_string()))
        .then_some(())
    })
    .await;

    github.add_comment(REPOSITORY, 42, "owner", "Continue.");

    wait_for(async || {
        let labels = github.labels(REPOSITORY, 41);
        (labels.contains(&"mobius:working".to_string())
            && !labels.contains(&"mobius:needs-human".to_string()))
        .then_some(())
    })
    .await;
}

#[tokio::test]
async fn mobius_ready_on_a_task_in_needs_human_starts_a_fix_round_on_the_same_pull_request() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect_with(
        &data_dir,
        &github,
        "max_fix_rounds = 1",
        "shell = \"true\"",
        &format!(
            "{IMPLEMENTER}\n[[prompts]]\nwhen = \"Action: fix\"\nshell = \"echo x >> plan.txt && git commit -q -am Fix\"\n"
        ),
    )
    .await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || (!inbox::list(&engine).await.unwrap().is_empty()).then_some(())).await;
    github.add_review_comment(REPOSITORY, 42, None, BOT, "Rename plan to tier.");
    wait_for(async || {
        (task_state(&engine).await.as_deref() == Some("ready_for_review")).then_some(())
    })
    .await;
    github.add_review_comment(REPOSITORY, 42, None, BOT, "Rename tier to plan.");
    wait_for(async || {
        let labels = github.labels(REPOSITORY, 41);
        (task_state(&engine).await.as_deref() == Some("needs_human")
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
    let head = git(&remote, &["rev-parse", "mobius/41"]);

    github.remove_label(REPOSITORY, 41, "mobius:needs-human", "owner");
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    let fixed = wait_for(async || {
        let fixed = git(&remote, &["rev-parse", "mobius/41"]);
        (fixed != head).then_some(fixed)
    })
    .await;
    git(&remote, &["merge-base", "--is-ancestor", &head, &fixed]);
    let task = engine
        .store
        .tasks()
        .live(REPOSITORY, 41)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.id, stopped.id);
    assert_eq!(task.branch, stopped.branch);
    assert_eq!(task.pull_request, stopped.pull_request);
    assert_ne!(task.state, "stopped");
    assert_eq!(github.pull_requests(REPOSITORY).len(), 1);
    let labels = github.labels(REPOSITORY, 41);
    assert!(labels.contains(&"mobius:working".to_string()), "{labels:?}");
    assert!(!labels.contains(&"mobius:ready".to_string()), "{labels:?}");
    assert!(
        !labels.contains(&"mobius:needs-human".to_string()),
        "{labels:?}"
    );
    let last = prompts(&engine, "implementer").await.pop().unwrap();
    assert!(last.contains("Rename tier to plan."), "{last}");
    assert!(last.contains("Action: fix"), "{last}");
}

#[tokio::test]
async fn a_removal_of_the_working_label_stops_a_task_while_the_judge_runs_from_a_state_other_than_needs_human()
 {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, "hang = true", IMPLEMENTER).await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || (!inbox::list(&engine).await.unwrap().is_empty()).then_some(())).await;
    github.add_comment(REPOSITORY, 42, "owner", "Why cents?");
    wait_for(async || (task_state(&engine).await.as_deref() == Some("working")).then_some(()))
        .await;

    github.remove_label(REPOSITORY, 41, "mobius:working", "mallory");

    wait_for(async || (task_state(&engine).await.as_deref() == Some("stopped")).then_some(()))
        .await;
}
