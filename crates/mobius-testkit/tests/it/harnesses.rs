use std::time::Duration;

use mobius_domain::{Harness, Session};
use mobius_engine::config::RoleBinding;
use mobius_engine::{Engine, chat, github, workstreams};
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::{config, start_engine, wait_for};
use serde_json::Value;
use tempfile::TempDir;

const REPOSITORY: &str = "owner/shop";
const PROMPT: &str = "Call the Mobius tool list_tasks one time. Do not call other tools. Then reply with its result.";

// A real Harness turn takes much longer than the limit of `wait_for`.
async fn ended_session(engine: &Engine) -> Session {
    tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            let sessions = engine
                .store
                .sessions()
                .list("owner", REPOSITORY, 12)
                .await
                .unwrap();
            if let Some(session) = sessions
                .into_iter()
                .find(|session| session.ended_at.is_some())
            {
                return session;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the Lead session does not end after 300 seconds")
}

async fn lead_calls_list_tasks(harness: Harness, model: &str, effort: Option<&str>) {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    let mut lead_config = config(data_dir.path(), "correct horse", "");
    lead_config.roles.lead = RoleBinding {
        harness,
        model: model.to_string(),
        effort: effort.map(str::to_string),
        max: 2,
        counts_in_max_agents: false,
    };
    let engine = start_engine(lead_config, &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || (!workstreams::list(&engine).await.unwrap().is_empty()).then_some(())).await;

    chat::send(&engine, "owner", REPOSITORY, 12, PROMPT)
        .await
        .unwrap();

    let session = ended_session(&engine).await;
    let rows = engine.store.transcript().list(session.id).await.unwrap();
    let transcript: Vec<&str> = rows.iter().map(|row| row.json.as_str()).collect();
    assert_eq!(
        session.end_reason.as_deref(),
        Some("idle"),
        "{transcript:#?}"
    );
    let tools: Vec<String> = rows
        .iter()
        .filter(|row| row.kind == "mcp_call")
        .map(|row| {
            let call: Value = serde_json::from_str(&row.json).unwrap();
            call["tool"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(tools, ["list_tasks"], "{transcript:#?}");
}

#[tokio::test]
#[ignore = "starts the real Claude Code from PATH"]
async fn claude_code_sets_the_model_and_the_effort_and_calls_a_mobius_tool() {
    lead_calls_list_tasks(Harness::ClaudeCode, "sonnet", Some("low")).await;
}

#[tokio::test]
#[ignore = "starts the real Antigravity from PATH"]
async fn antigravity_sets_the_model_and_calls_a_mobius_tool() {
    lead_calls_list_tasks(Harness::Antigravity, "gemini-3.1-pro-low", None).await;
}

#[tokio::test]
#[ignore = "starts the real Devin from PATH"]
async fn devin_sets_the_model_and_the_effort_and_calls_a_mobius_tool() {
    lead_calls_list_tasks(Harness::Devin, "claude-sonnet-5-medium", Some("low")).await;
}
