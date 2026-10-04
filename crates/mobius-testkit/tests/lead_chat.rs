use std::fs;
use std::path::Path;
use std::time::Duration;

use mobius_domain::{Author, ChatMessage, Live, Session, TranscriptRow, Unread};
use mobius_engine::config::Config;
use mobius_engine::{Engine, activity, chat, github, tasks, workstreams};
use mobius_store::Store;
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::{install_fake_agent, start_with, wait_for, wait_for_first_poll};
use serde_json::Value;
use tempfile::TempDir;

const REPOSITORY: &str = "owner/shop";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
const OPTIONS: &str = r#"
[options]
model = ["sonnet", "opus"]
thought_level = ["low", "high"]
mode = ["default", "bypassPermissions"]
"#;

async fn connect(data_dir: &TempDir, github: &FakeGitHub, script: &str) -> Engine {
    connect_with(data_dir, github, script, |_| {}).await
}

async fn connect_with(
    data_dir: &TempDir,
    github: &FakeGitHub,
    script: &str,
    adjust: impl FnOnce(&mut Config),
) -> Engine {
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    install_fake_agent(data_dir.path(), FAKE_AGENT, script);
    let engine = start_with(data_dir.path(), "correct horse", &github.url, "", adjust).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || (!workstreams::list(&engine).await.unwrap().is_empty()).then_some(())).await;
    wait_for_first_poll(&engine, REPOSITORY).await;
    engine
}

async fn messages(engine: &Engine) -> Vec<ChatMessage> {
    engine
        .store
        .chat_messages()
        .list("owner", REPOSITORY, 12)
        .await
        .unwrap()
}

async fn wait_for_lead_text(engine: &Engine, text: &str) {
    wait_for(async || {
        messages(engine)
            .await
            .iter()
            .any(|message| message.author == Author::Lead && message.text == text)
            .then_some(())
    })
    .await;
}

async fn sessions(engine: &Engine) -> Vec<Session> {
    engine
        .store
        .sessions()
        .list("owner", REPOSITORY, 12)
        .await
        .unwrap()
}

async fn task_numbers(engine: &Engine, workstream: i64) -> Vec<i64> {
    tasks::list(engine, REPOSITORY, workstream)
        .await
        .unwrap()
        .into_iter()
        .map(|line| line.number)
        .collect()
}

async fn ended_session(engine: &Engine, index: usize) -> Session {
    wait_for(async || {
        sessions(engine)
            .await
            .into_iter()
            .nth(index)
            .filter(|session| session.ended_at.is_some())
    })
    .await
}

async fn transcript(engine: &Engine, session: i64) -> Vec<TranscriptRow> {
    engine.store.transcript().list(session).await.unwrap()
}

fn json(row: &TranscriptRow) -> Value {
    serde_json::from_str(&row.json).unwrap()
}

fn prompts(rows: &[TranscriptRow]) -> Vec<String> {
    rows.iter()
        .filter(|row| row.kind == "prompt")
        .map(|row| json(row)["text"].as_str().unwrap().to_string())
        .collect()
}

fn session_updates(rows: &[TranscriptRow], kind: &str) -> Vec<Value> {
    rows.iter()
        .filter(|row| row.kind == "update")
        .map(|row| json(row)["update"].clone())
        .filter(|update| update["sessionUpdate"] == kind)
        .collect()
}

fn env_value(data_dir: &Path, name: &str) -> String {
    fs::read_to_string(data_dir.join("harnesses/env"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name}=")))
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn an_owner_message_gets_the_lead_reply_in_the_store() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let script = format!("{OPTIONS}\n[[prompts]]\nreply = [\"Hello\", \" there\"]\n");
    let engine = connect(&data_dir, &github, &script).await;

    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the loyalty API")
        .await
        .unwrap();

    wait_for_lead_text(&engine, "Hello there").await;
    let messages = messages(&engine).await;
    assert_eq!(
        messages
            .iter()
            .map(|message| (message.author, message.text.as_str()))
            .collect::<Vec<_>>(),
        [
            (Author::Owner, "Plan the loyalty API"),
            (Author::Lead, "Hello there")
        ]
    );
    let session = ended_session(&engine, 0).await;
    let chunks = session_updates(
        &transcript(&engine, session.id).await,
        "agent_message_chunk",
    );
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0]["content"]["text"], "Hello there");
}

#[tokio::test]
async fn the_harness_process_gets_the_agent_env_guard_in_the_lead_directory() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let script = format!("{OPTIONS}\n[[prompts]]\nreply = [\"Hello\"]\n");
    let engine = connect(&data_dir, &github, &script).await;

    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the loyalty API")
        .await
        .unwrap();

    wait_for_lead_text(&engine, "Hello").await;
    let data_dir = data_dir.path();
    let agent_env = data_dir.join("agent-env");
    assert_eq!(
        fs::read_to_string(data_dir.join("harnesses/pwd"))
            .unwrap()
            .trim(),
        data_dir
            .join("leads/owner/shop/12")
            .canonicalize()
            .unwrap()
            .display()
            .to_string()
    );
    let gh_config = env_value(data_dir, "GH_CONFIG_DIR");
    assert_eq!(gh_config, agent_env.join("gh-config").display().to_string());
    assert_eq!(fs::read_dir(&gh_config).unwrap().count(), 0);
    assert_eq!(
        env_value(data_dir, "GIT_CONFIG_GLOBAL"),
        agent_env.join("gitconfig").display().to_string()
    );
    assert_eq!(env_value(data_dir, "GIT_TERMINAL_PROMPT"), "0");
    let path = env_value(data_dir, "PATH");
    let first = path.split(':').next().unwrap();
    assert_eq!(first, agent_env.join("chat-bin").display().to_string());
}

#[tokio::test]
async fn the_session_gets_the_model_the_effort_and_the_full_auto_mode() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let script = format!("{OPTIONS}\n[[prompts]]\nreply = [\"Hello\"]\n");
    let engine = connect(&data_dir, &github, &script).await;

    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the loyalty API")
        .await
        .unwrap();

    let session = ended_session(&engine, 0).await;
    let updates = session_updates(
        &transcript(&engine, session.id).await,
        "config_option_update",
    );
    let values: Vec<(String, String)> = updates.last().unwrap()["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|option| {
            (
                option["id"].as_str().unwrap().to_string(),
                option["currentValue"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        values,
        [
            ("mode".to_string(), "bypassPermissions".to_string()),
            ("model".to_string(), "opus".to_string()),
            ("thought_level".to_string(), "high".to_string()),
        ]
    );
}

#[tokio::test]
async fn a_refused_model_ends_the_session_before_the_first_prompt() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let script = OPTIONS.replace("[\"sonnet\", \"opus\"]", "[\"sonnet\", \"haiku\"]");
    let engine = connect(&data_dir, &github, &script).await;

    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the loyalty API")
        .await
        .unwrap();

    let session = ended_session(&engine, 0).await;
    assert_eq!(session.end_reason.as_deref(), Some("failed"));
    let rows = transcript(&engine, session.id).await;
    assert!(prompts(&rows).is_empty());
    let error = rows.iter().find(|row| row.kind == "error").unwrap();
    assert_eq!(
        json(error)["message"],
        "The Harness refuses model \"opus\". The Harness has: sonnet, haiku."
    );
    assert!(
        !chat::view(&engine, "owner", REPOSITORY, 12)
            .await
            .unwrap()
            .writing
    );
}

#[tokio::test]
async fn a_harness_that_needs_a_login_fails_the_session() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let script = format!("login_required = true\n{OPTIONS}");
    let engine = connect(&data_dir, &github, &script).await;

    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the loyalty API")
        .await
        .unwrap();

    let session = ended_session(&engine, 0).await;
    assert_eq!(session.end_reason.as_deref(), Some("failed"));
    let rows = transcript(&engine, session.id).await;
    let error = rows.iter().find(|row| row.kind == "error").unwrap();
    assert_eq!(json(error)["message"], "Authentication required");
}

#[tokio::test]
async fn the_first_prompt_has_the_context_parts_in_order() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, OPTIONS).await;
    github.set_body(REPOSITORY, 12, "Ship loyalty plans to all shops.");
    // A pull request keeps `mobius:working` while the recovery takes a working
    // label with no live task as a lost task and changes it to needs-human.
    github.add_pull_request(REPOSITORY, 41, "Add plan model");
    github.add_label(REPOSITORY, 41, "mobius:working", "owner");
    github.add_issue(REPOSITORY, 42, "Old spike");
    github.close_issue(REPOSITORY, 42);
    github.add_issue(REPOSITORY, 43, "Plan API");
    github.add_issue(REPOSITORY, 50, "Billing");
    github.add_label(REPOSITORY, 50, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 51, "Invoice totals");
    github.add_sub_issue(REPOSITORY, 12, 41);
    github.add_sub_issue(REPOSITORY, 12, 42);
    github.add_sub_issue(REPOSITORY, 41, 43);
    github.add_sub_issue(REPOSITORY, 12, 50);
    github.add_sub_issue(REPOSITORY, 50, 51);
    let lead_dir = data_dir.path().join("leads/owner/shop/12");
    fs::create_dir_all(&lead_dir).unwrap();
    let memory: Vec<String> = (1..=201).map(|line| format!("note {line}")).collect();
    fs::write(lead_dir.join("MEMORY.md"), memory.join("\n")).unwrap();
    for number in 1..=21 {
        let author = if number % 2 == 1 {
            Author::Owner
        } else {
            Author::Lead
        };
        engine
            .store
            .chat_messages()
            .add(
                "owner",
                REPOSITORY,
                12,
                author,
                &format!("message {number}"),
            )
            .await
            .unwrap();
    }

    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the next step")
        .await
        .unwrap();

    let session = ended_session(&engine, 0).await;
    let prompt = prompts(&transcript(&engine, session.id).await)[0].clone();
    let parts = [
        "You are the Lead of one Workstream.",
        "# Brief\n\nShip loyalty plans to all shops.\n",
        "# MEMORY.md\n\nnote 1\n",
        "note 200\nMEMORY.md is too long. Make it shorter.\n",
        "# Task list\n\n#41 Add plan model: working\n#43 Plan API: open\n\n",
        "# Chat history\n\nLead (",
        "):\nmessage 2\n\n",
        "):\nmessage 21\n\n",
        "# Owner message\n\nPlan the next step",
    ];
    let positions: Vec<usize> = parts
        .iter()
        .map(|part| {
            prompt
                .find(part)
                .unwrap_or_else(|| panic!("{part:?} in {prompt}"))
        })
        .collect();
    assert!(positions.is_sorted(), "{prompt}");
    assert!(prompt.ends_with("Plan the next step"));
    assert!(!prompt.contains("note 201"));
    assert!(!prompt.contains("message 1\n"));
    assert!(!prompt.contains("Old spike"));
    assert!(!prompt.contains("Invoice totals"));
}

#[tokio::test]
async fn a_new_owner_message_waits_for_the_turn_and_stop_ends_the_turn() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let script = format!(
        "{OPTIONS}\n[[prompts]]\nhang = true\n\n[[prompts]]\nreply = [\"After the stop\"]\n"
    );
    let engine = connect(&data_dir, &github, &script).await;
    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the loyalty API")
        .await
        .unwrap();
    let session = wait_for(async || {
        let session = sessions(&engine).await.into_iter().next()?;
        (prompts(&transcript(&engine, session.id).await).len() == 1).then_some(session)
    })
    .await;

    chat::send(&engine, "owner", REPOSITORY, 12, "Also add a plan price")
        .await
        .unwrap();

    assert!(
        chat::view(&engine, "owner", REPOSITORY, 12)
            .await
            .unwrap()
            .writing
    );
    assert_eq!(prompts(&transcript(&engine, session.id).await).len(), 1);

    chat::stop(&engine, "owner", REPOSITORY, 12).unwrap();

    wait_for_lead_text(&engine, "After the stop").await;
    let session = ended_session(&engine, 0).await;
    assert_eq!(session.end_reason.as_deref(), Some("idle"));
    assert_eq!(
        prompts(&transcript(&engine, session.id).await)[1],
        "Also add a plan price"
    );
}

#[tokio::test]
async fn an_idle_session_saves_and_closes_and_the_next_message_starts_a_new_session() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let script = format!("{OPTIONS}\n[[prompts]]\nreply = [\"First answer\"]\n");
    let engine = connect(&data_dir, &github, &script).await;
    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the loyalty API")
        .await
        .unwrap();

    let first = ended_session(&engine, 0).await;

    assert_eq!(first.end_reason.as_deref(), Some("idle"));
    assert_eq!(
        prompts(&transcript(&engine, first.id).await)
            .last()
            .unwrap(),
        "Save in the Workstream memory what the next session needs."
    );

    chat::send(&engine, "owner", REPOSITORY, 12, "Add a plan price")
        .await
        .unwrap();

    let second = ended_session(&engine, 1).await;
    let prompt = prompts(&transcript(&engine, second.id).await)[0].clone();
    let history = &prompt[prompt.find("# Chat history").unwrap()..];
    assert!(history.contains("):\nPlan the loyalty API\n"), "{history}");
    assert!(history.contains("):\nFirst answer\n"), "{history}");
    assert!(history.ends_with("# Owner message\n\nAdd a plan price"));
}

#[tokio::test]
async fn a_lead_reply_is_unread_until_the_owner_sees_it() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let script = format!("{OPTIONS}\n[[prompts]]\nreply = [\"Hello\"]\n");
    let engine = connect(&data_dir, &github, &script).await;
    let mut feed = activity::feed(&engine, None).await.unwrap();

    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the loyalty API")
        .await
        .unwrap();

    wait_for_lead_text(&engine, "Hello").await;
    let unread = Unread {
        organization: "owner".to_string(),
        repository: REPOSITORY.to_string(),
        workstream: 12,
        count: 1,
    };
    assert_eq!(
        engine.store.chat_messages().unread().await.unwrap(),
        std::slice::from_ref(&unread)
    );
    let lead = messages(&engine).await.pop().unwrap();

    chat::seen(&engine, "owner", REPOSITORY, 12, lead.id)
        .await
        .unwrap();

    assert!(
        engine
            .store
            .chat_messages()
            .unread()
            .await
            .unwrap()
            .is_empty()
    );
    let seen = Unread { count: 0, ..unread };
    tokio::time::timeout(Duration::from_secs(5), async {
        while feed.next().await.unwrap() != Live::Unread(seen.clone()) {}
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_stop_before_the_first_turn_ends_the_waiting_session() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let script = format!("{OPTIONS}\n[[prompts]]\nhang = true\n");
    let engine = connect(&data_dir, &github, &script).await;

    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the loyalty API")
        .await
        .unwrap();
    chat::stop(&engine, "owner", REPOSITORY, 12).unwrap();

    let session = ended_session(&engine, 0).await;
    assert_eq!(session.end_reason.as_deref(), Some("stopped"));
    // The Harness process never starts, so the first turn never runs.
    assert!(prompts(&transcript(&engine, session.id).await).is_empty());
    assert!(
        !chat::view(&engine, "owner", REPOSITORY, 12)
            .await
            .unwrap()
            .writing
    );
}

#[tokio::test]
async fn the_lead_chat_creates_a_workstream_after_the_approval() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let script = format!(
        "{OPTIONS}\n{}",
        r##"
[[prompts]]
when = "# Event\n\n"
list_tools = true

[[prompts]]
when = "# Owner message\n\nMove the API work to a new Workstream."
reply = ["Title: Shop API\n\nBrief: The public API of the shop. The context is in #12."]

[[prompts]]
when = "Yes, create it."
call = { tool = "create_workstream", arguments = { title = "Shop API", brief = "The public API of the shop. The context is in #12." } }
"##
    );
    let engine = connect(&data_dir, &github, &script).await;
    let mut feed = activity::feed(&engine, None).await.unwrap();

    chat::send(
        &engine,
        "owner",
        REPOSITORY,
        12,
        "Move the API work to a new Workstream.",
    )
    .await
    .unwrap();
    wait_for_lead_text(
        &engine,
        "Title: Shop API\n\nBrief: The public API of the shop. The context is in #12.",
    )
    .await;
    chat::send(&engine, "owner", REPOSITORY, 12, "Yes, create it.")
        .await
        .unwrap();

    wait_for_lead_text(&engine, "Created the Workstream #13.").await;
    assert_eq!(
        github.issue(REPOSITORY, 13),
        (
            "Shop API".to_string(),
            "The public API of the shop. The context is in #12.".to_string()
        )
    );
    assert_eq!(
        github.labels(REPOSITORY, 13),
        ["mobius:workstream".to_string()]
    );
    // `Live::Workstreams` refreshes the sidebar, and the chat does not move to a different screen.
    tokio::time::timeout(Duration::from_secs(5), async {
        while feed.next().await.unwrap() != Live::Workstreams {}
    })
    .await
    .unwrap();
    let list = wait_for(async || {
        let list = workstreams::list(&engine).await.unwrap();
        (list.len() == 2).then_some(list)
    })
    .await;
    assert!(
        list.iter()
            .any(|workstream| workstream.number == 13 && workstream.title == "Shop API"),
        "{list:?}"
    );

    // The Lead of the new Workstream has the same tools as the Lead of the first Workstream.
    let tools = wait_for(async || {
        let session = engine
            .store
            .sessions()
            .list("owner", REPOSITORY, 13)
            .await
            .unwrap()
            .into_iter()
            .find(|session| session.role == "lead_chat")?;
        transcript(&engine, session.id)
            .await
            .iter()
            .find_map(|row| {
                let update = json(row);
                (update["update"]["sessionUpdate"] == "agent_message_chunk").then(|| {
                    update["update"]["content"]["text"]
                        .as_str()
                        .unwrap()
                        .to_string()
                })
            })
    })
    .await;
    let result: Value = serde_json::from_str(&tools).unwrap();
    let names: Vec<&str> = result["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for name in ["tell_owner", "create_workstream", "move_task"] {
        assert!(names.contains(&name), "{names:?}");
    }
}

#[tokio::test]
async fn the_lead_chat_creates_a_workstream_and_moves_a_task_to_it_after_the_approvals() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    // The first prompt of a new session has the earlier messages in its history, so the prompt of the newest message comes first.
    let script = format!(
        "{OPTIONS}\n{}",
        r##"
[[prompts]]
when = "# Event\n\n"
reply = ["Noted."]

[[prompts]]
when = "Yes, move it."
call = { tool = "move_task", arguments = { n = 13, workstream = 15 } }

[[prompts]]
when = "Yes, create it."
call = { tool = "create_workstream", arguments = { title = "Shop API", brief = "The public API of the shop. The context is in #12." } }
"##
    );
    let engine = connect(&data_dir, &github, &script).await;
    github.add_issue(REPOSITORY, 13, "Add the API route");
    github.add_issue(REPOSITORY, 14, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 13);
    github.add_sub_issue(REPOSITORY, 12, 14);
    let mut feed = activity::feed(&engine, None).await.unwrap();

    chat::send(&engine, "owner", REPOSITORY, 12, "Yes, create it.")
        .await
        .unwrap();
    wait_for_lead_text(&engine, "Created the Workstream #15.").await;
    chat::send(&engine, "owner", REPOSITORY, 12, "Yes, move it.")
        .await
        .unwrap();

    wait_for_lead_text(&engine, "Moved #13 to the Workstream #15.").await;
    assert_eq!(
        github.issue(REPOSITORY, 15),
        (
            "Shop API".to_string(),
            "The public API of the shop. The context is in #12.".to_string()
        )
    );
    assert_eq!(
        github.labels(REPOSITORY, 15),
        ["mobius:workstream".to_string()]
    );
    assert_eq!(github.sub_issue_numbers(REPOSITORY, 15), [13]);
    assert_eq!(github.sub_issue_numbers(REPOSITORY, 12), [14]);
    // Each `Live::Workstreams` event refreshes the sidebar and an open Tasks tab, and the chat does not move to a different screen.
    let mut events = 0;
    tokio::time::timeout(Duration::from_secs(5), async {
        while events < 2 {
            if feed.next().await.unwrap() == Live::Workstreams {
                events += 1;
            }
        }
    })
    .await
    .unwrap();
    let list = workstreams::list(&engine).await.unwrap();
    assert!(
        list.iter()
            .any(|workstream| workstream.number == 15 && workstream.title == "Shop API"),
        "{list:?}"
    );
    assert_eq!(task_numbers(&engine, 12).await, [14]);
    assert_eq!(task_numbers(&engine, 15).await, [13]);
}

#[tokio::test]
async fn the_lead_chat_refuses_to_move_a_task_when_the_task_or_the_target_does_not_fit() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    // The first prompt of a new session has the earlier messages in its history, so the prompt of the newest message comes first.
    let script = format!(
        "{OPTIONS}\n{}",
        r##"
[[prompts]]
when = "case-live"
call = { tool = "move_task", arguments = { n = 13, workstream = 20 } }

[[prompts]]
when = "case-plain"
call = { tool = "move_task", arguments = { n = 13, workstream = 14 } }

[[prompts]]
when = "case-closed"
call = { tool = "move_task", arguments = { n = 13, workstream = 22 } }

[[prompts]]
when = "case-self"
call = { tool = "move_task", arguments = { n = 13, workstream = 12 } }

[[prompts]]
when = "case-pull"
call = { tool = "move_task", arguments = { n = 15, workstream = 20 } }

[[prompts]]
when = "case-other"
call = { tool = "move_task", arguments = { n = 21, workstream = 20 } }
"##
    );
    let engine = connect(&data_dir, &github, &script).await;
    github.add_issue(REPOSITORY, 13, "Add the API route");
    github.add_issue(REPOSITORY, 14, "Add plan model");
    github.add_pull_request(REPOSITORY, 15, "Add the API route");
    github.add_sub_issue(REPOSITORY, 12, 13);
    github.add_sub_issue(REPOSITORY, 12, 14);
    github.add_sub_issue(REPOSITORY, 12, 15);
    github.add_issue(REPOSITORY, 20, "Billing");
    github.add_label(REPOSITORY, 20, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 21, "Invoice totals");
    github.add_sub_issue(REPOSITORY, 20, 21);
    github.add_issue(REPOSITORY, 22, "Old Billing");
    github.add_label(REPOSITORY, 22, "mobius:workstream", "owner");
    github.close_issue(REPOSITORY, 22);

    for (message, result) in [
        ("case-other", "error: #21 is not in this Workstream."),
        ("case-pull", "error: #15 is not an issue of owner/shop."),
        ("case-self", "error: #12 is this Workstream."),
        ("case-closed", "error: #22 is not an open Workstream."),
        ("case-plain", "error: #14 is not an open Workstream."),
    ] {
        chat::send(&engine, "owner", REPOSITORY, 12, message)
            .await
            .unwrap();
        wait_for_lead_text(&engine, result).await;
    }
    engine.store.tasks().add(REPOSITORY, 13, 12).await.unwrap();
    chat::send(&engine, "owner", REPOSITORY, 12, "case-live")
        .await
        .unwrap();

    wait_for_lead_text(&engine, "error: #13 has a live task. Stop the task first.").await;
    assert_eq!(github.sub_issue_numbers(REPOSITORY, 12), [13, 14, 15]);
    assert_eq!(github.sub_issue_numbers(REPOSITORY, 20), [21]);
}

fn dispatch_task(github: &FakeGitHub, number: i64, title: &str) {
    github.add_issue(REPOSITORY, number, title);
    github.add_sub_issue(REPOSITORY, 12, number);
    github.add_label(REPOSITORY, number, "mobius:ready", "owner");
}

fn keep_session_open(config: &mut Config) {
    config.lead_idle_timeout = Duration::from_secs(30);
}

async fn event_delivered(engine: &Engine) -> bool {
    messages(engine)
        .await
        .iter()
        .any(|message| message.author == Author::Event)
        && engine
            .store
            .lead_events()
            .undelivered(REPOSITORY, 12)
            .await
            .unwrap()
            .is_empty()
}

#[tokio::test]
async fn the_lead_gets_an_event_from_github_and_the_next_owner_question_in_the_same_session() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let script = format!(
        "{OPTIONS}\n[[prompts]]\nreply = [\"Noted.\"]\n\n[[prompts]]\nreply = [\"The Owner dispatched #41.\"]\n"
    );
    let engine = connect_with(&data_dir, &github, &script, keep_session_open).await;

    dispatch_task(&github, 41, "Add plan model");
    wait_for(async || event_delivered(&engine).await.then_some(())).await;
    chat::send(&engine, "owner", REPOSITORY, 12, "What happened with #41?")
        .await
        .unwrap();

    wait_for_lead_text(&engine, "The Owner dispatched #41.").await;
    let sessions = sessions(&engine).await;
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    let prompts = prompts(&transcript(&engine, sessions[0].id).await);
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    let (_, event) = prompts[0].rsplit_once("# Event\n\n").unwrap();
    assert!(
        event.contains(" dispatch of #41 \"Add plan model\" by @owner:"),
        "{event}"
    );
    assert_eq!(prompts[1], "What happened with #41?");
}

#[tokio::test]
async fn the_reply_text_of_an_event_turn_goes_only_to_the_transcript() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let script = format!(
        "{OPTIONS}\n[[prompts]]\nreply = [\"Noted.\"]\n\n[[prompts]]\nreply = [\"The Owner dispatched #41.\"]\n"
    );
    let engine = connect_with(&data_dir, &github, &script, keep_session_open).await;

    dispatch_task(&github, 41, "Add plan model");
    wait_for(async || event_delivered(&engine).await.then_some(())).await;
    chat::send(&engine, "owner", REPOSITORY, 12, "What happened with #41?")
        .await
        .unwrap();

    wait_for_lead_text(&engine, "The Owner dispatched #41.").await;
    let lead_texts: Vec<String> = messages(&engine)
        .await
        .into_iter()
        .filter(|message| message.author == Author::Lead)
        .map(|message| message.text)
        .collect();
    assert_eq!(lead_texts, ["The Owner dispatched #41."]);
    let session = sessions(&engine).await.remove(0);
    let chunks = session_updates(
        &transcript(&engine, session.id).await,
        "agent_message_chunk",
    );
    assert_eq!(chunks[0]["content"]["text"], "Noted.");
}

#[tokio::test]
async fn an_event_during_a_turn_for_an_owner_message_gets_its_own_turn_after_that_turn() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let script = format!(
        "{OPTIONS}\n[[prompts]]\nwhen = \"Plan the API\"\nhang = true\n\n[[prompts]]\nreply = [\"Noted.\"]\n"
    );
    let engine = connect_with(&data_dir, &github, &script, keep_session_open).await;
    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the API")
        .await
        .unwrap();
    wait_for(async || {
        let sessions = sessions(&engine).await;
        let first = sessions.first()?;
        (prompts(&transcript(&engine, first.id).await).len() == 1).then_some(())
    })
    .await;

    dispatch_task(&github, 41, "Add plan model");
    wait_for(async || {
        messages(&engine)
            .await
            .iter()
            .any(|message| message.author == Author::Event)
            .then_some(())
    })
    .await;
    let session = sessions(&engine).await.remove(0);
    assert_eq!(prompts(&transcript(&engine, session.id).await).len(), 1);
    assert!(!event_delivered(&engine).await);
    chat::stop(&engine, "owner", REPOSITORY, 12).unwrap();

    let prompts = wait_for(async || {
        let prompts = prompts(&transcript(&engine, session.id).await);
        (prompts.len() == 2 && event_delivered(&engine).await).then_some(prompts)
    })
    .await;
    assert!(
        prompts[0].ends_with("# Owner message\n\nPlan the API"),
        "{}",
        prompts[0]
    );
    assert!(prompts[1].starts_with("# Event\n\n"), "{}", prompts[1]);
    assert!(prompts[1].contains(" dispatch of #41 "), "{}", prompts[1]);
    assert_eq!(sessions(&engine).await.len(), 1);
}

#[tokio::test]
async fn an_event_keeps_its_place_before_a_later_owner_message_when_the_lead_held_no_event() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let script = format!(
        "{OPTIONS}\n[[prompts]]\nwhen = \"Plan the API\"\nhang = true\n\n[[prompts]]\nreply = [\"Noted.\"]\n\n[[prompts]]\nreply = [\"Done.\"]\n"
    );
    let engine = connect_with(&data_dir, &github, &script, keep_session_open).await;
    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the API")
        .await
        .unwrap();
    wait_for(async || {
        let sessions = sessions(&engine).await;
        let first = sessions.first()?;
        (prompts(&transcript(&engine, first.id).await).len() == 1).then_some(())
    })
    .await;
    dispatch_task(&github, 41, "Add plan model");
    wait_for(async || (event_count(&engine).await == 1).then_some(())).await;
    chat::send(&engine, "owner", REPOSITORY, 12, "Also add a price")
        .await
        .unwrap();
    chat::stop(&engine, "owner", REPOSITORY, 12).unwrap();

    let prompts = wait_for(async || {
        let prompts = event_prompts(&engine).await;
        (prompts.len() == 3 && event_delivered(&engine).await).then_some(prompts)
    })
    .await;
    assert!(prompts[1].contains(" dispatch of #41 "), "{}", prompts[1]);
    assert_eq!(prompts[2], "Also add a price");
}

#[tokio::test]
async fn an_event_and_an_owner_message_make_one_lead_session_for_the_workstream() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let script = format!(
        "{OPTIONS}\n[[prompts]]\nreply = [\"Noted.\"]\n\n[[prompts]]\nreply = [\"Hello\"]\n"
    );
    let engine = connect_with(&data_dir, &github, &script, keep_session_open).await;

    dispatch_task(&github, 41, "Add plan model");
    wait_for(async || event_delivered(&engine).await.then_some(())).await;
    chat::send(&engine, "owner", REPOSITORY, 12, "Hello")
        .await
        .unwrap();

    wait_for_lead_text(&engine, "Hello").await;
    let sessions = sessions(&engine).await;
    assert_eq!(
        sessions
            .iter()
            .map(|session| session.role.as_str())
            .collect::<Vec<_>>(),
        ["lead_chat"]
    );
}

#[tokio::test]
async fn a_stop_does_not_cancel_an_event_turn_and_the_event_stays_undelivered_until_the_turn_ends()
{
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let script = format!("{OPTIONS}\n[[prompts]]\nhang = true\n");
    let engine = connect_with(&data_dir, &github, &script, keep_session_open).await;
    dispatch_task(&github, 41, "Add plan model");
    wait_for(async || {
        let session = sessions(&engine).await.into_iter().next()?;
        (prompts(&transcript(&engine, session.id).await).len() == 1).then_some(())
    })
    .await;

    chat::stop(&engine, "owner", REPOSITORY, 12).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let session = sessions(&engine).await.remove(0);
    assert!(session.ended_at.is_none(), "{session:?}");
    assert_eq!(
        engine
            .store
            .lead_events()
            .undelivered(REPOSITORY, 12)
            .await
            .unwrap()
            .len(),
        1
    );
}

async fn event_prompts(engine: &Engine) -> Vec<String> {
    let sessions = sessions(engine).await;
    let prompts = prompts(&transcript(engine, sessions[0].id).await);
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    prompts
}

async fn event_count(engine: &Engine) -> usize {
    messages(engine)
        .await
        .iter()
        .filter(|message| message.author == Author::Event)
        .count()
}

async fn held(engine: &Engine, events: usize) -> bool {
    let store = engine.store.lead_events();
    store.undelivered(REPOSITORY, 12).await.unwrap().len() == events
        && store.ready(REPOSITORY, 12).await.unwrap().is_empty()
}

#[tokio::test]
async fn a_held_event_returns_after_each_turn_for_an_owner_message_with_the_later_events_of_its_task_behind_it()
 {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let hold = "call = { tool = \"hold_event\", arguments = {} }";
    let script = format!(
        "{OPTIONS}
[[prompts]]
{hold}

[[prompts]]
reply = [\"Noted.\"]

[[prompts]]
reply = [\"Owner one.\"]

[[prompts]]
{hold}

[[prompts]]
reply = [\"Owner two.\"]

[[prompts]]
reply = [\"Noted.\"]

[[prompts]]
reply = [\"Noted.\"]
"
    );
    let engine = connect_with(&data_dir, &github, &script, keep_session_open).await;

    dispatch_task(&github, 41, "Add plan model");
    wait_for(async || held(&engine, 1).await.then_some(())).await;
    dispatch_task(&github, 42, "Add plan route");
    wait_for(async || (event_count(&engine).await == 2 && held(&engine, 1).await).then_some(()))
        .await;
    github.add_comment(REPOSITORY, 41, "owner", "Round down.");
    wait_for(async || (event_count(&engine).await == 3).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(event_prompts(&engine).await.len(), 2);
    assert!(held(&engine, 2).await);

    chat::send(&engine, "owner", REPOSITORY, 12, "First decision")
        .await
        .unwrap();
    wait_for(async || (event_prompts(&engine).await.len() == 4).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let prompts = event_prompts(&engine).await;
    assert_eq!(prompts.len(), 4, "{prompts:?}");
    assert!(prompts[1].contains(" dispatch of #42 "), "{}", prompts[1]);
    assert_eq!(prompts[2], "First decision");
    assert!(prompts[3].starts_with("# Event\n\n"), "{}", prompts[3]);
    assert!(prompts[3].contains(" dispatch of #41 "), "{}", prompts[3]);
    assert!(held(&engine, 2).await);

    chat::send(&engine, "owner", REPOSITORY, 12, "Second decision")
        .await
        .unwrap();
    let prompts = wait_for(async || {
        let prompts = event_prompts(&engine).await;
        (prompts.len() == 7 && event_delivered(&engine).await).then_some(prompts)
    })
    .await;
    assert_eq!(prompts[4], "Second decision");
    assert!(prompts[5].contains(" dispatch of #41 "), "{}", prompts[5]);
    assert!(prompts[6].contains(" comment on #41 "), "{}", prompts[6]);
}

#[tokio::test]
async fn a_freed_event_does_not_move_a_later_event_of_another_task_before_a_queued_owner_message() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let script = format!(
        "{OPTIONS}
[[prompts]]
call = {{ tool = \"hold_event\", arguments = {{}} }}

[[prompts]]
reply = [\"Noted.\"]

[[prompts]]
when = \"First decision\"
hang = true

[[prompts]]
reply = [\"Noted.\"]

[[prompts]]
reply = [\"Noted.\"]

[[prompts]]
reply = [\"Noted.\"]

[[prompts]]
reply = [\"Noted.\"]
"
    );
    let engine = connect_with(&data_dir, &github, &script, keep_session_open).await;
    dispatch_task(&github, 41, "Add plan model");
    wait_for(async || held(&engine, 1).await.then_some(())).await;
    chat::send(&engine, "owner", REPOSITORY, 12, "First decision")
        .await
        .unwrap();
    wait_for(async || (event_prompts(&engine).await.len() == 2).then_some(())).await;
    chat::send(&engine, "owner", REPOSITORY, 12, "Second decision")
        .await
        .unwrap();
    dispatch_task(&github, 42, "Add plan route");
    wait_for(async || (event_count(&engine).await == 2).then_some(())).await;
    chat::stop(&engine, "owner", REPOSITORY, 12).unwrap();

    let prompts = wait_for(async || {
        let prompts = event_prompts(&engine).await;
        (prompts.len() == 5 && event_delivered(&engine).await).then_some(prompts)
    })
    .await;
    assert_eq!(prompts[1], "First decision");
    assert!(prompts[2].contains(" dispatch of #41 "), "{}", prompts[2]);
    assert_eq!(prompts[3], "Second decision");
    assert!(prompts[4].contains(" dispatch of #42 "), "{}", prompts[4]);
}

#[tokio::test]
async fn a_held_event_stays_held_after_a_restart_until_a_turn_for_an_owner_message_ends() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    {
        let store = Store::open(data_dir.path()).await.unwrap();
        let events = store.lead_events();
        events
            .add(
                "owner",
                REPOSITORY,
                12,
                Some(41),
                "comment",
                "A held comment.",
            )
            .await
            .unwrap();
        let id = events.undelivered(REPOSITORY, 12).await.unwrap()[0].id;
        events.hold(id).await.unwrap();
    }
    let script = format!(
        "{OPTIONS}\n[[prompts]]\nreply = [\"Owner one.\"]\n\n[[prompts]]\nreply = [\"Noted.\"]\n"
    );
    let engine = connect_with(&data_dir, &github, &script, keep_session_open).await;

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(sessions(&engine).await.is_empty());
    assert!(held(&engine, 1).await);
    chat::send(&engine, "owner", REPOSITORY, 12, "Decision")
        .await
        .unwrap();

    wait_for(async || all_delivered(&engine).await.then_some(())).await;
    let prompts = event_prompts(&engine).await;
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    assert!(
        prompts[0].ends_with("# Owner message\n\nDecision"),
        "{}",
        prompts[0]
    );
    assert_eq!(prompts[1], "# Event\n\nA held comment.");
}

async fn all_delivered(engine: &Engine) -> bool {
    engine
        .store
        .lead_events()
        .undelivered(REPOSITORY, 12)
        .await
        .unwrap()
        .is_empty()
}

#[tokio::test]
async fn hold_event_in_a_turn_for_an_owner_message_gives_an_error() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let script =
        format!("{OPTIONS}\n[[prompts]]\ncall = {{ tool = \"hold_event\", arguments = {{}} }}\n");
    let engine = connect(&data_dir, &github, &script).await;

    chat::send(&engine, "owner", REPOSITORY, 12, "Hold")
        .await
        .unwrap();

    wait_for_lead_text(
        &engine,
        "error: hold_event works only in a turn for an event.",
    )
    .await;
}
