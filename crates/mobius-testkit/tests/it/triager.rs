use mobius_domain::{Author, Live, Session, TranscriptRow};
use mobius_engine::{Engine, activity, chat, github, workstreams};
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::{install_fake_harness, start, wait_for};
use serde_json::Value;
use tempfile::TempDir;

const REPOSITORY: &str = "owner/shop";
const APP: &str = "mobius-test[bot]";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
const CLAUDE: &str = r##"
[options]
model = ["sonnet", "opus"]
thought_level = ["medium", "high"]
mode = ["default", "bypassPermissions"]

[[prompts]]
when = "# Issue\n\n#50 "
shell = "pwd"
call = { tool = "move_issue", arguments = { n = 50, workstream = 12 } }

[[prompts]]
when = "# Issue\n\n#51 "
hang = true

[[prompts]]
when = "# Issue\n\n#52 "
reply = ["Create the Workstream \"Loyalty points\" for this issue."]

[[prompts]]
when = "# Owner message\n\nStart a Workstream for loyalty points."
reply = ["Title: Loyalty points\n\nBrief: Give points for each order."]

[[prompts]]
when = "Yes, create it."
call = { tool = "create_workstream", arguments = { title = "Loyalty points", brief = "Give points for each order." } }
"##;

async fn connect(data_dir: &TempDir, github: &FakeGitHub) -> Engine {
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.set_body(REPOSITORY, 12, "Ship loyalty plans to all shops.");
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", CLAUDE);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || (!workstreams::list(&engine).await.unwrap().is_empty()).then_some(())).await;
    engine
}

async fn triagers(engine: &Engine, organization: &str, repository: &str) -> Vec<Session> {
    engine
        .store
        .sessions()
        .list(organization, repository, 0)
        .await
        .unwrap()
        .into_iter()
        .filter(|session| session.role == "triager")
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

#[tokio::test]
async fn an_issue_with_no_workstream_goes_to_the_triager_that_moves_it() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    github.add_issue(REPOSITORY, 50, "Add loyalty points");
    github.set_body(REPOSITORY, 50, "Points for each order.");

    github.add_label(REPOSITORY, 50, "mobius:ready", "owner");

    let task = wait_for(async || engine.store.tasks().live(REPOSITORY, 50).await.unwrap()).await;
    assert_eq!(task.workstream, 12);
    assert_eq!(github.sub_issue_numbers(REPOSITORY, 12), [50]);
    // Mobius adds the feed row after the task.
    let dispatched = wait_for(async || {
        engine
            .store
            .events()
            .latest(100)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.text == "Dispatched \"Add loyalty points\"")
    })
    .await;
    assert_eq!(dispatched.actor, "owner");
    let session = wait_for(async || {
        triagers(&engine, "owner", REPOSITORY)
            .await
            .into_iter()
            .find(|session| session.ended_at.is_some())
    })
    .await;
    assert_eq!(session.end_reason.as_deref(), Some("done"));
    let prompts = texts(&engine, session.id, "prompt").await;
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    let parts = [
        "You are the Triager",
        "# Open Workstreams\n\n#12 Integrate loyalty plans (owner/shop)\n\nShip loyalty plans to all shops.\n",
        "# Issue\n\n#50 Add loyalty points\n\nPoints for each order.",
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
    let reply = texts(&engine, session.id, "update").await.concat();
    assert!(
        reply.contains(&format!("/scratch/{}\nexit 0", session.id)),
        "{reply}"
    );
    wait_for(async || {
        (!data_dir
            .path()
            .join(format!("scratch/{}", session.id))
            .exists())
        .then_some(())
    })
    .await;
    assert!(
        !github
            .labels(REPOSITORY, 50)
            .contains(&"mobius:no-workstream".to_string())
    );
}

#[tokio::test]
async fn a_removal_of_the_label_stops_the_triager_and_a_proposal_goes_to_the_issue() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    github.add_issue(REPOSITORY, 51, "Rename plans");
    github.add_issue(REPOSITORY, 52, "Add loyalty points");
    github.add_label(REPOSITORY, 51, "mobius:ready", "owner");
    github.add_label(REPOSITORY, 52, "mobius:ready", "owner");
    wait_for(async || (triagers(&engine, "owner", REPOSITORY).await.len() == 2).then_some(()))
        .await;
    assert_eq!(
        github.labels(REPOSITORY, 51),
        ["mobius:no-workstream".to_string()]
    );

    github.remove_label(REPOSITORY, 51, "mobius:no-workstream", "owner");

    let ended = wait_for(async || {
        let sessions = triagers(&engine, "owner", REPOSITORY).await;
        sessions
            .iter()
            .all(|session| session.ended_at.is_some())
            .then_some(sessions)
    })
    .await;
    let mut reasons: Vec<String> = ended
        .into_iter()
        .filter_map(|session| session.end_reason)
        .collect();
    reasons.sort();
    assert_eq!(reasons, ["done", "stopped"]);
    wait_for(async || {
        github
            .comments(REPOSITORY, 52)
            .contains(&(
                APP.to_string(),
                "Create the Workstream \"Loyalty points\" for this issue.".to_string(),
            ))
            .then_some(())
    })
    .await;
    assert!(github.comments(REPOSITORY, 51).is_empty());
}

#[tokio::test]
async fn the_triager_chat_creates_a_workstream_after_the_approval() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    let mut feed = activity::feed(&engine, None).await.unwrap();

    chat::send(
        &engine,
        "owner",
        "",
        0,
        "Start a Workstream for loyalty points.",
    )
    .await
    .unwrap();
    wait_for(async || {
        chat::view(&engine, "owner", "", 0)
            .await
            .unwrap()
            .messages
            .into_iter()
            .find(|message| message.author == Author::Triager)
    })
    .await;
    chat::send(&engine, "owner", "", 0, "Yes, create it.")
        .await
        .unwrap();

    let created = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(Live::WorkstreamCreated { repository, number }) = feed.next().await {
                return (repository, number);
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(created, (REPOSITORY.to_string(), 13));
    assert_eq!(
        github.issue(REPOSITORY, 13),
        (
            "Loyalty points".to_string(),
            "Give points for each order.".to_string()
        )
    );
    assert_eq!(
        github.labels(REPOSITORY, 13),
        ["mobius:workstream".to_string()]
    );
    let prompts = wait_for(async || {
        let mut prompts = Vec::new();
        for session in triagers(&engine, "owner", "").await {
            prompts.extend(texts(&engine, session.id, "prompt").await);
        }
        (prompts.len() == 2).then_some(prompts)
    })
    .await;
    for part in [
        "You are the Triager",
        "# Open Workstreams\n\n#12 Integrate loyalty plans (owner/shop)\n\nShip loyalty plans to all shops.\n",
        "# Owner message\n\nStart a Workstream for loyalty points.",
    ] {
        assert!(prompts[0].contains(part), "{part:?} in {}", prompts[0]);
    }
    assert!(prompts[1].contains("Yes, create it."));
}

#[tokio::test]
async fn the_triager_chat_refuses_an_unknown_organization() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;

    let error = chat::send(&engine, "", "", 0, "Start a Workstream for loyalty points.")
        .await
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "Mobius has no repository in the organization \"\"."
    );
    assert!(
        chat::view(&engine, "", "", 0)
            .await
            .unwrap()
            .messages
            .is_empty()
    );
    assert!(triagers(&engine, "", "").await.is_empty());
}

#[tokio::test]
async fn a_new_triager_chat_session_gets_the_chat_history() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    chat::send(
        &engine,
        "owner",
        "",
        0,
        "Start a Workstream for loyalty points.",
    )
    .await
    .unwrap();
    wait_for(async || {
        triagers(&engine, "owner", "")
            .await
            .pop()
            .filter(|session| session.end_reason.as_deref() == Some("idle"))
    })
    .await;

    chat::send(&engine, "owner", "", 0, "Yes, create it.")
        .await
        .unwrap();

    let prompt = wait_for(async || {
        let session = triagers(&engine, "owner", "").await.into_iter().nth(1)?;
        texts(&engine, session.id, "prompt")
            .await
            .into_iter()
            .next()
    })
    .await;
    let history = &prompt[prompt.find("# Chat history\n\n").unwrap()..];
    assert!(
        history.contains("):\nStart a Workstream for loyalty points.\n\n"),
        "{history}"
    );
    assert!(
        history.contains("):\nTitle: Loyalty points\n\nBrief: Give points for each order.\n\n"),
        "{history}"
    );
    assert!(
        history.ends_with("# Owner message\n\nYes, create it."),
        "{history}"
    );
}
