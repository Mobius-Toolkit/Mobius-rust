use std::time::Duration;

use mobius_domain::{AgentNode, Harness, Live, TranscriptLine, agent_rows, shown_agents};
use mobius_engine::{Engine, activity, agents, chat, github, transcript, workstreams};
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::{install_fake_agent, install_fake_harness, start, wait_for};
use tempfile::TempDir;

const REPOSITORY: &str = "owner/shop";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
const OPTIONS: &str = r#"
[options]
model = ["sonnet", "opus"]
thought_level = ["low", "high"]
mode = ["default", "bypassPermissions"]
"#;

async fn connect(data_dir: &TempDir, github: &FakeGitHub, prompts: &str) -> Engine {
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    install_fake_agent(
        data_dir.path(),
        FAKE_AGENT,
        &format!("{OPTIONS}\n{prompts}"),
    );
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || (!workstreams::list(&engine).await.unwrap().is_empty()).then_some(())).await;
    engine
}

async fn tree(engine: &Engine) -> Vec<AgentNode> {
    agents::tree(engine, REPOSITORY, 12).await.unwrap()
}

async fn ended_node(engine: &Engine) -> AgentNode {
    wait_for(async || {
        tree(engine)
            .await
            .into_iter()
            .next()
            .filter(|node| node.session.ended_at.is_some())
    })
    .await
}

async fn lines(engine: &Engine) -> Vec<TranscriptLine> {
    chat::send(engine, "owner", REPOSITORY, 12, "Read the work")
        .await
        .unwrap();
    let node = ended_node(engine).await;
    let lines = transcript::lines(engine, node.session.id).await.unwrap();
    let rows = engine
        .store
        .transcript()
        .list(node.session.id)
        .await
        .unwrap();
    assert_eq!(
        lines
            .iter()
            .map(|line| (line.id, line.time, line.raw.clone()))
            .collect::<Vec<_>>(),
        rows.into_iter()
            .map(|row| (row.id, row.time, row.json))
            .collect::<Vec<_>>()
    );
    lines
}

#[tokio::test]
async fn a_chat_session_is_a_lead_node_that_is_live_until_it_ends() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, "[[prompts]]\nhang = true\n").await;

    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the loyalty API")
        .await
        .unwrap();

    let node = wait_for(async || tree(&engine).await.into_iter().next()).await;
    assert_eq!(node.role, "Lead");
    assert_eq!(node.title, "chat session");
    assert_eq!(node.session.harness, Harness::ClaudeCode);
    assert_eq!(node.session.model, "opus");
    assert_eq!(node.session.ended_at, None);
    // A stop before the Lead slot ends the chat with `stopped`. The first prompt comes after the slot.
    wait_for(async || {
        engine
            .store
            .transcript()
            .list(node.session.id)
            .await
            .unwrap()
            .iter()
            .any(|row| row.kind == "prompt")
            .then_some(())
    })
    .await;

    chat::stop(&engine, "owner", REPOSITORY, 12).unwrap();

    let node = ended_node(&engine).await;
    assert_eq!(node.session.end_reason.as_deref(), Some("idle"));
    assert_eq!(tree(&engine).await.len(), 1);
}

#[tokio::test]
async fn the_live_feed_gives_the_node_at_the_start_and_at_the_end() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github, "[[prompts]]\nreply = [\"Hello\"]\n").await;
    let mut feed = activity::feed(&engine, None).await.unwrap();

    chat::send(&engine, "owner", REPOSITORY, 12, "Plan the loyalty API")
        .await
        .unwrap();

    let ended = ended_node(&engine).await;
    let mut nodes = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while nodes
            .last()
            .is_none_or(|node: &AgentNode| node.session.ended_at.is_none())
        {
            if let Live::Agent(node) = feed.next().await.unwrap() {
                nodes.push(node);
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(nodes[0].session.id, ended.session.id);
    assert_eq!(nodes[0].session.ended_at, None);
    assert_eq!(nodes.last().unwrap(), &ended);
}

#[tokio::test]
async fn the_transcript_folds_the_first_prompt_and_keeps_the_others_open() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let prompts = "[[prompts]]\nreply = [\"Hello\"]\n";
    let engine = connect(&data_dir, &github, prompts).await;

    let lines = lines(&engine).await;

    let prompts: Vec<&TranscriptLine> = lines.iter().filter(|line| line.kind == "prompt").collect();
    assert_eq!(
        prompts[0].text,
        "You are the Lead of one Workstream. The Owner talks to you in this chat. Mobius also sends you events in this session. A message of the Owner and an event come one at a time, in the order that they occurred.…"
    );
    assert!(prompts[0].folded);
    assert!(prompts[0].body.as_ref().unwrap().ends_with("Read the work"));
    assert_eq!(
        prompts[1].text,
        "Save in the Workstream memory what the next session needs."
    );
    assert!(!prompts[1].folded);
    assert_eq!(prompts[1].body, None);
    let message = lines.iter().find(|line| line.text == "message").unwrap();
    assert_eq!(message.kind, "update");
    assert_eq!(message.body.as_deref(), Some("Hello"));
}

#[tokio::test]
async fn a_mobius_call_shows_the_mobius_name_and_the_short_result() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let prompts = "[[prompts]]\ncall = { tool = \"list_tasks\" }\n";
    let engine = connect(&data_dir, &github, prompts).await;
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);

    let lines = lines(&engine).await;

    let call = lines.iter().find(|line| line.kind == "mcp_call").unwrap();
    assert_eq!(call.text, "mobius · list_tasks");
    assert_eq!(call.body.as_deref(), Some("{} → #41 Add plan model: open"));
    assert!(!call.error);
}

#[tokio::test]
async fn a_validation_error_is_an_error_line() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let prompts = "[[prompts]]\ncall = { tool = \"read_issue\", arguments = { n = 0 } }\n";
    let engine = connect(&data_dir, &github, prompts).await;

    let lines = lines(&engine).await;

    let call = lines.iter().find(|line| line.kind == "mcp_call").unwrap();
    assert_eq!(call.text, "mobius · read_issue");
    assert_eq!(
        call.body.as_deref(),
        Some("{\"n\":0} → n must be 1 or more.")
    );
    assert!(call.error);
}

#[tokio::test]
async fn a_mobius_tool_call_of_each_harness_shows_the_mobius_name_and_the_harness_name() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let prompts = r#"
[[prompts]]
updates = [
  '{"sessionUpdate":"tool_call","toolCallId":"toolu_1","title":"mcp__mobius__echo","kind":"other","status":"pending","rawInput":{},"content":[],"_meta":{"claudeCode":{"toolName":"mcp__mobius__echo"}}}',
  '{"sessionUpdate":"tool_call","toolCallId":"16bf","title":"mobius_echo","kind":"other","status":"pending","rawInput":{"arguments":{"text":"probe"},"text":"probe"},"content":[],"_meta":{"mcp":{"tool":"echo","server":"mobius"},"is_mcp_tool_call":true}}',
  '{"sessionUpdate":"tool_call","toolCallId":"toolu_2","title":"Calling echo from mobius","rawInput":{"text":"probe"},"_meta":{"cognition.ai/toolName":"mcp__mobius__echo","cognition.ai/eventType":"mcp_tool_call","cognition.ai/inferenceToolName":"mcp__mobius__echo"}}',
  '{"sessionUpdate":"tool_call_update","toolCallId":"toolu_2","status":"completed","content":[{"type":"content","content":{"type":"text","text":"echo: probe"}}],"_meta":{"cognition.ai/inferenceToolName":"mcp__mobius__echo"}}',
]
"#;
    let engine = connect(&data_dir, &github, prompts).await;

    let lines = lines(&engine).await;

    let calls: Vec<(&str, Option<&str>, Option<&str>)> = lines
        .iter()
        .filter(|line| line.text.starts_with("mobius · "))
        .map(|line| {
            (
                line.text.as_str(),
                line.harness_tool_name.as_deref(),
                line.body.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        calls,
        [
            ("mobius · echo", Some("mcp__mobius__echo"), None),
            ("mobius · echo", Some("mobius_echo"), None),
            ("mobius · echo", Some("mcp__mobius__echo"), None),
            (
                "mobius · echo",
                Some("mcp__mobius__echo"),
                Some("echo: probe")
            ),
        ]
    );
}

#[tokio::test]
async fn another_tool_call_shows_its_title_and_a_short_output() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let prompts = r#"
[[prompts]]
updates = [
  '{"sessionUpdate":"tool_call","toolCallId":"toolu_3","title":"Read src/main.rs","kind":"read","status":"pending"}',
  '{"sessionUpdate":"tool_call_update","toolCallId":"toolu_3","status":"completed","content":[{"type":"content","content":{"type":"text","text":"one\ntwo\nthree\nfour"}}]}',
]
"#;
    let engine = connect(&data_dir, &github, prompts).await;

    let lines = lines(&engine).await;

    let calls: Vec<(&str, Option<&str>, Option<&str>)> = lines
        .iter()
        .filter(|line| line.text.starts_with("tool call"))
        .map(|line| {
            (
                line.text.as_str(),
                line.harness_tool_name.as_deref(),
                line.body.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        calls,
        [
            ("tool call · Read src/main.rs", None, None),
            ("tool call update", None, Some("one\ntwo\nthree…")),
        ]
    );
}

#[tokio::test]
async fn the_tree_shows_each_agent_below_the_agent_that_started_it() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);
    github.set_check(REPOSITORY, "grep -q cents plan.txt");
    let lead = r#"
[[prompts]]
when = "dispatch of #41"
call = { tool = "start_implementer", arguments = { n = 41, instructions = "Store plans in cents." } }

[[prompts]]
when = "You are the Reviewer"
shell = "true"
"#;
    let implementer = r#"
[options]
model = ["swe-1.5"]
thought_level = ["high"]

[[prompts]]
shell = "echo cents > plan.txt && git add plan.txt && git commit -q -m 'Add plan model'"
"#;
    install_fake_harness(
        data_dir.path(),
        FAKE_AGENT,
        "claude-agent-acp",
        &format!("{OPTIONS}\n{lead}"),
    );
    install_fake_harness(data_dir.path(), FAKE_AGENT, "devin", implementer);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || (!workstreams::list(&engine).await.unwrap().is_empty()).then_some(())).await;

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    let rows = wait_for(async || {
        let rows = agent_rows(tree(&engine).await);
        rows.iter()
            .any(|(_, node)| node.session.role == "reviewer")
            .then_some(rows)
    })
    .await;
    let reviewer = rows
        .iter()
        .position(|(_, node)| node.session.role == "reviewer")
        .unwrap();
    let (depth, node) = &rows[reviewer];
    assert_eq!(*depth, 2);
    let (depth, parent) = &rows[reviewer - 1];
    assert_eq!(*depth, 1);
    assert_eq!(parent.session.role, "implementer");
    assert_eq!(node.session.parent, Some(parent.session.id));
    let (depth, lead) = &rows[reviewer - 2];
    assert_eq!(*depth, 0);
    assert_eq!(lead.session.role, "lead_chat");
    assert_eq!(parent.session.parent, Some(lead.session.id));
    assert_eq!(lead.session.parent, None);
}

#[tokio::test]
async fn the_list_hides_a_stopped_agent_unless_an_agent_below_it_is_active_or_the_toggle_is_on() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);
    github.set_check(REPOSITORY, "grep -q cents plan.txt");
    let lead = r#"
[[prompts]]
when = "dispatch of #41"
call = { tool = "start_implementer", arguments = { n = 41, instructions = "Store plans in cents." } }

[[prompts]]
when = "You are the Reviewer"
hang = true
"#;
    let implementer = r#"
[options]
model = ["swe-1.5"]
thought_level = ["high"]

[[prompts]]
shell = "echo cents > plan.txt && git add plan.txt && git commit -q -m 'Add plan model'"
"#;
    install_fake_harness(
        data_dir.path(),
        FAKE_AGENT,
        "claude-agent-acp",
        &format!("{OPTIONS}\n{lead}"),
    );
    install_fake_harness(data_dir.path(), FAKE_AGENT, "devin", implementer);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || (!workstreams::list(&engine).await.unwrap().is_empty()).then_some(())).await;

    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");

    let reviewer = wait_for(async || {
        tree(&engine)
            .await
            .into_iter()
            .find(|node| node.session.role == "reviewer")
    })
    .await;
    let implementer = wait_for(async || {
        tree(&engine)
            .await
            .into_iter()
            .find(|node| node.session.id == reviewer.session.parent.unwrap())
            .filter(|node| node.session.ended_at.is_some())
    })
    .await;
    let shown = async |show_stopped: bool| -> Vec<(usize, i64, bool)> {
        agent_rows(shown_agents(tree(&engine).await, show_stopped))
            .into_iter()
            .map(|(depth, node)| (depth, node.session.id, node.session.ended_at.is_some()))
            .collect()
    };
    let lead = implementer.session.parent.unwrap();
    let (lead, implementer, reviewer) = (lead, implementer.session.id, reviewer.session.id);

    wait_for(async || {
        let ended = tree(&engine)
            .await
            .iter()
            .any(|node| node.session.id == lead && node.session.ended_at.is_some());
        ended.then_some(())
    })
    .await;
    let three_levels = [
        (0, lead, true),
        (1, implementer, true),
        (2, reviewer, false),
    ];
    assert_eq!(shown(false).await, three_levels);
    assert_eq!(shown(true).await, three_levels);

    engine.store.sessions().end(reviewer, "done").await.unwrap();

    assert_eq!(shown(false).await, []);
    assert_eq!(
        shown(true).await,
        [(0, lead, true), (1, implementer, true), (2, reviewer, true)]
    );
}
