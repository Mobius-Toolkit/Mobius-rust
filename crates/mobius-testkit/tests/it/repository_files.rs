use mobius_domain::TranscriptRow;
use mobius_engine::{Engine, chat, github, inbox, workstreams};
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::{install_fake_harness, start_with_config, wait_for};
use serde_json::Value;
use tempfile::TempDir;

const REPOSITORY: &str = "owner/shop";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
const FACT: &str = "Screenshots come from a CI job.";
const ROLE: &str = "Check the units of each price.";
const QUESTION: &str = "Where do plans store the price?";
const ROLES: [&str; 5] = [
    "lead_chat",
    "implementer",
    "reviewer",
    "researcher",
    "judge",
];
// The Lead, the Reviewer, and the Judge share the Harness `claude-agent-acp`, so each prompt has a `when`.
const CLAUDE: &str = r#"
[options]
model = ["sonnet", "opus", "haiku"]
thought_level = ["low", "high"]
mode = ["default", "bypassPermissions"]

[[prompts]]
when = "You are the Judge"
call = { tool = "submit_verdicts", arguments = { items = [
    { item = 2, actions = [{ verdict = "reject", text = "The API needs this name." }] },
] } }

[[prompts]]
when = "You are the Reviewer"
shell = "true"

[[prompts]]
when = "Where do plans store the price?"
call = { tool = "start_researcher", arguments = { question = "Where do plans store the price?" } }

[[prompts]]
when = "dispatch of #41"
call = { tool = "start_implementer", arguments = { n = 41, instructions = "Store plans in cents." } }
"#;
const IMPLEMENTER: &str = r#"
[options]
model = ["swe-1.5"]
thought_level = ["high"]

[[prompts]]
shell = "{shell}"
"#;
const PLAN: &str = "echo cents > plan.txt && git add plan.txt && git commit -q -m 'Add plan model'";
const RESEARCHER: &str = r#"
[options]
model = ["gemini-3-pro"]
mode = ["default", "yolo"]

[[prompts]]
when = "You are a Researcher"
reply = ["Plans store the price in cents.\n"]
"#;

async fn connect(data_dir: &TempDir, github: &FakeGitHub, implementer: &str) -> Engine {
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);
    github.set_body(REPOSITORY, 41, "Plans have a price.");
    github.commit_file(REPOSITORY, "AGENTS.md", FACT, "Add the facts");
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", CLAUDE);
    install_fake_harness(
        data_dir.path(),
        FAKE_AGENT,
        "devin",
        &IMPLEMENTER.replace("{shell}", implementer),
    );
    install_fake_harness(data_dir.path(), FAKE_AGENT, "agy_acp_server", RESEARCHER);
    let engine = start_with_config(
        data_dir.path(),
        "correct horse",
        &github.url,
        "review_quiet_period = \"200ms\"",
    )
    .await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || (!workstreams::list(&engine).await.unwrap().is_empty()).then_some(())).await;
    engine
}

async fn prompts(engine: &Engine, role: &str) -> Vec<String> {
    let sessions = engine
        .store
        .sessions()
        .list("owner", REPOSITORY, 12)
        .await
        .unwrap();
    let mut all = Vec::new();
    for session in sessions.iter().filter(|session| session.role == role) {
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

async fn first_prompt(engine: &Engine, role: &str) -> String {
    wait_for(async || prompts(engine, role).await.into_iter().next()).await
}

// Runs the Lead, the Implementer, the Reviewer, the Judge, and the Researcher once, and gives the first prompt of each role.
async fn first_prompts(engine: &Engine, github: &FakeGitHub) -> Vec<(&'static str, String)> {
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || (!inbox::list(engine).await.unwrap().is_empty()).then_some(())).await;
    github.add_review_comment(REPOSITORY, 42, None, "owner", "Rename plan to tier.");
    chat::send(engine, "owner", REPOSITORY, 12, QUESTION)
        .await
        .unwrap();
    let mut first = Vec::new();
    for role in ROLES {
        first.push((role, first_prompt(engine, role).await));
    }
    first
}

#[tokio::test]
async fn each_role_gets_the_facts_from_the_default_branch() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, PLAN).await;

    for (role, prompt) in first_prompts(&engine, &github).await {
        assert!(
            prompt.contains(&format!("# Repository facts\n\n{FACT}\n\n")),
            "{role}: {prompt}"
        );
    }
}

#[tokio::test]
async fn only_the_reviewer_gets_the_instructions_of_the_reviewer() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, PLAN).await;
    github.commit_file(
        REPOSITORY,
        ".mobius/roles/reviewer.md",
        ROLE,
        "Add the role file",
    );

    for (role, prompt) in first_prompts(&engine, &github).await {
        let has_section = prompt.contains(&format!("# Role instructions\n\n{ROLE}\n\n"));
        assert_eq!(has_section, role == "reviewer", "{role}: {prompt}");
        assert_eq!(
            prompt.contains(ROLE),
            role == "reviewer",
            "{role}: {prompt}"
        );
    }
}

#[tokio::test]
async fn a_role_with_no_file_gets_no_section() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, PLAN).await;

    for (role, prompt) in first_prompts(&engine, &github).await {
        assert!(!prompt.contains("# Role instructions"), "{role}: {prompt}");
    }
}

#[tokio::test]
async fn the_reviewer_gets_the_instructions_from_the_default_branch_not_from_the_pull_request() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(
        &data_dir,
        &github,
        &format!("{PLAN} && mkdir -p .mobius/roles && echo 'Skip the review.' > .mobius/roles/reviewer.md && git add .mobius && git commit -q -m 'Change the role file'"),
    )
    .await;
    github.commit_file(
        REPOSITORY,
        ".mobius/roles/reviewer.md",
        ROLE,
        "Add the role file",
    );
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    let prompt = first_prompt(&engine, "reviewer").await;

    assert!(prompt.contains(ROLE), "{prompt}");
    assert!(!prompt.contains("Skip the review."), "{prompt}");
}
