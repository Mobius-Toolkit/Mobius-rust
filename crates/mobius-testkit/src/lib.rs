pub mod fake_github;

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use mobius_engine::Engine;
use mobius_engine::config::Config;
use mobius_store::Store;
use tokio::net::TcpListener;

pub async fn start(data_dir: &Path, access_password: &str, github_url: &str) -> Engine {
    start_with_config(data_dir, access_password, github_url, "").await
}

pub async fn start_with_config(
    data_dir: &Path,
    access_password: &str,
    github_url: &str,
    extra_config: &str,
) -> Engine {
    start_with(data_dir, access_password, github_url, extra_config, |_| {}).await
}

// `adjust` changes the parsed Config, for example to set `config.roles.lead.max`.
pub async fn start_with(
    data_dir: &Path,
    access_password: &str,
    github_url: &str,
    extra_config: &str,
    adjust: impl FnOnce(&mut Config),
) -> Engine {
    let mut config = config(data_dir, access_password, extra_config);
    adjust(&mut config);
    start_engine(config, github_url).await
}

pub fn config(data_dir: &Path, access_password: &str, extra_config: &str) -> Config {
    let mut config = mobius_engine::config::parse(&format!(
        r#"
access_password = "{access_password}"
trusted_users = ["owner"]
data_dir = "{}"
poll_interval = "50ms"
lead_idle_timeout = "300ms"
{extra_config}

[roles]
lead        = {{ harness = "claude-code", model = "opus",    effort = "high" }}
triager     = {{ harness = "claude-code", model = "sonnet",  effort = "medium" }}
implementer = {{ harness = "devin",       model = "swe-1.5", effort = "high" }}
researcher  = {{ harness = "antigravity", model = "gemini-3-pro" }}
reviewer    = {{ harness = "claude-code", model = "opus",    effort = "high" }}
judge       = {{ harness = "claude-code", model = "haiku",   effort = "low" }}
"#,
        data_dir.display()
    ))
    .unwrap();
    config.restart_waits = vec![Duration::from_millis(10)];
    config
}

// The Harness `PATH` is `harnesses` and then the `PATH` of the test. The `gh` in `harnesses` prints its arguments and `GH_TOKEN`.
pub async fn start_engine(config: Config, github_url: &str) -> Engine {
    let harnesses = config.data_dir.join("harnesses");
    fs::create_dir_all(&harnesses).unwrap();
    let gh = harnesses.join("gh");
    fs::write(&gh, "#!/bin/sh\necho \"gh $* with GH_TOKEN=$GH_TOKEN\"\n").unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    let mut path = vec![harnesses];
    path.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    let store = Store::open(&config.data_dir).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let engine = mobius_engine::start(
        config,
        store,
        github_url,
        github_url,
        env::join_paths(path).unwrap(),
        listener.local_addr().unwrap().port(),
    )
    .await
    .unwrap();
    let router =
        mobius_engine::mcp::router(engine.clone()).merge(mobius_engine::gh::router(engine.clone()));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    engine
}

// Each Harness command runs `fake_agent` with `script`.
pub fn install_fake_agent(data_dir: &Path, fake_agent: &str, script: &str) {
    for program in ["claude-agent-acp", "agy_acp_server", "devin"] {
        install_fake_harness(data_dir, fake_agent, program, script);
    }
}

// The Harness command `program` runs `fake_agent` with `script`, and writes its environment to `harnesses/env` and its working directory to `harnesses/pwd`.
pub fn install_fake_harness(data_dir: &Path, fake_agent: &str, program: &str, script: &str) {
    let harnesses = data_dir.join("harnesses");
    fs::create_dir_all(&harnesses).unwrap();
    let script_path = harnesses.join(format!("{program}.toml"));
    fs::write(&script_path, script).unwrap();
    let wrapper = harnesses.join(program);
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n/usr/bin/env > '{0}/env'\npwd > '{0}/pwd'\nexec '{fake_agent}' '{1}'\n",
            harnesses.display(),
            script_path.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
}

// Gives the trimmed stdout. The system and user git config do not apply.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=owner",
            "-c",
            "user.email=owner@example.com",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

pub async fn wait_for<T>(mut check: impl AsyncFnMut() -> Option<T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Some(value) = check().await {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the condition is not true after 60 seconds")
}

// The first poll of a repository runs the lost-task recovery and treats each earlier issue event as old. It stores the `since` cursor at its end.
pub async fn wait_for_first_poll(engine: &Engine, repository: &str) {
    wait_for(async || {
        engine
            .store
            .sync_cursors()
            .get(repository, "issues")
            .await
            .unwrap()
            .since
            .map(|_| ())
    })
    .await;
}
