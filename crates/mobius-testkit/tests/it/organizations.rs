use mobius_domain::{Author, Live, TranscriptRow};
use mobius_engine::{Engine, activity, chat, github, workstreams};
use mobius_testkit::fake_github::{self, FakeGitHub};
use mobius_testkit::{install_fake_agent, start, wait_for};
use serde_json::Value;
use tempfile::TempDir;

const SHOP: &str = "owner/shop";
const GARDEN: &str = "other/garden";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
const SCRIPT: &str = r##"
[options]
model = ["sonnet", "opus"]
thought_level = ["medium", "high"]
mode = ["default", "bypassPermissions"]

[[prompts]]
when = "# Owner message\n\nStart a Workstream for seeds."
reply = ["Title: Seeds\n\nBrief: Sell seeds."]

[[prompts]]
when = "Yes, create it."
call = { tool = "create_workstream", arguments = { title = "Seeds", brief = "Sell seeds." } }

[[prompts]]
when = "List the issues"
shell = "gh issue list"
"##;

// The first App has the organization `owner`, and the second App has the organization `other`.
async fn connect(data_dir: &TempDir, github: &FakeGitHub) -> Engine {
    github.add_manifest_code("first-code");
    github.add_manifest_code("second-code");
    github.install_second_app("other");
    for (repository, title) in [(SHOP, "Integrate loyalty plans"), (GARDEN, "Plant roses")] {
        github.add_repository(repository);
        github.add_issue(repository, 12, title);
        github.add_label(repository, 12, "mobius:workstream", "owner");
    }
    install_fake_agent(data_dir.path(), FAKE_AGENT, SCRIPT);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "first-code")
        .await
        .unwrap();
    github::convert_manifest(&engine, "second-code")
        .await
        .unwrap();
    wait_for(async || (workstreams::list(&engine).await.unwrap().len() == 2).then_some(())).await;
    engine
}

async fn workstream_repositories(engine: &Engine) -> Vec<String> {
    let mut repositories: Vec<String> = workstreams::list(engine)
        .await
        .unwrap()
        .into_iter()
        .map(|workstream| workstream.repository)
        .collect();
    repositories.sort();
    repositories
}

#[tokio::test]
async fn each_app_gives_the_repositories_of_its_organization() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;

    assert_eq!(workstreams::organizations(&engine), ["other", "owner"]);
    assert_eq!(workstream_repositories(&engine).await, [GARDEN, SHOP]);
}

#[tokio::test]
async fn a_failed_app_keeps_its_repositories_and_the_other_app_continues() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;

    github.fail_installations(fake_github::SECOND_APP_ID);
    github.add_repository("owner/cafe");
    github.add_issue("owner/cafe", 12, "Serve coffee");
    github.add_label("owner/cafe", 12, "mobius:workstream", "owner");

    wait_for(async || (workstreams::list(&engine).await.unwrap().len() == 3).then_some(())).await;
    assert_eq!(
        workstream_repositories(&engine).await,
        [GARDEN, "owner/cafe", SHOP]
    );
}

#[tokio::test]
async fn the_triager_chat_of_an_organization_creates_the_workstream_in_its_repository() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    let mut feed = activity::feed(&engine, None).await.unwrap();

    chat::send(&engine, "other", "", 0, "Start a Workstream for seeds.")
        .await
        .unwrap();
    wait_for(async || {
        chat::view(&engine, "other", "", 0)
            .await
            .unwrap()
            .messages
            .into_iter()
            .find(|message| message.author == Author::Triager)
    })
    .await;
    chat::send(&engine, "other", "", 0, "Yes, create it.")
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
    assert_eq!(created, (GARDEN.to_string(), 13));
    assert_eq!(github.labels(GARDEN, 13), ["mobius:workstream".to_string()]);
    let session = engine
        .store
        .sessions()
        .list("other", "", 0)
        .await
        .unwrap()
        .remove(0);
    let rows: Vec<TranscriptRow> = engine.store.transcript().list(session.id).await.unwrap();
    let first: Value = serde_json::from_str(&rows[0].json).unwrap();
    let prompt = first["text"].as_str().unwrap();
    assert!(
        prompt.contains("#12 Plant roses (other/garden)"),
        "{prompt}"
    );
    assert!(!prompt.contains(SHOP), "{prompt}");
}

#[tokio::test]
async fn gh_in_a_lead_chat_gets_the_user_token_of_the_app_of_the_repository() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_second_app_user_code("user-code", "owner");
    let engine = connect(&data_dir, &github).await;
    assert!(github::authorize_user(&engine, "user-code").await.unwrap());

    chat::send(&engine, "other", GARDEN, 12, "List the issues")
        .await
        .unwrap();

    let reply = wait_for(async || {
        engine
            .store
            .chat_messages()
            .list("other", GARDEN, 12)
            .await
            .unwrap()
            .into_iter()
            .find(|message| message.author == Author::Lead)
    })
    .await;
    assert_eq!(reply.text, "gh issue list with GH_TOKEN=ghu_1\nexit 0");
}
