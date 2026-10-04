use mobius_domain::{Author, Session, TranscriptRow};
use mobius_engine::{Engine, chat, github, workstreams};
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::{git, install_fake_harness, start, wait_for};
use serde_json::Value;
use tempfile::TempDir;

const REPOSITORY: &str = "owner/shop";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
const QUESTION: &str = "Where do plans store the price?";
// The first prompt of a new chat session also has the Role prompt, so the report prompt comes first in the script.
const LEAD: &str = r##"
[options]
model = ["sonnet", "opus"]
thought_level = ["low", "high"]
mode = ["default", "bypassPermissions"]

[[prompts]]
when = "# Researcher message"
reply = ["I have the report."]

[[prompts]]
when = "report of the Researcher"
reply = ["I have the report."]

[[prompts]]
when = "# Owner message"
call = { tool = "start_researcher", arguments = { question = "Where do plans store the price?" } }
"##;
const RESEARCHER: &str = r#"
[options]
model = ["gemini-3-pro"]
mode = ["default", "yolo"]

[[prompts]]
when = "You are a Researcher"
reply = ["Plans store the price in cents.\n"]
shell = "pwd && git rev-parse HEAD && git rev-parse --abbrev-ref HEAD"
"#;

async fn connect(data_dir: &TempDir, github: &FakeGitHub) -> Engine {
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.set_body(REPOSITORY, 12, "Ship loyalty plans to all shops.");
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", LEAD);
    install_fake_harness(data_dir.path(), FAKE_AGENT, "agy_acp_server", RESEARCHER);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
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

async fn texts(engine: &Engine, session: i64, kind: &str) -> Vec<String> {
    let rows: Vec<TranscriptRow> = engine.store.transcript().list(session).await.unwrap();
    rows.iter()
        .filter(|row| row.kind == kind)
        .filter_map(|row| {
            let json: Value = serde_json::from_str(&row.json).unwrap();
            let text = match kind {
                "prompt" => &json["text"],
                _ => &json["update"]["content"]["text"],
            };
            text.as_str().map(str::to_string)
        })
        .collect()
}

async fn prompts(engine: &Engine, role: &str) -> Vec<String> {
    let mut all = Vec::new();
    for session in sessions(engine, role).await {
        all.extend(texts(engine, session.id, "prompt").await);
    }
    all
}

async fn researcher_report(data_dir: &TempDir, github: &FakeGitHub, engine: &Engine) -> String {
    let session = wait_for(async || {
        sessions(engine, "researcher")
            .await
            .into_iter()
            .find(|session| session.ended_at.is_some())
    })
    .await;
    assert_eq!(session.end_reason.as_deref(), Some("done"));
    let prompts = texts(engine, session.id, "prompt").await;
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    for part in [
        "You are a Researcher",
        "# Brief\n\nShip loyalty plans to all shops.\n",
        &format!("# Question\n\n{QUESTION}"),
    ] {
        assert!(prompts[0].contains(part), "{part:?} in {}", prompts[0]);
    }
    let main = git(&github.remote(REPOSITORY), &["rev-parse", "main"]);
    let report = texts(engine, session.id, "update").await.concat();
    assert!(
        report.contains(&format!(
            "/worktrees/owner/shop/research-{}\n{main}\nHEAD\nexit 0",
            session.id
        )),
        "{report}"
    );
    assert!(
        !data_dir
            .path()
            .join(format!("worktrees/owner/shop/research-{}", session.id))
            .exists()
    );
    report
}

#[tokio::test]
async fn a_report_for_the_chat_goes_to_a_chat_session() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;

    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the price model.")
        .await
        .unwrap();

    let report = researcher_report(&data_dir, &github, &engine).await;
    wait_for(async || {
        prompts(&engine, "lead_chat")
            .await
            .iter()
            .any(|prompt| {
                prompt.contains(&format!(
                    "# Researcher message\n\nReport of the Researcher on \"{QUESTION}\":\n\n{report}"
                ))
            })
            .then_some(())
    })
    .await;
    let messages = wait_for(async || {
        let messages = chat::view(&engine, "owner", REPOSITORY, 12)
            .await
            .unwrap()
            .messages;
        messages
            .iter()
            .any(|message| {
                message.author == Author::Lead && message.text.contains("I have the report.")
            })
            .then_some(messages)
    })
    .await;
    assert!(
        !messages
            .iter()
            .any(|message| message.author == Author::Researcher),
        "{messages:?}"
    );
    let unread = engine
        .store
        .chat_messages()
        .unread_of("owner", REPOSITORY, 12)
        .await
        .unwrap();
    assert_eq!(
        unread.count,
        messages
            .iter()
            .filter(|message| message.author != Author::Owner)
            .count() as i64
    );
}
