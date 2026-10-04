pub mod activity;
pub mod agents;
pub mod auth;
mod autopilot;
pub mod chat;
pub mod checks;
pub mod checkup;
pub mod config;
pub mod conflicts;
mod copy;
mod dispatch;
pub mod drain;
mod ends;
pub mod gh;
pub mod github;
mod housekeeper;
mod implementer;
pub mod inbox;
pub mod init;
mod issues;
mod judge;
pub mod labels;
mod lead;
mod lead_events;
pub mod limits;
pub mod mcp;
mod plans;
mod poll;
mod recovery;
mod researcher;
mod reviewer;
pub mod tasks;
mod threads;
pub mod transcript;
mod triager;
mod trust;
pub mod upgrade;
mod workers;
pub mod workstreams;

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::sync::{Arc, Mutex, RwLock};

use chat::ChatHandle;
use config::Config;
use mobius_domain::Live;
use mobius_github::{GitHub, IssueLinks, Repository};
use mobius_store::Store;
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;
use tokio::sync::broadcast;

const TIME_FORMAT: &[BorrowedFormatItem] =
    format_description!("[year]-[month]-[day] [hour]:[minute] UTC");

// The organization, the repository, and the Workstream. The Triager chat has the empty repository.
type ChatKey = (String, String, i64);

#[derive(Clone)]
pub struct Engine {
    pub config: Arc<Config>,
    pub store: Store,
    pub github: GitHub,
    harness_path: Arc<OsString>,
    port: u16,
    repositories: Arc<RwLock<Vec<Repository>>>,
    // The repositories where a full sync of the copy of the Workstream and task data succeeded in this run of the engine,
    // with the links of their open issues as the copy last saw them.
    copied: Arc<Mutex<HashMap<String, BTreeMap<i64, IssueLinks>>>>,
    // The repositories where the poll tried a label fix in this run of the engine, with success or failure.
    labels_fixed: Arc<Mutex<std::collections::HashSet<String>>>,
    chats: Arc<Mutex<HashMap<ChatKey, ChatHandle>>>,
    // A chat entry that goes to a Lead session takes its place in the chat and in the session under this lock, so the two orders stay equal.
    chat_order: Arc<tokio::sync::Mutex<()>>,
    callers: Arc<Mutex<HashMap<String, mcp::Caller>>>,
    live: broadcast::Sender<Live>,
    // All tasks of a repository share one bare clone, and two git commands that write its refs at the same time can fail on a ref lock.
    git: Arc<tokio::sync::Mutex<()>>,
    workers: Arc<workers::Workers>,
    checks: Arc<tokio::sync::Semaphore>,
    // Each send carries the id of a task that stops or ends.
    stops: broadcast::Sender<i64>,
    // Each send carries the repository and the number of a Workstream whose Lead stops.
    lead_stops: broadcast::Sender<(String, i64)>,
    // The repositories that the first poll after start checked.
    recovered: Arc<Mutex<std::collections::HashSet<String>>>,
    // Two sessions at the same usage limit, or two checks on a full disk, make one pause.
    pausing: Arc<tokio::sync::Mutex<()>>,
    // Each end of a pause of a Harness wakes the sessions that wait for it.
    pauses_changed: Arc<tokio::sync::Notify>,
    // The drain for an upgrade.
    drain: Arc<drain::Drain>,
    // Set while an upgrade runs.
    upgrading: Arc<std::sync::atomic::AtomicBool>,
    // The error of the last upgrade, for a page that opens after the error.
    upgrade_error: Arc<Mutex<Option<String>>>,
    // The Housekeeper wakes the checks that wait for free disk space.
    disk_freed: Arc<tokio::sync::Notify>,
    // The stop signal of the Triager session of each issue.
    triages: Arc<Mutex<HashMap<(String, i64), triager::Stop>>>,
    // For each task, the time of its newest Judge item and the moment when Mobius first saw it.
    quiet: Arc<Mutex<HashMap<i64, (time::OffsetDateTime, std::time::Instant)>>>,
}

impl Engine {
    fn repository(&self, name: &str) -> Result<Repository, String> {
        self.repositories
            .read()
            .unwrap()
            .iter()
            .find(|repository| repository.full_name == name)
            .cloned()
            .ok_or_else(|| format!("The Mobius App has no access to {name}."))
    }

    fn broadcast(&self, live: Live) {
        // With no open feed, the channel has no receiver and the send fails.
        let _ = self.live.send(live);
    }
}

pub async fn start(
    config: Config,
    store: Store,
    github_api_url: &str,
    github_web_url: &str,
    harness_path: OsString,
    port: u16,
) -> Result<Engine, Box<dyn Error + Send + Sync>> {
    let gh =
        mobius_runner::find_gh(&config.data_dir, &harness_path).ok_or("`gh` is not on PATH")?;
    mobius_runner::prepare(&config.data_dir, &gh)?;
    let checks = Arc::new(tokio::sync::Semaphore::new(config.max_checks as usize));
    let engine = Engine {
        config: Arc::new(config),
        store,
        github: GitHub::new(github_api_url, github_web_url)?,
        harness_path: Arc::new(harness_path),
        port,
        repositories: Arc::default(),
        copied: Arc::default(),
        labels_fixed: Arc::default(),
        chats: Arc::default(),
        chat_order: Arc::default(),
        callers: Arc::default(),
        live: broadcast::channel(256).0,
        git: Arc::default(),
        workers: Arc::default(),
        checks,
        stops: broadcast::channel(64).0,
        lead_stops: broadcast::channel(16).0,
        recovered: Arc::default(),
        pausing: Arc::default(),
        pauses_changed: Arc::default(),
        drain: Arc::default(),
        upgrading: Arc::default(),
        upgrade_error: Arc::default(),
        disk_freed: Arc::default(),
        triages: Arc::default(),
        quiet: Arc::default(),
    };
    auth::start(&engine).await?;
    limits::start(&engine).await?;
    recovery::start(&engine).await?;
    poll::spawn(engine.clone());
    housekeeper::spawn(engine.clone());
    Ok(engine)
}

fn random_hex() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn missing_commands(config: &Config, path: &OsStr) -> Vec<&'static str> {
    let mut programs: Vec<&'static str> = config
        .roles
        .bindings()
        .iter()
        .map(|(_, binding)| mobius_runner::program(binding.harness))
        .collect();
    programs.push("gh");
    programs.push("curl");
    programs.push("tar");
    programs.sort();
    programs.dedup();
    programs.retain(|program| mobius_runner::find(program, path).is_none());
    programs
}
