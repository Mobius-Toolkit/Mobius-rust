use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Cursor;
use std::path::Path;

use mobius_engine::{config, init};
use mobius_store::Store;
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::install_fake_harness;
use tempfile::TempDir;
use tokio::net::TcpListener;

const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
const CLAUDE: &str = r#"
[options]
model = ["sonnet", "opus"]
thought_level = ["low", "medium", "high"]
mode = ["default", "bypassPermissions"]
"#;
const ANTIGRAVITY: &str = r#"
login_required = true

[options]
model = ["gemini-3-pro"]
mode = ["default", "yolo"]
"#;
const DEVIN: &str = r#"
[options]
model = ["swe-1.5"]
thought_level = ["high"]
"#;
// Lead, Triager, Implementer, Researcher, a refused Reviewer, the Reviewer, and the Judge: each has a Harness, a model, and an effort.
const ANSWERS: &str = "short
short
correct horse
correct horse
owner teammate

2
3
1

2
2


1
1
1
2


1
2
3
1
1
1
";

fn harness_path(data_dir: &Path) -> OsString {
    install_fake_harness(data_dir, FAKE_AGENT, "claude-agent-acp", CLAUDE);
    install_fake_harness(data_dir, FAKE_AGENT, "agy_acp_server", ANTIGRAVITY);
    install_fake_harness(data_dir, FAKE_AGENT, "devin", DEVIN);
    let mut dirs = vec![data_dir.join("harnesses")];
    dirs.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    env::join_paths(dirs).unwrap()
}

#[tokio::test]
async fn init_writes_a_config_file_that_the_server_starts_with() {
    let data_dir = TempDir::new().unwrap();
    let path = harness_path(data_dir.path());
    let config_path = data_dir.path().join("mobius/config.toml");
    let mut output = Vec::new();

    init::run(&mut Cursor::new(ANSWERS), &mut output, &config_path, &path)
        .await
        .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(
        output.contains("The password has less than 8 characters."),
        "{output}"
    );
    assert!(
        output.contains("Antigravity: Internal error \"Onboarding failed: Timed out waiting for the authentication flow to complete.\"\n"),
        "{output}"
    );
    assert!(
        output.contains("  1) claude-code\n  2) devin\nHarness [1]: "),
        "{output}"
    );
    assert!(
        output
            .contains("The Reviewer needs another Harness or another model than the Implementer."),
        "{output}"
    );
    assert!(
        output.ends_with(&format!(
            "Mobius wrote {}.\nStart Mobius with `mobius`.\n",
            config_path.display()
        )),
        "{output}"
    );
    assert_eq!(
        fs::read_to_string(&config_path).unwrap(),
        r#"access_password = "correct horse"
trusted_users = ["owner", "teammate"]

[roles]
lead = { harness = "claude-code", model = "opus", effort = "high" }
triager = { harness = "claude-code", model = "sonnet", effort = "medium" }
implementer = { harness = "devin", model = "swe-1.5", effort = "high" }
researcher = { harness = "claude-code", model = "sonnet", effort = "low" }
reviewer = { harness = "claude-code", model = "opus", effort = "high" }
judge = { harness = "claude-code", model = "sonnet", effort = "low" }
"#
    );
    let mut config = config::load(&config_path).unwrap();
    config.data_dir = data_dir.path().join("data");
    let github = FakeGitHub::start().await;
    let store = Store::open(&config.data_dir).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    mobius_engine::start(
        config,
        store,
        &github.url,
        &github.url,
        path,
        listener.local_addr().unwrap().port(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn init_logs_in_to_antigravity_and_offers_it() {
    let data_dir = TempDir::new().unwrap();
    let path = harness_path(data_dir.path());
    install_fake_harness(
        data_dir.path(),
        FAKE_AGENT,
        "agy_acp_server",
        &format!("login_works = true\n{ANTIGRAVITY}"),
    );
    let config_path = data_dir.path().join("config.toml");
    let mut output = Vec::new();
    // The Researcher uses Antigravity, and the Implementer uses Devin. The other Roles take the defaults.
    let answers = "correct horse\ncorrect horse\nowner\n\n\n\n\n\n\n3\n\n\n2\n\n\n\n\n\n\n\n";

    init::run(&mut Cursor::new(answers), &mut output, &config_path, &path)
        .await
        .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Antigravity: logged in\n"), "{output}");
    assert!(
        output.contains("  1) claude-code\n  2) antigravity\n  3) devin\nHarness [1]: "),
        "{output}"
    );
    assert!(
        fs::read_to_string(&config_path)
            .unwrap()
            .contains("researcher = { harness = \"antigravity\", model = \"gemini-3-pro\" }\n")
    );
}

#[tokio::test]
async fn init_refuses_an_existing_config_file() {
    let data_dir = TempDir::new().unwrap();
    let config_path = data_dir.path().join("config.toml");
    fs::write(&config_path, "").unwrap();

    let error = init::run(
        &mut Cursor::new(ANSWERS),
        &mut Vec::new(),
        &config_path,
        &OsString::new(),
    )
    .await
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        format!(
            "{} exists. `mobius init` writes only a new file.",
            config_path.display()
        )
    );
}
