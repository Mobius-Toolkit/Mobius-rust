#![cfg(feature = "server")]

use std::collections::BTreeMap;
use std::fs::{self, File, TryLockError};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use mobius_domain::Harness;
use mobius_store::NewSession;

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

    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mobius"));
        command
            .args(arguments)
            .env("HOME", self.home.path())
            .env("MOBIUS_CONFIG", self.home.path().join("config.toml"))
            .env("PORT", "0");
        command
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.command(arguments).output().unwrap()
    }

    fn add_open_session(&self) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let store = runtime
            .block_on(mobius_store::Store::open(&self.data_dir))
            .unwrap();
        runtime
            .block_on(store.sessions().add(NewSession {
                role: "lead",
                harness: Harness::ClaudeCode,
                model: "opus",
                organization: "owner",
                repository: "owner/repository",
                workstream: 1,
                issue: None,
                parent: None,
            }))
            .unwrap();
        runtime.block_on(store.pool.close());
    }

    fn open_session_count(&self) -> usize {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let store = runtime
            .block_on(mobius_store::Store::open(&self.data_dir))
            .unwrap();
        let count = runtime.block_on(store.sessions().open_ids()).unwrap().len();
        runtime.block_on(store.pool.close());
        count
    }

    fn start_server(&self) -> (Child, u16) {
        let stubs = self.home.path().join("stubs");
        fs::create_dir_all(&stubs).unwrap();
        for program in [
            "claude-agent-acp",
            "agy_acp_server",
            "devin",
            "gh",
            "curl",
            "tar",
        ] {
            let file = stubs.join(program);
            fs::write(&file, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::join_paths(
            std::iter::once(stubs).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let public = self.home.path().join("public");
        fs::create_dir_all(&public).unwrap();
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let server = self
            .command(&[])
            .env("PATH", path)
            .env("PORT", port.to_string())
            .env("DIOXUS_PUBLIC_PATH", public)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        (server, port)
    }

    fn lock_is_held(&self) -> bool {
        let Ok(file) = File::open(self.data_dir.join("mobius.lock")) else {
            return false;
        };
        matches!(file.try_lock(), Err(TryLockError::WouldBlock))
    }

    fn assert_server_holds_lock(&self, server: &mut Child, port: u16) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !server_answers(port) {
            assert!(server.try_wait().unwrap().is_none(), "the server stopped");
            assert!(Instant::now() < deadline, "the server did not answer");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(server.try_wait().unwrap().is_none(), "the server stopped");
        assert!(self.lock_is_held());
    }

    fn assert_lock_is_released(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.lock_is_held() {
            assert!(Instant::now() < deadline, "the lock stayed held");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn server_answers(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let request = "GET /api/devices HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let mut response = String::new();
    stream.write_all(request.as_bytes()).is_ok()
        && stream.read_to_string(&mut response).is_ok()
        && response.starts_with("HTTP/1.1 401")
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

#[test]
fn second_process_fails_and_changes_nothing() {
    let setup = Setup::new();
    setup.add_open_session();
    let lock = File::create(setup.data_dir.join("mobius.lock")).unwrap();
    lock.try_lock().unwrap();
    let before = snapshot(setup.home.path());

    let output = setup.run(&[]);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr,
        format!(
            "mobius: a Mobius server already uses the data directory {}\n",
            setup.data_dir.display()
        )
    );
    assert_eq!(snapshot(setup.home.path()), before);
    assert_eq!(setup.open_session_count(), 1);
}

#[test]
fn server_gets_the_lock_after_the_holder_is_killed() {
    let setup = Setup::new();
    let (mut first, port) = setup.start_server();
    setup.assert_server_holds_lock(&mut first, port);

    first.kill().unwrap();
    first.wait().unwrap();

    setup.assert_lock_is_released();
    let (mut second, port) = setup.start_server();
    setup.assert_server_holds_lock(&mut second, port);
    second.kill().unwrap();
    second.wait().unwrap();
}
