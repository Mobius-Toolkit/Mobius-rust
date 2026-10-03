#![cfg(feature = "server")]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

struct Setup {
    home: TempDir,
    data_dir: PathBuf,
}

impl Setup {
    fn new() -> Setup {
        let home = tempfile::tempdir().unwrap();
        let data_dir = home.path().join("data");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let store = runtime
            .block_on(mobius_store::Store::open(&data_dir))
            .unwrap();
        runtime.block_on(store.pool.close());
        fs::write(
            home.path().join("config.toml"),
            format!(
                r#"
access_password = "correct horse"
trusted_users = ["owner"]
data_dir = "{}"

[roles]
lead        = {{ harness = "claude-code", model = "opus",    effort = "high" }}
triager     = {{ harness = "claude-code", model = "sonnet",  effort = "medium" }}
implementer = {{ harness = "devin",       model = "swe-1.5", effort = "high" }}
researcher  = {{ harness = "antigravity", model = "gemini-3-pro" }}
reviewer    = {{ harness = "claude-code", model = "opus",    effort = "high" }}
judge       = {{ harness = "claude-code", model = "haiku",   effort = "low" }}
"#,
                data_dir.display()
            ),
        )
        .unwrap();
        Setup { home, data_dir }
    }

    fn run(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_mobius"))
            .args(arguments)
            .env("HOME", self.home.path())
            .env("MOBIUS_CONFIG", self.home.path().join("config.toml"))
            .env("PORT", "0")
            .output()
            .unwrap()
    }
}

fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.insert(path.clone(), fs::read(path).unwrap());
            }
        }
    }
    files
}

fn assert_help_prints_and_changes_nothing(argument: &str) {
    let setup = Setup::new();
    let before = snapshot(setup.home.path());

    let output = setup.run(&[argument]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("Usage: mobius [COMMAND]"), "{stdout}");
    assert!(stdout.contains("init"), "{stdout}");
    assert!(output.stderr.is_empty());
    assert_eq!(snapshot(setup.home.path()), before);
    assert!(setup.data_dir.join("mobius.db").is_file());
}

#[test]
fn help_flag_prints_help_and_changes_nothing() {
    assert_help_prints_and_changes_nothing("--help");
}

#[test]
fn short_help_flag_prints_help_and_changes_nothing() {
    assert_help_prints_and_changes_nothing("-h");
}

#[test]
fn unknown_argument_fails_and_changes_nothing() {
    let setup = Setup::new();
    let before = snapshot(setup.home.path());

    let output = setup.run(&["--bogus"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.starts_with("mobius: unknown argument `--bogus`"),
        "{stderr}"
    );
    assert!(stderr.contains("Usage: mobius [COMMAND]"), "{stderr}");
    assert_eq!(snapshot(setup.home.path()), before);
}
