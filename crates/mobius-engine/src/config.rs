use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use mobius_domain::Harness;
use serde::{Deserialize, Deserializer};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub access_password: String,
    pub trusted_users: Vec<String>,
    #[serde(default)]
    pub trusted_bots: Vec<String>,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "default_max_agents")]
    pub max_agents: u32,
    #[serde(default = "default_max_checks")]
    pub max_checks: u32,
    #[serde(default = "default_max_fix_rounds")]
    pub max_fix_rounds: u32,
    #[serde(default = "default_three")]
    pub max_check_attempts: u32,
    #[serde(default = "default_three")]
    pub max_worker_restarts: u32,
    #[serde(with = "humantime_serde", default = "default_check_timeout")]
    pub check_timeout: Duration,
    #[serde(with = "humantime_serde", default = "default_review_quiet_period")]
    pub review_quiet_period: Duration,
    #[serde(with = "humantime_serde", default = "default_stale_pr_age")]
    pub stale_pr_age: Duration,
    #[serde(with = "humantime_serde", default = "default_lead_idle_timeout")]
    pub lead_idle_timeout: Duration,
    #[serde(with = "humantime_serde", default = "default_poll_interval")]
    pub poll_interval: Duration,
    #[serde(with = "humantime_serde", default = "default_housekeeper_interval")]
    pub housekeeper_interval: Duration,
    // The wait before the first, second, and later Worker restarts. The file has no key for it.
    #[serde(skip, default = "default_restart_waits")]
    pub restart_waits: Vec<Duration>,
    pub roles: Roles,
}

impl Config {
    // The wait before restart number `restarts` of a task, counted from 1. Each restart after the last listed wait uses the last wait.
    pub fn restart_wait(&self, restarts: i64) -> Duration {
        let index = usize::try_from(restarts - 1).unwrap_or_default();
        self.restart_waits[index.min(self.restart_waits.len() - 1)]
    }
}

#[derive(Debug)]
pub struct Roles {
    pub lead: RoleBinding,
    pub triager: RoleBinding,
    pub implementer: RoleBinding,
    pub researcher: RoleBinding,
    pub reviewer: RoleBinding,
    pub judge: RoleBinding,
}

// The default `max` and `counts_in_max_agents` differ for each role, so the raw binding keeps them optional.
impl<'de> Deserialize<'de> for Roles {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct RawRoles {
            lead: RawRoleBinding,
            triager: RawRoleBinding,
            implementer: RawRoleBinding,
            researcher: RawRoleBinding,
            reviewer: RawRoleBinding,
            judge: RawRoleBinding,
        }
        let raw = RawRoles::deserialize(deserializer)?;
        Ok(Self {
            lead: raw.lead.with_defaults(8, false),
            triager: raw.triager.with_defaults(2, false),
            implementer: raw.implementer.with_defaults(2, true),
            researcher: raw.researcher.with_defaults(2, true),
            reviewer: raw.reviewer.with_defaults(2, true),
            judge: raw.judge.with_defaults(2, true),
        })
    }
}

impl Roles {
    pub fn bindings(&self) -> [(&'static str, &RoleBinding); 6] {
        [
            ("lead", &self.lead),
            ("triager", &self.triager),
            ("implementer", &self.implementer),
            ("researcher", &self.researcher),
            ("reviewer", &self.reviewer),
            ("judge", &self.judge),
        ]
    }
}

#[derive(Debug)]
pub struct RoleBinding {
    pub harness: Harness,
    pub model: String,
    pub effort: Option<String>,
    pub max: u32,
    pub counts_in_max_agents: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoleBinding {
    harness: Harness,
    model: String,
    effort: Option<String>,
    max: Option<u32>,
    counts_in_max_agents: Option<bool>,
}

impl RawRoleBinding {
    fn with_defaults(self, max: u32, global: bool) -> RoleBinding {
        RoleBinding {
            harness: self.harness,
            model: self.model,
            effort: self.effort,
            max: self.max.unwrap_or(max),
            counts_in_max_agents: self.counts_in_max_agents.unwrap_or(global),
        }
    }
}

pub(crate) const EFFORT_LEVEL_HARNESSES: [Harness; 2] = [Harness::ClaudeCode, Harness::Devin];

fn default_data_dir() -> PathBuf {
    std::env::home_dir().unwrap_or_default().join(".mobius")
}

fn default_max_agents() -> u32 {
    4
}

fn default_max_checks() -> u32 {
    1
}

fn default_max_fix_rounds() -> u32 {
    7
}

fn default_three() -> u32 {
    3
}

fn default_check_timeout() -> Duration {
    Duration::from_secs(15 * 60)
}

fn default_review_quiet_period() -> Duration {
    Duration::from_secs(10 * 60)
}

fn default_stale_pr_age() -> Duration {
    Duration::from_secs(7 * 24 * 60 * 60)
}

fn default_lead_idle_timeout() -> Duration {
    Duration::from_secs(60 * 60)
}

fn default_poll_interval() -> Duration {
    Duration::from_secs(30)
}

fn default_housekeeper_interval() -> Duration {
    Duration::from_secs(60 * 60)
}

fn default_restart_waits() -> Vec<Duration> {
    [1, 5, 15]
        .into_iter()
        .map(|minutes| Duration::from_secs(minutes * 60))
        .collect()
}

pub fn load(path: &Path) -> Result<Config, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("{}: cannot read: {error}", path.display()))?;
    parse(&text).map_err(|error| format!("{}: {error}", path.display()))
}

pub fn parse(text: &str) -> Result<Config, String> {
    let deserializer = toml::Deserializer::parse(text).map_err(|error| {
        let line = error
            .span()
            .map_or(1, |span| text[..span.start].lines().count().max(1));
        format!("line {line}: {}", error.message())
    })?;
    let config: Config = serde_path_to_error::deserialize(deserializer).map_err(|error| {
        let key = error.path().to_string();
        let message = error.inner().message();
        if key == "." {
            message.to_string()
        } else {
            format!("{key}: {message}")
        }
    })?;
    if config.access_password.chars().count() < 8 {
        return Err("access_password: must have at least 8 characters".to_string());
    }
    if config.trusted_users.is_empty() {
        return Err("trusted_users: must not be empty".to_string());
    }
    for (role, binding) in config.roles.bindings() {
        let harness = binding.harness.name();
        match (
            EFFORT_LEVEL_HARNESSES.contains(&binding.harness),
            &binding.effort,
        ) {
            (true, None) => return Err(format!("roles.{role}.effort: is required for {harness}")),
            (false, Some(_)) => {
                return Err(format!("roles.{role}.effort: is not allowed for {harness}"));
            }
            _ => {}
        }
    }
    let (reviewer, implementer) = (&config.roles.reviewer, &config.roles.implementer);
    if reviewer.harness == implementer.harness && reviewer.model == implementer.model {
        return Err(
            "roles.reviewer: must not have the same harness and model as roles.implementer"
                .to_string(),
        );
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
access_password = "correct horse"
trusted_users = ["owner"]

[roles]
lead        = { harness = "claude-code", model = "opus",    effort = "high" }
triager     = { harness = "claude-code", model = "sonnet",  effort = "medium" }
implementer = { harness = "devin",       model = "swe-1.5", effort = "high" }
researcher  = { harness = "antigravity", model = "gemini-3-pro" }
reviewer    = { harness = "claude-code", model = "opus",    effort = "high" }
judge       = { harness = "claude-code", model = "haiku",   effort = "low" }
"#;

    #[test]
    fn parses_a_valid_file_with_defaults() {
        let config = parse(VALID).unwrap();

        assert_eq!(config.access_password, "correct horse");
        assert_eq!(config.trusted_users, ["owner"]);
        assert!(config.trusted_bots.is_empty());
        assert_eq!(
            config.data_dir,
            std::env::home_dir().unwrap().join(".mobius")
        );
        assert_eq!(config.max_agents, 4);
        assert_eq!(config.max_checks, 1);
        assert_eq!(config.max_fix_rounds, 7);
        assert_eq!(config.max_check_attempts, 3);
        assert_eq!(config.max_worker_restarts, 3);
        assert_eq!(config.check_timeout, Duration::from_secs(15 * 60));
        assert_eq!(config.stale_pr_age, Duration::from_secs(7 * 24 * 60 * 60));
        assert_eq!(config.poll_interval, Duration::from_secs(30));
        assert_eq!(config.roles.researcher.harness, Harness::Antigravity);
        assert_eq!(config.roles.researcher.effort, None);
        assert_eq!(config.roles.implementer.model, "swe-1.5");
        for (role, binding) in config.roles.bindings() {
            let max = if role == "lead" { 8 } else { 2 };
            assert_eq!(binding.max, max, "{role}");
            assert_eq!(
                binding.counts_in_max_agents,
                !matches!(role, "lead" | "triager"),
                "{role}"
            );
        }
    }

    #[test]
    fn the_restart_wait_grows_with_each_restart_up_to_the_last_wait() {
        let config = parse(VALID).unwrap();

        let waits: Vec<u64> = (1..=5)
            .map(|restarts| config.restart_wait(restarts).as_secs() / 60)
            .collect();

        assert_eq!(waits, [1, 5, 15, 15, 15]);
    }

    #[test]
    fn the_file_has_no_key_for_the_restart_waits() {
        let error = parse(&format!("restart_waits = []\n{VALID}")).unwrap_err();

        assert!(error.contains("restart_waits"), "{error}");
    }

    #[test]
    fn parses_the_max_of_each_role_and_the_global_limit() {
        let text = VALID
            .replace(
                "implementer = { harness = \"devin\",       model = \"swe-1.5\", effort = \"high\" }",
                "implementer = { harness = \"devin\", model = \"swe-1.5\", effort = \"high\", max = 3 }",
            )
            .replace(
                "judge       = { harness = \"claude-code\", model = \"haiku\",   effort = \"low\" }",
                "judge       = { harness = \"claude-code\", model = \"haiku\", effort = \"low\", counts_in_max_agents = false }",
            );
        let config = parse(&format!("max_agents = 1\n{text}")).unwrap();

        assert_eq!(config.max_agents, 1);
        assert_eq!(config.roles.implementer.max, 3);
        assert_eq!(config.roles.judge.max, 2);
        assert!(!config.roles.judge.counts_in_max_agents);
        assert!(config.roles.implementer.counts_in_max_agents);
    }

    fn error(text: &str) -> String {
        parse(text).unwrap_err()
    }

    #[test]
    fn refuses_a_missing_required_key() {
        assert_eq!(
            error(&VALID.replace("access_password = \"correct horse\"\n", "")),
            "missing field `access_password`"
        );
        assert_eq!(
            error(&VALID.replace("judge ", "#judge ")),
            "roles: missing field `judge`"
        );
    }

    #[test]
    fn refuses_a_wrong_value() {
        assert_eq!(
            error(&format!("max_checks = \"one\"\n{VALID}")),
            "max_checks: invalid type: string \"one\", expected u32"
        );
        assert_eq!(
            error(&format!("poll_interval = \"soon\"\n{VALID}")),
            "poll_interval: invalid value: string \"soon\", expected a duration"
        );
    }

    #[test]
    fn refuses_an_unknown_key() {
        assert!(
            error(&format!("pool_interval = \"1s\"\n{VALID}"))
                .starts_with("pool_interval: unknown field `pool_interval`, expected one of")
        );
        assert_eq!(
            error(&VALID.replace("model = \"haiku\"", "modle = \"haiku\"")),
            "roles.judge.modle: unknown field `modle`, expected one of `harness`, `model`, `effort`, `max`, `counts_in_max_agents`"
        );
    }

    #[test]
    fn refuses_the_removed_worker_limits() {
        assert!(
            error(&format!("max_workers = {{ devin = 1 }}\n{VALID}"))
                .starts_with("max_workers: unknown field `max_workers`, expected one of")
        );
        assert!(
            error(&format!("max_workers_total = 1\n{VALID}")).starts_with(
                "max_workers_total: unknown field `max_workers_total`, expected one of"
            )
        );
    }

    #[test]
    fn refuses_an_unknown_harness() {
        assert_eq!(
            error(&VALID.replace("\"devin\"", "\"codex\"")),
            "roles.implementer.harness: unknown variant `codex`, expected one of `claude-code`, `antigravity`, `devin`"
        );
    }

    #[test]
    fn refuses_a_syntax_error() {
        assert_eq!(
            error(&format!("{VALID}\n[roles")),
            "line 13: unclosed table, expected `]`"
        );
    }

    #[test]
    fn refuses_a_short_access_password() {
        assert_eq!(
            error(&VALID.replace("correct horse", "short")),
            "access_password: must have at least 8 characters"
        );
    }

    #[test]
    fn refuses_empty_trusted_users() {
        assert_eq!(
            error(&VALID.replace("[\"owner\"]", "[]")),
            "trusted_users: must not be empty"
        );
    }

    #[test]
    fn refuses_a_missing_effort() {
        assert_eq!(
            error(&VALID.replace(",   effort = \"low\"", "")),
            "roles.judge.effort: is required for claude-code"
        );
        assert_eq!(
            error(&VALID.replace("\"swe-1.5\", effort = \"high\"", "\"swe-1.5\"")),
            "roles.implementer.effort: is required for devin"
        );
    }

    #[test]
    fn refuses_an_effort_for_a_harness_without_effort_levels() {
        assert_eq!(
            error(&VALID.replace("\"gemini-3-pro\"", "\"gemini-3-pro\", effort = \"high\"")),
            "roles.researcher.effort: is not allowed for antigravity"
        );
    }

    #[test]
    fn refuses_a_reviewer_with_the_harness_and_model_of_the_implementer() {
        let text = VALID.replace(
            "reviewer    = { harness = \"claude-code\", model = \"opus\",    effort = \"high\" }",
            "reviewer    = { harness = \"devin\", model = \"swe-1.5\", effort = \"low\" }",
        );
        assert_eq!(
            error(&text),
            "roles.reviewer: must not have the same harness and model as roles.implementer"
        );
    }

    #[test]
    fn refuses_a_file_that_does_not_exist() {
        let error = load(Path::new("/nonexistent/config.toml")).unwrap_err();

        assert!(
            error.starts_with("/nonexistent/config.toml: cannot read:"),
            "{error}"
        );
    }
}
