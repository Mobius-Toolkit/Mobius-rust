use std::io::Write;
use std::process::{Command, Stdio};

fn ui_changed(files: &str) -> String {
    let script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../.github/scripts/ui-changed.sh"
    );
    let mut child = Command::new(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(files.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[test]
fn docs_change_does_not_run_the_screenshots() {
    assert_eq!(ui_changed("docs/install.md\n"), "false");
}

#[test]
fn ui_file_runs_the_screenshots() {
    assert_eq!(
        ui_changed("docs/install.md\ncrates/mobius-ui/src/lib.rs\n"),
        "true"
    );
}

#[test]
fn engine_code_runs_the_screenshots() {
    assert_eq!(ui_changed("crates/mobius-engine/src/checkup.rs\n"), "true");
}

#[test]
fn testkit_source_runs_the_screenshots() {
    assert_eq!(ui_changed("crates/mobius-testkit/src/lib.rs\n"), "true");
}
