use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::Duration;

use mobius_domain::{DrainEnd, Live};
use tempfile::TempDir;
use tokio::sync::OwnedMutexGuard;

use crate::Engine;
use crate::drain::{self, Seal};

type Failure = Box<dyn Error + Send + Sync>;

// The web UI gets the response of the upgrade call before the process ends.
const RESTART_DELAY: Duration = Duration::from_secs(1);

// The new release, extracted next to the running program. The directory is on the file system of the program, so each rename stays on one file system.
struct Staging {
    dir: TempDir,
    exe: PathBuf,
}

// The error of the last upgrade, or `None` when the last upgrade has no error.
pub fn last_error(engine: &Engine) -> Option<String> {
    engine.upgrade_error.lock().unwrap().clone()
}

fn set_error(engine: &Engine, error: Option<String>) {
    *engine.upgrade_error.lock().unwrap() = error.clone();
    engine.broadcast(Live::UpgradeError(error));
}

// The drain has no time limit, so the work runs in its own task. A browser that closes the call does not stop the upgrade, and the error stays in the engine for the next page.
pub async fn run(engine: &Engine) -> Result<DrainEnd, Failure> {
    if engine.upgrading.swap(true, Ordering::SeqCst) {
        return Err("An upgrade runs now.".into());
    }
    set_error(engine, None);
    let task = tokio::spawn({
        let engine = engine.clone();
        async move {
            let result = upgrade(&engine).await;
            if let Err(error) = &result {
                set_error(&engine, Some(error.to_string()));
            }
            engine.upgrading.store(false, Ordering::SeqCst);
            result
        }
    });
    task.await?
}

async fn upgrade(engine: &Engine) -> Result<DrainEnd, Failure> {
    let Some(current) = mobius_domain::RELEASE_VERSION else {
        return Err("This Mobius build is not a release.".into());
    };
    let release = engine.any_repository()?.latest_release().await?;
    if !mobius_domain::newer_release(current, &release.tag_name) {
        return Err(format!("{} is the newest release.", release.tag_name).into());
    }
    let asset = format!("mobius-{}.tar.gz", target()?);
    if !release.assets.iter().any(|known| known.name == asset) {
        return Err(format!("Release {} has no {asset}.", release.tag_name).into());
    }
    let url = engine.github.release_url(&release.tag_name, &asset);
    let staging = tokio::task::spawn_blocking(move || download(&url)).await??;
    // A chat Lead can start between the end of the drain and the seal, so the wait repeats.
    loop {
        if drain::start(engine).await == DrainEnd::Cancelled {
            return Ok(DrainEnd::Cancelled);
        }
        match drain::seal(engine) {
            Seal::Sealed => break,
            Seal::Busy => {}
            Seal::Cancelled => return Ok(DrainEnd::Cancelled),
        }
    }
    // A git command of the old process must not run during `exec`.
    let git = engine.git.clone().lock_owned().await;
    let exe = staging.exe.clone();
    if let Err(error) = tokio::task::spawn_blocking(move || swap(&staging)).await? {
        drop(git);
        let _ = drain::abort(engine).await;
        return Err(error);
    }
    restart(exe, git);
    Ok(DrainEnd::Drained)
}

fn target() -> Result<&'static str, Failure> {
    let (arch, os) = (std::env::consts::ARCH, std::env::consts::OS);
    match (arch, os) {
        ("aarch64", "macos") => Ok("aarch64-apple-darwin"),
        ("x86_64", "linux") => Ok("x86_64-unknown-linux-gnu"),
        _ => Err(format!("Mobius has no release for {arch} {os}.").into()),
    }
}

fn download(url: &str) -> Result<Staging, Failure> {
    // After the swap, `current_exe` can name the old file, so the path is fixed here.
    let exe = std::env::current_exe()?.canonicalize()?;
    let dir = exe.parent().ok_or("The mobius program has no directory.")?;
    let staging = tempfile::tempdir_in(dir)?;
    let archive = staging.path().join("mobius.tar.gz");
    run_command(
        Command::new("curl")
            .args(["-fsSL", url, "-o"])
            .arg(&archive),
    )?;
    run_command(
        Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .current_dir(staging.path()),
    )?;
    if !staging.path().join("mobius").is_file() {
        return Err(format!("The archive at {url} has no mobius program.").into());
    }
    Ok(Staging { dir: staging, exe })
}

// The old files move aside first, so a failed swap can move them back.
fn swap(staging: &Staging) -> Result<(), Failure> {
    let dir = staging
        .exe
        .parent()
        .ok_or("The mobius program has no directory.")?;
    let previous = staging.dir.path().join("previous");
    fs::create_dir(&previous)?;
    let result = replace(staging.dir.path(), &previous, dir, &staging.exe);
    if result.is_err() {
        let _ = fs::rename(previous.join("mobius"), &staging.exe);
        let _ = fs::rename(previous.join("public"), dir.join("public"));
    }
    result
}

fn replace(staging: &Path, previous: &Path, dir: &Path, exe: &Path) -> Result<(), Failure> {
    let public = dir.join("public");
    fs::rename(exe, previous.join("mobius"))?;
    if public.exists() {
        fs::rename(&public, previous.join("public"))?;
    }
    fs::rename(staging.join("mobius"), exe)?;
    if staging.join("public").is_dir() {
        fs::rename(staging.join("public"), public)?;
    }
    Ok(())
}

fn run_command(command: &mut Command) -> Result<(), Failure> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!(
            "`{program}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(())
}

// `exec` keeps the environment, the working directory, and the terminal. A child process continues to run after `exec`, and the new program does not know it. Thus the drain must be complete, and the git lock must be held until the call.
fn restart(exe: PathBuf, git: OwnedMutexGuard<()>) {
    use std::os::unix::process::CommandExt;
    tokio::spawn(async move {
        let _git = git;
        tokio::time::sleep(RESTART_DELAY).await;
        let error = Command::new(&exe).args(std::env::args_os().skip(1)).exec();
        eprintln!("mobius: the restart after the upgrade failed: {error}");
        std::process::exit(1);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged(dir: &Path) -> Staging {
        let exe = dir.join("mobius");
        fs::write(&exe, "old").unwrap();
        fs::create_dir(dir.join("public")).unwrap();
        fs::write(dir.join("public/index.html"), "old").unwrap();
        let staging = tempfile::tempdir_in(dir).unwrap();
        fs::write(staging.path().join("mobius"), "new").unwrap();
        Staging { dir: staging, exe }
    }

    #[test]
    fn a_swap_replaces_the_program_and_the_public_directory() {
        let root = tempfile::tempdir().unwrap();
        let staging = staged(root.path());
        fs::create_dir(staging.dir.path().join("public")).unwrap();
        fs::write(staging.dir.path().join("public/index.html"), "new").unwrap();
        swap(&staging).unwrap();
        assert_eq!(fs::read_to_string(&staging.exe).unwrap(), "new");
        assert_eq!(
            fs::read_to_string(root.path().join("public/index.html")).unwrap(),
            "new"
        );
    }

    #[test]
    fn a_failed_swap_restores_the_old_files() {
        let root = tempfile::tempdir().unwrap();
        let staging = staged(root.path());
        // The new program is missing, so the third rename of the swap fails.
        fs::remove_file(staging.dir.path().join("mobius")).unwrap();
        assert!(swap(&staging).is_err());
        assert_eq!(fs::read_to_string(&staging.exe).unwrap(), "old");
        assert_eq!(
            fs::read_to_string(root.path().join("public/index.html")).unwrap(),
            "old"
        );
    }
}
