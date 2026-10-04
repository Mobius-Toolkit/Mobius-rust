use std::fs;

use mobius_domain::{Author, Session, TranscriptRow};
use mobius_engine::config::Config;
use mobius_engine::{Engine, chat, github, workstreams};
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::{install_fake_harness, start_with, wait_for};
use serde_json::Value;
use tempfile::TempDir;

const REPOSITORY: &str = "owner/shop";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
const LEAD_OPTIONS: &str = r#"
[options]
model = ["sonnet", "opus", "haiku"]
thought_level = ["low", "medium", "high"]
mode = ["default", "bypassPermissions"]
"#;
const IMPLEMENTER_OPTIONS: &str = r#"
[options]
model = ["swe-1.5"]
thought_level = ["high"]
"#;
const RESEARCHER: &str = r#"
[options]
model = ["gemini-3-pro"]
mode = ["default", "yolo"]

[[prompts]]
when = "You are a Researcher"
reply = ["Plans store the price in cents."]
"#;
const COMMIT: &str =
    "shell = \"echo cents > plan.txt && git add plan.txt && git commit -q -m 'Add plan model'\"\n";
const START_41: &str = "[[prompts]]\nwhen = \"dispatch of #41\"\ncall = { tool = \"start_implementer\", arguments = { n = 41, instructions = \"Store plans in cents.\" } }\n";
const START_43: &str = "[[prompts]]\nwhen = \"dispatch of #43\"\ncall = { tool = \"start_implementer\", arguments = { n = 43, instructions = \"Round prices down.\" } }\n";

async fn connect(
    data_dir: &TempDir,
    github: &FakeGitHub,
    extra_config: &str,
    adjust: impl FnOnce(&mut Config),
    lead: &str,
    implementer: &str,
) -> Engine {
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.set_body(REPOSITORY, 12, "Ship loyalty plans to all shops.");
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
    install_fake_harness(data_dir.path(), FAKE_AGENT, "agy_acp_server", RESEARCHER);
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

async fn sessions(engine: &Engine, repository: &str, workstream: i64, role: &str) -> Vec<Session> {
    engine
        .store
        .sessions()
        .list("owner", repository, workstream)
        .await
        .unwrap()
        .into_iter()
        .filter(|session| session.role == role)
        .collect()
}

async fn prompts_of(engine: &Engine, session: i64) -> Vec<String> {
    let rows: Vec<TranscriptRow> = engine.store.transcript().list(session).await.unwrap();
    rows.iter()
        .filter(|row| row.kind == "prompt")
        .filter_map(|row| {
            let json: Value = serde_json::from_str(&row.json).unwrap();
            json["text"].as_str().map(str::to_string)
        })
        .collect()
}

// A session writes a prompt only after it takes a slot and starts its first turn.
async fn prompted(engine: &Engine, repository: &str, workstream: i64, role: &str) -> Session {
    wait_for(async || {
        for session in sessions(engine, repository, workstream, role).await {
            if !prompts_of(engine, session.id).await.is_empty() {
                return Some(session);
            }
        }
        None
    })
    .await
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

#[tokio::test]
async fn a_second_implementer_waits_for_the_implementer_limit_while_a_researcher_starts() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    // The first prompt of a Lead session has the earlier events in its history, so the entry for the comment comes first.
    let lead = format!(
        "[[prompts]]\nwhen = \"comment on #41\"\ncall = {{ tool = \"start_researcher\", arguments = {{ question = \"Where is the price?\" }} }}\n{START_41}{START_43}"
    );
    let engine = connect(
        &data_dir,
        &github,
        "",
        |config| config.roles.implementer.max = 1,
        &lead,
        &format!("[[prompts]]\n{COMMIT}"),
    )
    .await;
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);
    github.add_issue(REPOSITORY, 43, "Add plan price");
    github.add_sub_issue(REPOSITORY, 12, 43);
    let go = data_dir.path().join("go");
    github.set_check(
        REPOSITORY,
        &format!("while [ ! -e '{}' ]; do sleep 0.05; done", go.display()),
    );

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    github.add_label(REPOSITORY, 43, "mobius:ready", "owner");

    let queued = wait_for(async || {
        sessions(&engine, REPOSITORY, 12, "implementer")
            .await
            .into_iter()
            .find(|session| session.queue_reason.is_some())
    })
    .await;
    assert_eq!(
        queued.queue_reason.as_deref(),
        Some("no free implementer slot (1/1)")
    );

    github.add_comment(REPOSITORY, 41, "owner", "Where is the price?");

    // The full Implementer limit does not stop a Researcher.
    let researcher = wait_for(async || {
        sessions(&engine, REPOSITORY, 12, "researcher")
            .await
            .into_iter()
            .find(|session| session.ended_at.is_some())
    })
    .await;
    assert_eq!(researcher.end_reason.as_deref(), Some("done"));

    fs::write(&go, "").unwrap();

    let ended = wait_for(async || {
        let ended: Vec<Session> = sessions(&engine, REPOSITORY, 12, "implementer")
            .await
            .into_iter()
            .filter(|session| session.ended_at.is_some())
            .collect();
        (ended.len() == 2).then_some(ended)
    })
    .await;
    let (second, first): (Vec<Session>, Vec<Session>) = ended
        .into_iter()
        .partition(|session| session.id == queued.id);
    assert_eq!(first[0].end_reason.as_deref(), Some("done"));
    assert_eq!(second[0].end_reason.as_deref(), Some("done"));
    assert_eq!(second[0].queue_reason, None);
    assert!(second[0].started_at >= first[0].ended_at.unwrap());
}

#[tokio::test]
async fn a_judge_waits_for_the_global_limit_behind_a_running_implementer() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let lead = format!(
        "{START_41}{START_43}[[prompts]]\nwhen = \"You are the Judge\"\ncall = {{ tool = \"submit_verdicts\", arguments = {{ items = [{{ item = 1, actions = [{{ verdict = \"question\", text = \"Why cents?\" }}] }}] }} }}\n[[prompts]]\nwhen = \"You are the Reviewer\"\nshell = \"true\"\n"
    );
    let implementer = format!(
        "[[prompts]]\nwhen = \"#41 Add plan model\"\n{COMMIT}[[prompts]]\nwhen = \"#43 Add plan price\"\nhang = true\n"
    );
    let engine = connect(
        &data_dir,
        &github,
        "max_agents = 1\nreview_quiet_period = \"200ms\"",
        |_| {},
        &lead,
        &implementer,
    )
    .await;
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);
    github.add_issue(REPOSITORY, 43, "Add plan price");
    github.add_sub_issue(REPOSITORY, 12, 43);
    github.set_check(REPOSITORY, "grep -q cents plan.txt");

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || {
        (task_state(&engine, 41).await.as_deref() == Some("ready_for_review")).then_some(())
    })
    .await;

    github.add_label(REPOSITORY, 43, "mobius:ready", "owner");
    // The Implementer of #43 takes the only agent slot and waits for a stop.
    let hanging = wait_for(async || {
        let implementers = sessions(&engine, REPOSITORY, 12, "implementer").await;
        let second = implementers.into_iter().nth(1)?;
        (!prompts_of(&engine, second.id).await.is_empty()).then_some(second)
    })
    .await;
    github.add_comment(
        REPOSITORY,
        github.pull_requests(REPOSITORY)[0].number,
        "owner",
        "Why cents?",
    );

    let queued = wait_for(async || {
        sessions(&engine, REPOSITORY, 12, "judge")
            .await
            .into_iter()
            .find(|session| session.queue_reason.is_some())
    })
    .await;
    assert_eq!(
        queued.queue_reason.as_deref(),
        Some("no free agent slot (1/1)")
    );

    github.remove_label(REPOSITORY, 43, "mobius:working", "owner");

    let judge = wait_for(async || {
        sessions(&engine, REPOSITORY, 12, "judge")
            .await
            .into_iter()
            .find(|session| session.ended_at.is_some())
    })
    .await;
    assert_eq!(judge.end_reason.as_deref(), Some("done"));
    let stopped = sessions(&engine, REPOSITORY, 12, "implementer")
        .await
        .into_iter()
        .find(|session| session.id == hanging.id)
        .unwrap();
    assert_eq!(stopped.end_reason.as_deref(), Some("stopped"));
    assert!(judge.started_at >= stopped.ended_at.unwrap());
}

#[tokio::test]
async fn a_judge_with_counts_in_max_agents_false_starts_while_the_global_limit_is_full() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let lead = format!(
        "{START_41}{START_43}[[prompts]]\nwhen = \"You are the Judge\"\ncall = {{ tool = \"submit_verdicts\", arguments = {{ items = [{{ item = 1, actions = [{{ verdict = \"question\", text = \"Why cents?\" }}] }}] }} }}\n[[prompts]]\nwhen = \"You are the Reviewer\"\nshell = \"true\"\n"
    );
    let implementer = format!(
        "[[prompts]]\nwhen = \"#41 Add plan model\"\n{COMMIT}[[prompts]]\nwhen = \"#43 Add plan price\"\nhang = true\n"
    );
    let engine = connect(
        &data_dir,
        &github,
        "max_agents = 1\nreview_quiet_period = \"200ms\"",
        |config| config.roles.judge.counts_in_max_agents = false,
        &lead,
        &implementer,
    )
    .await;
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);
    github.add_issue(REPOSITORY, 43, "Add plan price");
    github.add_sub_issue(REPOSITORY, 12, 43);
    github.set_check(REPOSITORY, "grep -q cents plan.txt");

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || {
        (task_state(&engine, 41).await.as_deref() == Some("ready_for_review")).then_some(())
    })
    .await;

    github.add_label(REPOSITORY, 43, "mobius:ready", "owner");
    // The Implementer of #43 takes the only agent slot and waits for a stop.
    let hanging = wait_for(async || {
        let implementers = sessions(&engine, REPOSITORY, 12, "implementer").await;
        let second = implementers.into_iter().nth(1)?;
        (!prompts_of(&engine, second.id).await.is_empty()).then_some(second)
    })
    .await;
    github.add_comment(
        REPOSITORY,
        github.pull_requests(REPOSITORY)[0].number,
        "owner",
        "Why cents?",
    );

    // The Judge does not count toward `max_agents`, so it starts while the only slot is taken.
    let judge = wait_for(async || {
        sessions(&engine, REPOSITORY, 12, "judge")
            .await
            .into_iter()
            .find(|session| session.ended_at.is_some())
    })
    .await;
    assert_eq!(judge.end_reason.as_deref(), Some("done"));
    assert_eq!(judge.queue_reason, None);
    let still_running = sessions(&engine, REPOSITORY, 12, "implementer")
        .await
        .into_iter()
        .find(|session| session.id == hanging.id)
        .unwrap();
    assert_eq!(still_running.ended_at, None);

    github.remove_label(REPOSITORY, 43, "mobius:working", "owner");
}

#[tokio::test]
async fn a_lead_chat_and_a_triager_start_while_the_global_limit_is_full() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let lead = format!(
        "{START_41}[[prompts]]\nwhen = \"You are the Triager\"\nreply = [\"A Workstream for loyalty analytics.\"]\n[[prompts]]\nwhen = \"Check the plan\"\nreply = [\"Looks good.\"]\n"
    );
    let implementer = "[[prompts]]\nwhen = \"#41 Add plan model\"\nhang = true\n";
    let engine = connect(
        &data_dir,
        &github,
        "max_agents = 1",
        |_| {},
        &lead,
        implementer,
    )
    .await;
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    // The Implementer takes the only agent slot.
    prompted(&engine, REPOSITORY, 12, "implementer").await;

    chat::send(&engine, "owner", REPOSITORY, 12, "Check the plan")
        .await
        .unwrap();
    chat::send(
        &engine,
        "owner",
        "",
        0,
        "A Workstream for loyalty analytics.",
    )
    .await
    .unwrap();

    let lead_chat = prompted(&engine, REPOSITORY, 12, "lead_chat").await;
    assert_eq!(lead_chat.queue_reason, None);
    let triager = prompted(&engine, "", 0, "triager").await;
    assert_eq!(triager.queue_reason, None);

    github.remove_label(REPOSITORY, 41, "mobius:working", "owner");
}

#[tokio::test]
async fn a_second_lead_chat_waits_for_the_lead_limit_and_shows_the_message_of_the_owner() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_issue(REPOSITORY, 50, "Loyalty points");
    github.add_label(REPOSITORY, 50, "mobius:workstream", "owner");
    let lead = "[[prompts]]\nwhen = \"Plan the loyalty API\"\nhang = true\n[[prompts]]\nreply = [\"Done.\"]\n";
    let engine = connect(
        &data_dir,
        &github,
        "",
        |config| config.roles.lead.max = 1,
        lead,
        "",
    )
    .await;

    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the loyalty API")
        .await
        .unwrap();
    // The first Lead chat takes the only lead slot.
    prompted(&engine, REPOSITORY, 12, "lead_chat").await;

    chat::send(&engine, "owner", REPOSITORY, 50, "Plan the loyalty points")
        .await
        .unwrap();

    let queued = wait_for(async || {
        sessions(&engine, REPOSITORY, 50, "lead_chat")
            .await
            .into_iter()
            .find(|session| session.queue_reason.is_some())
    })
    .await;
    assert_eq!(
        queued.queue_reason.as_deref(),
        Some("no free lead slot (1/1)")
    );
    // The message of the Owner shows while the Lead waits.
    let view = chat::view(&engine, "owner", REPOSITORY, 50).await.unwrap();
    assert!(view.messages.iter().any(|message| {
        message.author == Author::Owner && message.text == "Plan the loyalty points"
    }));

    chat::stop(&engine, "owner", REPOSITORY, 12).unwrap();

    let first = wait_for(async || {
        sessions(&engine, REPOSITORY, 12, "lead_chat")
            .await
            .pop()
            .filter(|session| session.ended_at.is_some())
    })
    .await;
    assert_eq!(first.end_reason.as_deref(), Some("idle"));
    let second = prompted(&engine, REPOSITORY, 50, "lead_chat").await;
    assert!(second.started_at >= first.ended_at.unwrap());
}

#[tokio::test]
async fn a_stop_ends_a_waiting_lead_chat() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_issue(REPOSITORY, 50, "Loyalty points");
    github.add_label(REPOSITORY, 50, "mobius:workstream", "owner");
    let lead = "[[prompts]]\nwhen = \"Plan the loyalty API\"\nhang = true\n[[prompts]]\nreply = [\"Done.\"]\n";
    let engine = connect(
        &data_dir,
        &github,
        "",
        |config| config.roles.lead.max = 1,
        lead,
        "",
    )
    .await;

    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the loyalty API")
        .await
        .unwrap();
    // The first Lead chat takes the only lead slot.
    prompted(&engine, REPOSITORY, 12, "lead_chat").await;

    chat::send(&engine, "owner", REPOSITORY, 50, "Plan the loyalty points")
        .await
        .unwrap();
    let queued = wait_for(async || {
        sessions(&engine, REPOSITORY, 50, "lead_chat")
            .await
            .into_iter()
            .find(|session| session.queue_reason.is_some())
    })
    .await;

    chat::stop(&engine, "owner", REPOSITORY, 50).unwrap();

    let stopped = wait_for(async || {
        sessions(&engine, REPOSITORY, 50, "lead_chat")
            .await
            .into_iter()
            .find(|session| session.ended_at.is_some())
    })
    .await;
    assert_eq!(stopped.id, queued.id);
    assert_eq!(stopped.end_reason.as_deref(), Some("stopped"));
    let view = chat::view(&engine, "owner", REPOSITORY, 50).await.unwrap();
    assert!(!view.writing);

    chat::stop(&engine, "owner", REPOSITORY, 12).unwrap();
    wait_for(async || {
        sessions(&engine, REPOSITORY, 12, "lead_chat")
            .await
            .pop()
            .filter(|session| session.ended_at.is_some())
    })
    .await;
}

#[tokio::test]
async fn a_stop_keeps_the_later_message_of_a_waiting_lead_chat() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_issue(REPOSITORY, 50, "Loyalty points");
    github.add_label(REPOSITORY, 50, "mobius:workstream", "owner");
    let lead = "[[prompts]]\nwhen = \"Plan the loyalty API\"\nhang = true\n[[prompts]]\nreply = [\"Done.\"]\n";
    let engine = connect(
        &data_dir,
        &github,
        "",
        |config| config.roles.lead.max = 1,
        lead,
        "",
    )
    .await;

    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the loyalty API")
        .await
        .unwrap();
    // The first Lead chat takes the only lead slot.
    prompted(&engine, REPOSITORY, 12, "lead_chat").await;

    chat::send(&engine, "owner", REPOSITORY, 50, "Message A")
        .await
        .unwrap();
    let queued = wait_for(async || {
        sessions(&engine, REPOSITORY, 50, "lead_chat")
            .await
            .into_iter()
            .find(|session| session.queue_reason.is_some())
    })
    .await;
    chat::send(&engine, "owner", REPOSITORY, 50, "Message B")
        .await
        .unwrap();

    chat::stop(&engine, "owner", REPOSITORY, 50).unwrap();
    chat::stop(&engine, "owner", REPOSITORY, 12).unwrap();

    let session = prompted(&engine, REPOSITORY, 50, "lead_chat").await;
    assert_eq!(session.id, queued.id);
    let prompts = prompts_of(&engine, session.id).await;
    // The chat history in the prompt still shows message A, but the turn answers message B.
    assert!(prompts.iter().any(|prompt| prompt.ends_with("Message B")));
    assert!(prompts.iter().all(|prompt| !prompt.ends_with("Message A")));
}

#[tokio::test]
async fn a_stop_keeps_the_event_of_a_waiting_lead_chat() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_issue(REPOSITORY, 50, "Loyalty points");
    github.add_label(REPOSITORY, 50, "mobius:workstream", "owner");
    let lead = "[[prompts]]\nwhen = \"Plan the loyalty API\"\nhang = true\n[[prompts]]\nreply = [\"Done.\"]\n";
    let engine = connect(
        &data_dir,
        &github,
        "",
        |config| config.roles.lead.max = 1,
        lead,
        "",
    )
    .await;

    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the loyalty API")
        .await
        .unwrap();
    // The first Lead chat takes the only lead slot.
    prompted(&engine, REPOSITORY, 12, "lead_chat").await;

    github.add_issue(REPOSITORY, 51, "Add points model");
    github.add_sub_issue(REPOSITORY, 50, 51);
    github.add_label(REPOSITORY, 51, "mobius:ready", "owner");
    let queued = wait_for(async || {
        sessions(&engine, REPOSITORY, 50, "lead_chat")
            .await
            .into_iter()
            .find(|session| session.queue_reason.is_some())
    })
    .await;

    chat::stop(&engine, "owner", REPOSITORY, 50).unwrap();
    chat::stop(&engine, "owner", REPOSITORY, 12).unwrap();

    let session = prompted(&engine, REPOSITORY, 50, "lead_chat").await;
    assert_eq!(session.id, queued.id);
    let prompts = prompts_of(&engine, session.id).await;
    assert!(prompts[0].contains("# Event\n\n"), "{}", prompts[0]);
    assert!(prompts[0].contains(" dispatch of #51 "), "{}", prompts[0]);
}
