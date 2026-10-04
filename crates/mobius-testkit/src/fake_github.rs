use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::TempDir;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::git;

pub const APP_ID: i64 = 7;
pub const APP_SLUG: &str = "mobius-test";
pub const APP_PRIVATE_KEY: &str = include_str!("app_private_key.pem");
pub const APP_CLIENT_ID: &str = "Iv23test";
pub const APP_CLIENT_SECRET: &str = "client-secret";
pub const SECOND_APP_ID: i64 = 8;
pub const SECOND_APP_SLUG: &str = "mobius-second";
pub const SECOND_APP_CLIENT_ID: &str = "Iv23second";
pub const SECOND_APP_CLIENT_SECRET: &str = "second-client-secret";
pub const BOT_USER_ID: i64 = 41898282;
pub const INSTALLATION_TOKEN: &str = "ghs_installation";
const SECOND_INSTALLATION_TOKEN: &str = "ghs_second_installation";
// The id of an issue is its number plus this offset, so a number in place of an id finds no issue.
pub const ISSUE_ID_OFFSET: i64 = 100_000;

// The permissions of the Mobius App before the Owner adds `workflows`.
const DEFAULT_PERMISSIONS: [(&str, &str); 5] = [
    ("issues", "write"),
    ("pull_requests", "write"),
    ("contents", "write"),
    ("checks", "write"),
    ("metadata", "read"),
];

struct App {
    id: i64,
    slug: &'static str,
    client_id: &'static str,
    client_secret: &'static str,
    installation_token: &'static str,
}

// The manifest conversions create the Apps in this order. The installation id of an App is its index plus 1.
const APPS: [App; 2] = [
    App {
        id: APP_ID,
        slug: APP_SLUG,
        client_id: APP_CLIENT_ID,
        client_secret: APP_CLIENT_SECRET,
        installation_token: INSTALLATION_TOKEN,
    },
    App {
        id: SECOND_APP_ID,
        slug: SECOND_APP_SLUG,
        client_id: SECOND_APP_CLIENT_ID,
        client_secret: SECOND_APP_CLIENT_SECRET,
        installation_token: SECOND_INSTALLATION_TOKEN,
    },
];

#[derive(Clone, Debug, PartialEq)]
pub struct PullRequest {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub head: String,
    pub base: String,
    pub draft: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CheckRun {
    pub name: String,
    pub head_sha: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub output: Option<CheckRunOutput>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CheckRunOutput {
    pub title: String,
    pub summary: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SubmittedReview {
    pub commit_id: String,
    pub body: String,
    pub event: String,
    pub comments: Vec<InlineComment>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct InlineComment {
    pub path: String,
    pub line: i64,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Thread {
    pub resolved: bool,
    // The author and the body of each comment, in order.
    pub comments: Vec<(String, String)>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RepositoryLabel {
    pub name: String,
    pub color: String,
    pub description: String,
}

// The next request for the events of one issue stops until `release` gets a permit, and it gives `reached` a permit when it stops.
struct EventsGate {
    repository: String,
    number: i64,
    reached: Arc<Notify>,
    release: Arc<Notify>,
}

pub struct EventsHold {
    pub reached: Arc<Notify>,
    pub release: Arc<Notify>,
}

#[derive(Default)]
struct Records {
    events_gate: Option<EventsGate>,
    account_types: HashMap<String, &'static str>,
    manifest_codes: HashSet<String>,
    apps_created: usize,
    // The accounts that installed the second App. The first App has all other accounts.
    second_app_accounts: HashSet<String>,
    // The ids of the Apps whose installation list fails.
    failed_apps: HashSet<i64>,
    // The issues whose close request fails, as (repository, number).
    failed_closes: HashSet<(String, i64)>,
    // The issues whose sub-issue list request fails, as (repository, number).
    failed_sub_issues: HashSet<(String, i64)>,
    // The Unix time at which the exhausted core rate limit resets. Without it, the limit has calls left.
    rate_limit_reset: Option<i64>,
    // The repositories whose pull request creation fails, with the status and the message.
    failed_pull_requests: HashMap<String, (StatusCode, String)>,
    // The permissions of each App and of its installation by App id. An App without an entry has `DEFAULT_PERMISSIONS`.
    app_permissions: HashMap<i64, HashMap<String, String>>,
    installation_permissions: HashMap<i64, HashMap<String, String>>,
    // The login and the App index of each code and refresh token.
    user_codes: HashMap<String, (String, usize)>,
    user_tokens: HashMap<String, String>,
    refresh_tokens: HashMap<String, (String, usize)>,
    tokens_given: u32,
    repositories: Vec<String>,
    remotes: PathBuf,
    issues: BTreeMap<(String, i64), Issue>,
    pull_requests: Vec<(String, PullRequest)>,
    // The creation time of each pull request, in seconds after the Unix epoch.
    pull_request_created_at: HashMap<(String, i64), i64>,
    behind_pull_requests: HashSet<(String, i64)>,
    // The id of a check run is its index plus 1.
    check_runs: Vec<(String, CheckRun)>,
    // The annotations of each check run by id, as GitHub gives them.
    annotations: HashMap<usize, Vec<Value>>,
    // The slug of the App of each check run by id. A check run with no entry has no App.
    check_run_apps: HashMap<usize, String>,
    // The job log of each check run by id. A check run with no log gives `404`.
    job_logs: HashMap<usize, String>,
    submitted_reviews: Vec<(String, i64, SubmittedReview)>,
    // Issue comments and review comments share the ids, so an id names one comment.
    last_comment_id: i64,
    // The id of the first comment of each resolved review thread.
    resolved_threads: HashSet<i64>,
    // The labels of each repository by (repository, name).
    repository_labels: BTreeMap<(String, String), RepositoryLabel>,
    // The (repository, name) of each label that got a PATCH request, in request order.
    label_patches: Vec<(String, String)>,
    // The tag of the latest release. Without a tag, the repository has no release.
    latest_release: Option<String>,
    // The commit messages of each comparison, the oldest first.
    compared_commit_messages: Vec<String>,
    clock: i64,
    not_modified: u32,
}

struct Issue {
    title: String,
    body: String,
    author: String,
    pull_request: bool,
    merged_at: Option<i64>,
    state_reason: Option<String>,
    state: &'static str,
    // A sub-issue lives in the repository of its own issue, which can differ from
    // the repository of the parent, so a child is a (repository, number) pair.
    sub_issues: Vec<(String, i64)>,
    blocked_by: Vec<i64>,
    labels: Vec<String>,
    updated_at: i64,
    events: Vec<Value>,
    comments: Vec<Value>,
    reviews: Vec<Value>,
    review_comments: Vec<Value>,
}

impl Records {
    fn app_index(&self, repository: &str) -> usize {
        let account = repository.split('/').next().unwrap_or_default();
        usize::from(self.second_app_accounts.contains(account))
    }

    fn permissions(
        permissions: &HashMap<i64, HashMap<String, String>>,
        app_id: i64,
    ) -> HashMap<String, String> {
        permissions.get(&app_id).cloned().unwrap_or_else(|| {
            DEFAULT_PERMISSIONS
                .iter()
                .map(|(name, level)| (name.to_string(), level.to_string()))
                .collect()
        })
    }

    fn app_login(&self, repository: &str) -> String {
        format!("{}[bot]", APPS[self.app_index(repository)].slug)
    }

    // The actor of a label write is the user of a `ghu_` token, or the App bot for an installation token.
    fn actor(&self, repository: &str, headers: &HeaderMap) -> String {
        self.user_tokens
            .get(bearer(headers))
            .cloned()
            .unwrap_or_else(|| self.app_login(repository))
    }

    fn insert_pull_request(&mut self, repository: &str, new: NewPullRequest) -> i64 {
        let bot = self.app_login(repository);
        let number = self
            .issues
            .keys()
            .filter(|(name, _)| *name == repository)
            .map(|(_, number)| number + 1)
            .max()
            .unwrap_or(1);
        let updated_at = self.tick();
        self.issues.insert(
            (repository.to_string(), number),
            Issue {
                title: new.title.clone(),
                body: new.body.clone(),
                author: bot.clone(),
                pull_request: true,
                merged_at: None,
                state_reason: None,
                state: "open",
                sub_issues: Vec::new(),
                blocked_by: Vec::new(),
                labels: Vec::new(),
                updated_at,
                events: Vec::new(),
                comments: Vec::new(),
                reviews: Vec::new(),
                review_comments: Vec::new(),
            },
        );
        self.pull_request_created_at
            .insert((repository.to_string(), number), updated_at);
        self.pull_requests.push((
            repository.to_string(),
            PullRequest {
                number,
                title: new.title,
                body: new.body,
                head: new.head,
                base: new.base,
                draft: new.draft,
            },
        ));
        number
    }

    fn insert_issue(
        &mut self,
        repository: &str,
        number: i64,
        title: &str,
        body: &str,
        author: &str,
        pull_request: bool,
    ) {
        let updated_at = self.tick();
        self.issues.insert(
            (repository.to_string(), number),
            Issue {
                title: title.to_string(),
                body: body.to_string(),
                author: author.to_string(),
                pull_request,
                merged_at: None,
                state_reason: None,
                state: "open",
                sub_issues: Vec::new(),
                blocked_by: Vec::new(),
                labels: Vec::new(),
                updated_at,
                events: Vec::new(),
                comments: Vec::new(),
                reviews: Vec::new(),
                review_comments: Vec::new(),
            },
        );
    }

    // Each write is one second after the last, so `since` compares exactly.
    fn tick(&mut self) -> i64 {
        self.clock += 1;
        self.clock
    }

    fn comment(
        &mut self,
        repository: &str,
        number: i64,
        author: &str,
        body: &str,
        app: Option<&str>,
    ) -> Value {
        let now = self.tick();
        self.last_comment_id += 1;
        let comment = json!({
            "id": self.last_comment_id,
            "user": { "login": author },
            "body": body,
            "created_at": timestamp(now),
            "performed_via_github_app": app.map(|slug| json!({ "slug": slug }))
        });
        let issue = self
            .issues
            .get_mut(&(repository.to_string(), number))
            .unwrap();
        issue.comments.push(comment.clone());
        issue.updated_at = now;
        comment
    }

    // Gives the id of the new review comment.
    fn review_comment(
        &mut self,
        repository: &str,
        number: i64,
        in_reply_to: Option<i64>,
        author: &str,
        comment: &InlineComment,
    ) -> i64 {
        let now = self.tick();
        self.last_comment_id += 1;
        let id = self.last_comment_id;
        self.issues
            .get_mut(&(repository.to_string(), number))
            .unwrap()
            .review_comments
            .push(json!({
                "id": id,
                "user": { "login": author },
                "body": comment.body,
                "path": comment.path,
                "line": comment.line,
                "in_reply_to_id": in_reply_to,
                "created_at": timestamp(now)
            }));
        id
    }

    fn label(&mut self, repository: &str, number: i64, label: &str, actor: &str) {
        let key = (repository.to_string(), number);
        // GitHub records no `labeled` event for a label the issue already has.
        if self.issues[&key].labels.iter().any(|name| name == label) {
            return;
        }
        let now = self.tick();
        let issue = self.issues.get_mut(&key).unwrap();
        issue.labels.push(label.to_string());
        issue.updated_at = now;
        issue.events.push(json!({
            "event": "labeled",
            "actor": { "login": actor },
            "label": { "name": label },
            "created_at": timestamp(now)
        }));
    }

    fn set_state(
        &mut self,
        repository: &str,
        number: i64,
        state: &'static str,
        reason: Option<String>,
        actor: &str,
    ) {
        let now = self.tick();
        let issue = self
            .issues
            .get_mut(&(repository.to_string(), number))
            .unwrap();
        issue.state = state;
        issue.state_reason = reason;
        issue.updated_at = now;
        let event = if state == "closed" {
            "closed"
        } else {
            "reopened"
        };
        issue.events.push(json!({
            "event": event,
            "actor": { "login": actor },
            "label": null,
            "created_at": timestamp(now)
        }));
    }

    // Gives `false` when the issue does not have the label.
    fn unlabel(&mut self, repository: &str, number: i64, label: &str, actor: &str) -> bool {
        let now = self.tick();
        let issue = self
            .issues
            .get_mut(&(repository.to_string(), number))
            .unwrap();
        if !issue.labels.iter().any(|name| name == label) {
            return false;
        }
        issue.labels.retain(|name| name != label);
        issue.updated_at = now;
        issue.events.push(json!({
            "event": "unlabeled",
            "actor": { "login": actor },
            "label": { "name": label },
            "created_at": timestamp(now)
        }));
        true
    }

    fn issue_json(&self, repository: &str, number: i64) -> Value {
        let issue = &self.issues[&(repository.to_string(), number)];
        let open_blockers = issue
            .blocked_by
            .iter()
            .filter(|blocker| self.issues[&(repository.to_string(), **blocker)].state == "open")
            .count();
        let mut json = json!({
            "id": number + ISSUE_ID_OFFSET,
            "number": number,
            "title": issue.title,
            "body": issue.body,
            "user": { "login": issue.author },
            "html_url": format!("https://github.com/{repository}/issues/{number}"),
            "repository_url": format!("https://api.github.com/repos/{repository}"),
            "state": issue.state,
            "updated_at": timestamp(issue.updated_at),
            "labels": issue.labels.iter().map(|name| json!({ "name": name })).collect::<Vec<_>>()
        });
        // GitHub gives no `issue_dependencies_summary` for a pull request.
        if issue.pull_request {
            json["pull_request"] = json!({
                "url": format!("https://api.github.com/repos/{repository}/pulls/{number}")
            });
        } else {
            json["issue_dependencies_summary"] = json!({
                "blocked_by": open_blockers,
                "total_blocked_by": issue.blocked_by.len()
            });
        }
        json
    }
}

fn timestamp(seconds: i64) -> String {
    (OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(seconds))
        .format(&Rfc3339)
        .unwrap()
}

type Shared = Arc<Mutex<Records>>;

pub struct FakeGitHub {
    pub url: String,
    state: Shared,
    remotes: TempDir,
}

impl FakeGitHub {
    pub async fn start() -> FakeGitHub {
        let remotes = TempDir::new().unwrap();
        let state = Shared::new(Mutex::new(Records {
            clock: OffsetDateTime::now_utc().unix_timestamp(),
            remotes: remotes.path().to_path_buf(),
            ..Records::default()
        }));
        let router = Router::new()
            .route("/users/{name}", get(account))
            .route("/app-manifests/{code}/conversions", post(convert_manifest))
            .route("/login/oauth/access_token", post(exchange_code))
            .route("/user", get(user))
            .route("/rate_limit", get(rate_limit))
            .route("/app", get(app))
            .route("/app/installations", get(installations))
            .route(
                "/app/installations/{id}/access_tokens",
                post(installation_token),
            )
            .route("/installation/repositories", get(installation_repositories))
            .route(
                "/repos/{owner}/{repo}/issues",
                get(issues).post(create_issue),
            )
            .route(
                "/repos/{owner}/{repo}/labels",
                get(repository_labels).post(create_label),
            )
            .route("/repos/{owner}/{repo}/labels/{name}", patch(update_label))
            .route(
                "/repos/{owner}/{repo}/issues/{number}",
                get(issue).patch(update_issue),
            )
            .route(
                "/repos/{owner}/{repo}/issues/{number}/sub_issues",
                get(sub_issues).post(add_sub_issue),
            )
            .route(
                "/repos/{owner}/{repo}/issues/{number}/dependencies/blocked_by",
                get(blocked_by).post(add_blocked_by),
            )
            .route("/repos/{owner}/{repo}/issues/{number}/parent", get(parent))
            .route(
                "/repos/{owner}/{repo}/issues/{number}/events",
                get(issue_events),
            )
            .route(
                "/repos/{owner}/{repo}/issues/{number}/comments",
                get(issue_comments).post(add_issue_comment),
            )
            .route(
                "/repos/{owner}/{repo}/issues/comments/{id}",
                patch(update_issue_comment),
            )
            .route(
                "/repos/{owner}/{repo}/issues/{number}/labels",
                post(add_labels),
            )
            .route(
                "/repos/{owner}/{repo}/issues/{number}/labels/{name}",
                delete(remove_label),
            )
            .route("/repos/{owner}/{repo}/releases/latest", get(latest_release))
            .route("/repos/{owner}/{repo}/compare/{range}", get(compare))
            .route("/repos/{owner}/{repo}/pulls", post(create_pull_request))
            .route(
                "/repos/{owner}/{repo}/pulls/{number}",
                get(pull_request).patch(update_issue),
            )
            .route("/repos/{owner}/{repo}/check-runs", post(create_check_run))
            .route(
                "/repos/{owner}/{repo}/check-runs/{id}",
                patch(update_check_run),
            )
            .route(
                "/repos/{owner}/{repo}/check-runs/{id}/annotations",
                get(annotations),
            )
            .route(
                "/repos/{owner}/{repo}/commits/{sha}/check-runs",
                get(commit_check_runs),
            )
            .route(
                "/repos/{owner}/{repo}/actions/jobs/{id}/logs",
                get(job_log_redirect),
            )
            .route("/job-logs/{id}", get(job_log))
            .route(
                "/repos/{owner}/{repo}/pulls/{number}/reviews",
                get(reviews).post(submit_review),
            )
            .route("/graphql", post(graphql))
            .route(
                "/repos/{owner}/{repo}/pulls/{number}/comments",
                get(review_comments),
            )
            .route(
                "/repos/{owner}/{repo}/pulls/{number}/comments/{id}/replies",
                post(reply_to_review_comment),
            )
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        FakeGitHub {
            url,
            state,
            remotes,
        }
    }

    pub fn fail_close(&self, repository: &str, number: i64) {
        self.state
            .lock()
            .unwrap()
            .failed_closes
            .insert((repository.to_string(), number));
    }

    pub fn fail_sub_issues(&self, repository: &str, number: i64) {
        self.state
            .lock()
            .unwrap()
            .failed_sub_issues
            .insert((repository.to_string(), number));
    }

    pub fn exhaust_rate_limit(&self, reset: i64) {
        self.state.lock().unwrap().rate_limit_reset = Some(reset);
    }

    pub fn fail_pull_request_creation(&self, repository: &str, status: u16, message: &str) {
        self.state.lock().unwrap().failed_pull_requests.insert(
            repository.to_string(),
            (StatusCode::from_u16(status).unwrap(), message.to_string()),
        );
    }

    pub fn add_account(&self, login: &str, account_type: &'static str) {
        self.state
            .lock()
            .unwrap()
            .account_types
            .insert(login.to_string(), account_type);
    }

    pub fn add_manifest_code(&self, code: &str) {
        self.state
            .lock()
            .unwrap()
            .manifest_codes
            .insert(code.to_string());
    }

    pub fn add_user_code(&self, code: &str, login: &str) {
        self.state
            .lock()
            .unwrap()
            .user_codes
            .insert(code.to_string(), (login.to_string(), 0));
    }

    pub fn add_second_app_user_code(&self, code: &str, login: &str) {
        self.state
            .lock()
            .unwrap()
            .user_codes
            .insert(code.to_string(), (login.to_string(), 1));
    }

    pub fn install_second_app(&self, account: &str) {
        self.state
            .lock()
            .unwrap()
            .second_app_accounts
            .insert(account.to_string());
    }

    pub fn set_app_permissions(&self, app_id: i64, permissions: &[(&str, &str)]) {
        self.state
            .lock()
            .unwrap()
            .app_permissions
            .insert(app_id, owned(permissions));
    }

    pub fn set_installation_permissions(&self, app_id: i64, permissions: &[(&str, &str)]) {
        self.state
            .lock()
            .unwrap()
            .installation_permissions
            .insert(app_id, owned(permissions));
    }

    pub fn fail_installations(&self, app_id: i64) {
        self.state.lock().unwrap().failed_apps.insert(app_id);
    }

    // The repository gets a bare git repository with one commit on `main` as its `clone_url`.
    pub fn add_repository(&self, full_name: &str) {
        let remote = self.remote(full_name);
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, &["init", "--bare", "--initial-branch=main"]);
        let work = TempDir::new().unwrap();
        git(work.path(), &["init", "--initial-branch=main"]);
        git(
            work.path(),
            &["commit", "--allow-empty", "-m", "Initial commit"],
        );
        git(
            work.path(),
            &["push", remote.to_str().unwrap(), "main:refs/heads/main"],
        );
        self.state
            .lock()
            .unwrap()
            .repositories
            .push(full_name.to_string());
    }

    // The author is `owner`.
    pub fn add_issue(&self, repository: &str, number: i64, title: &str) {
        self.insert_issue(repository, number, title, false);
    }

    pub fn add_pull_request(&self, repository: &str, number: i64, title: &str) {
        self.insert_issue(repository, number, title, true);
    }

    fn insert_issue(&self, repository: &str, number: i64, title: &str, pull_request: bool) {
        self.state.lock().unwrap().insert_issue(
            repository,
            number,
            title,
            "",
            "owner",
            pull_request,
        );
    }

    pub fn sub_issue_numbers(&self, repository: &str, number: i64) -> Vec<i64> {
        self.state.lock().unwrap().issues[&(repository.to_string(), number)]
            .sub_issues
            .iter()
            .map(|(_, number)| *number)
            .collect()
    }

    pub fn blocker_numbers(&self, repository: &str, number: i64) -> Vec<i64> {
        self.state.lock().unwrap().issues[&(repository.to_string(), number)]
            .blocked_by
            .clone()
    }

    pub fn issue(&self, repository: &str, number: i64) -> (String, String) {
        let records = self.state.lock().unwrap();
        let issue = &records.issues[&(repository.to_string(), number)];
        (issue.title.clone(), issue.body.clone())
    }

    pub fn remote(&self, full_name: &str) -> PathBuf {
        self.remotes.path().join(format!("{full_name}.git"))
    }

    // Pushes one new commit on top of `main` to `branch`, as a human push.
    pub fn push_commit(&self, full_name: &str, branch: &str, message: &str) {
        let remote = self.remote(full_name);
        let work = TempDir::new().unwrap();
        git(
            work.path(),
            &["clone", "--branch=main", remote.to_str().unwrap(), "."],
        );
        git(work.path(), &["commit", "--allow-empty", "-m", message]);
        git(
            work.path(),
            &["push", "origin", &format!("HEAD:refs/heads/{branch}")],
        );
    }

    pub fn commit_file(&self, full_name: &str, path: &str, content: &str, message: &str) {
        let remote = self.remote(full_name);
        let work = TempDir::new().unwrap();
        git(
            work.path(),
            &["clone", "--branch=main", remote.to_str().unwrap(), "."],
        );
        let file = work.path().join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, content).unwrap();
        git(work.path(), &["add", path]);
        git(work.path(), &["commit", "-m", message]);
        git(work.path(), &["push", "origin", "HEAD:refs/heads/main"]);
    }

    // Sets the creation time of a pull request to `seconds` after the Unix epoch.
    pub fn set_created_at(&self, full_name: &str, number: i64, seconds: i64) {
        self.state
            .lock()
            .unwrap()
            .pull_request_created_at
            .insert((full_name.to_string(), number), seconds);
    }

    // The latest release has `tag`, and each comparison has the commits with `messages`, the oldest first.
    pub fn set_release(&self, tag: &str, messages: &[&str]) {
        let mut records = self.state.lock().unwrap();
        records.latest_release = Some(tag.to_string());
        records.compared_commit_messages =
            messages.iter().map(|message| message.to_string()).collect();
    }

    // The pull request gives `mergeable_state` `behind` while its base is not an ancestor of its head.
    pub fn set_behind(&self, full_name: &str, number: i64) {
        self.state
            .lock()
            .unwrap()
            .behind_pull_requests
            .insert((full_name.to_string(), number));
    }

    // Commits `script` as an executable `.mobius/check` on `main`.
    pub fn set_check(&self, full_name: &str, script: &str) {
        let remote = self.remote(full_name);
        let work = TempDir::new().unwrap();
        git(
            work.path(),
            &["clone", "--branch=main", remote.to_str().unwrap(), "."],
        );
        let check = work.path().join(".mobius/check");
        fs::create_dir_all(check.parent().unwrap()).unwrap();
        fs::write(&check, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&check, fs::Permissions::from_mode(0o755)).unwrap();
        git(work.path(), &["add", ".mobius/check"]);
        git(work.path(), &["commit", "-m", "Add the local check"]);
        git(work.path(), &["push", "origin", "HEAD:refs/heads/main"]);
    }

    // Opens a pull request from `head` into `main`, as the App bot, and gives its number.
    pub fn open_pull_request(&self, repository: &str, title: &str, head: &str) -> i64 {
        self.state.lock().unwrap().insert_pull_request(
            repository,
            NewPullRequest {
                title: title.to_string(),
                head: head.to_string(),
                base: "main".to_string(),
                body: String::new(),
                draft: false,
            },
        )
    }

    pub fn pull_requests(&self, full_name: &str) -> Vec<PullRequest> {
        self.state
            .lock()
            .unwrap()
            .pull_requests
            .iter()
            .filter(|(name, _)| name == full_name)
            .map(|(_, pull_request)| pull_request.clone())
            .collect()
    }

    pub fn check_runs(&self, full_name: &str) -> Vec<CheckRun> {
        self.state
            .lock()
            .unwrap()
            .check_runs
            .iter()
            .filter(|(name, _)| name == full_name)
            .map(|(_, check_run)| check_run.clone())
            .collect()
    }

    // Gives the id of the check run.
    pub fn add_check_run(&self, full_name: &str, check_run: CheckRun) -> i64 {
        let mut records = self.state.lock().unwrap();
        records.check_runs.push((full_name.to_string(), check_run));
        records.check_runs.len() as i64
    }

    pub fn add_annotation(&self, id: i64, path: &str, line: i64, message: &str) {
        self.state
            .lock()
            .unwrap()
            .annotations
            .entry(id as usize)
            .or_default()
            .push(json!({ "path": path, "start_line": line, "message": message }));
    }

    pub fn set_check_run_app(&self, id: i64, slug: &str) {
        self.state
            .lock()
            .unwrap()
            .check_run_apps
            .insert(id as usize, slug.to_string());
    }

    pub fn add_job_log(&self, id: i64, log: &str) {
        self.state
            .lock()
            .unwrap()
            .job_logs
            .insert(id as usize, log.to_string());
    }

    pub fn submitted_reviews(&self, full_name: &str, number: i64) -> Vec<SubmittedReview> {
        self.state
            .lock()
            .unwrap()
            .submitted_reviews
            .iter()
            .filter(|(name, pull_request, _)| name == full_name && *pull_request == number)
            .map(|(_, _, review)| review.clone())
            .collect()
    }

    pub fn set_author(&self, repository: &str, number: i64, author: &str) {
        self.state
            .lock()
            .unwrap()
            .issues
            .get_mut(&(repository.to_string(), number))
            .unwrap()
            .author = author.to_string();
    }

    pub fn hold_issue_events(&self, repository: &str, number: i64) -> EventsHold {
        let hold = EventsHold {
            reached: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        };
        self.state.lock().unwrap().events_gate = Some(EventsGate {
            repository: repository.to_string(),
            number,
            reached: hold.reached.clone(),
            release: hold.release.clone(),
        });
        hold
    }

    pub fn add_comment(&self, repository: &str, number: i64, author: &str, body: &str) -> i64 {
        self.state
            .lock()
            .unwrap()
            .comment(repository, number, author, body, None)["id"]
            .as_i64()
            .unwrap()
    }

    // A comment that `author` posts through the Mobius App, as the `gh` of the Lead chat session does.
    pub fn add_app_comment(&self, repository: &str, number: i64, author: &str, body: &str) {
        self.state
            .lock()
            .unwrap()
            .comment(repository, number, author, body, Some(APP_SLUG));
    }

    // Gives the author and the body of each comment.
    pub fn comments(&self, repository: &str, number: i64) -> Vec<(String, String)> {
        self.state.lock().unwrap().issues[&(repository.to_string(), number)]
            .comments
            .iter()
            .map(|comment| {
                (
                    comment["user"]["login"].as_str().unwrap().to_string(),
                    comment["body"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    pub fn add_repository_label(
        &self,
        repository: &str,
        name: &str,
        color: &str,
        description: &str,
    ) {
        self.state.lock().unwrap().repository_labels.insert(
            (repository.to_string(), name.to_string()),
            RepositoryLabel {
                name: name.to_string(),
                color: color.to_string(),
                description: description.to_string(),
            },
        );
    }

    pub fn delete_repository_label(&self, repository: &str, name: &str) {
        self.state
            .lock()
            .unwrap()
            .repository_labels
            .remove(&(repository.to_string(), name.to_string()));
    }

    // Gives the labels of the repository, in name order.
    pub fn repository_labels(&self, repository: &str) -> Vec<RepositoryLabel> {
        self.state
            .lock()
            .unwrap()
            .repository_labels
            .iter()
            .filter(|((name, _), _)| *name == repository)
            .map(|(_, label)| label.clone())
            .collect()
    }

    // The name of each label of the repository that got a PATCH request, in request order.
    pub fn label_patches(&self, repository: &str) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .label_patches
            .iter()
            .filter(|(name, _)| *name == repository)
            .map(|(_, name)| name.clone())
            .collect()
    }

    pub fn labels(&self, repository: &str, number: i64) -> Vec<String> {
        self.state.lock().unwrap().issues[&(repository.to_string(), number)]
            .labels
            .clone()
    }

    // Gives the actor of the last `labeled` or `unlabeled` event of `label`.
    pub fn label_actor(&self, repository: &str, number: i64, label: &str) -> Option<String> {
        self.state.lock().unwrap().issues[&(repository.to_string(), number)]
            .events
            .iter()
            .rev()
            .find(|event| event["label"]["name"] == label)
            .map(|event| event["actor"]["login"].as_str().unwrap().to_string())
    }

    pub fn add_review(&self, repository: &str, number: i64, author: &str, state: &str, body: &str) {
        let mut records = self.state.lock().unwrap();
        let now = records.tick();
        records
            .issues
            .get_mut(&(repository.to_string(), number))
            .unwrap()
            .reviews
            .push(json!({
                "user": { "login": author },
                "body": body,
                "state": state,
                "submitted_at": timestamp(now)
            }));
    }

    // Gives the id of the new review comment. A reply has the id of the first comment of its thread in `in_reply_to`.
    pub fn add_review_comment(
        &self,
        repository: &str,
        number: i64,
        in_reply_to: Option<i64>,
        author: &str,
        body: &str,
    ) -> i64 {
        self.state.lock().unwrap().review_comment(
            repository,
            number,
            in_reply_to,
            author,
            &InlineComment {
                path: "src/plan.rs".to_string(),
                line: 12,
                body: body.to_string(),
            },
        )
    }

    // Marks the review thread that starts with the comment `root` as unresolved.
    pub fn unresolve_review_thread(&self, root: i64) {
        self.state.lock().unwrap().resolved_threads.remove(&root);
    }

    // Gives the review thread that starts with the comment `root`.
    pub fn review_thread(&self, repository: &str, number: i64, root: i64) -> Thread {
        let records = self.state.lock().unwrap();
        let comments = records.issues[&(repository.to_string(), number)]
            .review_comments
            .iter()
            .filter(|comment| comment["id"] == root || comment["in_reply_to_id"] == root)
            .map(|comment| {
                (
                    comment["user"]["login"].as_str().unwrap().to_string(),
                    comment["body"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        Thread {
            resolved: records.resolved_threads.contains(&root),
            comments,
        }
    }

    pub fn set_body(&self, repository: &str, number: i64, body: &str) {
        let mut records = self.state.lock().unwrap();
        let now = records.tick();
        let issue = records
            .issues
            .get_mut(&(repository.to_string(), number))
            .unwrap();
        issue.body = body.to_string();
        issue.updated_at = now;
    }

    pub fn set_title(&self, repository: &str, number: i64, title: &str) {
        let mut records = self.state.lock().unwrap();
        let now = records.tick();
        let issue = records
            .issues
            .get_mut(&(repository.to_string(), number))
            .unwrap();
        issue.title = title.to_string();
        issue.updated_at = now;
    }

    pub fn close_issue(&self, repository: &str, number: i64) {
        self.state
            .lock()
            .unwrap()
            .set_state(repository, number, "closed", None, "owner");
    }

    pub fn reopen_issue(&self, repository: &str, number: i64) {
        self.state
            .lock()
            .unwrap()
            .set_state(repository, number, "open", None, "owner");
    }

    // Gives the state and the state reason.
    pub fn state(&self, repository: &str, number: i64) -> (String, Option<String>) {
        let records = self.state.lock().unwrap();
        let issue = &records.issues[&(repository.to_string(), number)];
        (issue.state.to_string(), issue.state_reason.clone())
    }

    // The branch stays.
    pub fn merge_pull_request(&self, repository: &str, number: i64) {
        let mut records = self.state.lock().unwrap();
        let now = records.tick();
        let issue = records
            .issues
            .get_mut(&(repository.to_string(), number))
            .unwrap();
        issue.state = "closed";
        issue.merged_at = Some(now);
        issue.updated_at = now;
    }

    pub fn remove_label(&self, repository: &str, number: i64, label: &str, actor: &str) {
        assert!(
            self.state
                .lock()
                .unwrap()
                .unlabel(repository, number, label, actor)
        );
    }

    pub fn add_sub_issue(&self, repository: &str, parent: i64, child: i64) {
        self.state
            .lock()
            .unwrap()
            .issues
            .get_mut(&(repository.to_string(), parent))
            .unwrap()
            .sub_issues
            .push((repository.to_string(), child));
    }

    // Creates the issue and the link in one step, so that a poll sees both.
    pub fn add_sub_issue_of(&self, repository: &str, parent: i64, number: i64, title: &str) {
        let mut records = self.state.lock().unwrap();
        records.insert_issue(repository, number, title, "", "owner", false);
        records
            .issues
            .get_mut(&(repository.to_string(), parent))
            .unwrap()
            .sub_issues
            .push((repository.to_string(), number));
    }

    // Links `child` of `child_repository` as a sub-issue of `parent` in `repository`,
    // the way GitHub links a sub-issue that lives in another repository.
    pub fn add_foreign_sub_issue(
        &self,
        repository: &str,
        parent: i64,
        child_repository: &str,
        child: i64,
    ) {
        self.state
            .lock()
            .unwrap()
            .issues
            .get_mut(&(repository.to_string(), parent))
            .unwrap()
            .sub_issues
            .push((child_repository.to_string(), child));
    }

    pub fn add_label(&self, repository: &str, number: i64, label: &str, actor: &str) {
        self.state
            .lock()
            .unwrap()
            .label(repository, number, label, actor);
    }

    pub fn add_blocker(&self, repository: &str, number: i64, blocker: i64) {
        let mut records = self.state.lock().unwrap();
        let now = records.tick();
        let issue = records
            .issues
            .get_mut(&(repository.to_string(), number))
            .unwrap();
        issue.blocked_by.push(blocker);
        issue.updated_at = now;
    }

    pub fn not_modified_count(&self) -> u32 {
        self.state.lock().unwrap().not_modified
    }
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "message": "Not Found" })),
    )
        .into_response()
}

async fn rate_limit(State(state): State<Shared>) -> Response {
    let (remaining, reset) = match state.lock().unwrap().rate_limit_reset {
        Some(reset) => (0, reset),
        None => (5000, 0),
    };
    let rate =
        json!({ "limit": 5000, "used": 5000 - remaining, "remaining": remaining, "reset": reset });
    Json(json!({ "resources": { "core": rate, "search": rate }, "rate": rate })).into_response()
}

async fn latest_release(State(state): State<Shared>) -> Response {
    match &state.lock().unwrap().latest_release {
        Some(tag) => Json(json!({ "tag_name": tag, "assets": [] })).into_response(),
        None => not_found(),
    }
}

async fn compare(State(state): State<Shared>) -> Response {
    let commits: Vec<Value> = state
        .lock()
        .unwrap()
        .compared_commit_messages
        .iter()
        .map(|message| json!({ "commit": { "message": message } }))
        .collect();
    Json(json!({ "commits": commits })).into_response()
}

async fn account(State(state): State<Shared>, Path(name): Path<String>) -> Response {
    if APPS.iter().any(|app| name == format!("{}[bot]", app.slug)) {
        return Json(json!({ "login": name, "id": BOT_USER_ID, "type": "Bot" })).into_response();
    }
    match state.lock().unwrap().account_types.get(&name) {
        Some(account_type) => {
            Json(json!({ "login": name, "id": 1, "type": account_type })).into_response()
        }
        None => not_found(),
    }
}

async fn convert_manifest(State(state): State<Shared>, Path(code): Path<String>) -> Response {
    let mut records = state.lock().unwrap();
    if !records.manifest_codes.remove(&code) {
        return not_found();
    }
    let app = &APPS[records.apps_created];
    records.apps_created += 1;
    (
        StatusCode::CREATED,
        Json(json!({
            "id": app.id,
            "slug": app.slug,
            "pem": APP_PRIVATE_KEY,
            "client_id": app.client_id,
            "client_secret": app.client_secret
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct CodeExchange {
    client_id: String,
    client_secret: String,
    code: Option<String>,
    grant_type: Option<String>,
    refresh_token: Option<String>,
}

async fn exchange_code(
    State(state): State<Shared>,
    Json(exchange): Json<CodeExchange>,
) -> Response {
    let mut records = state.lock().unwrap();
    let Some(app) = APPS.iter().position(|app| {
        exchange.client_id == app.client_id && exchange.client_secret == app.client_secret
    }) else {
        return Json(json!({
            "error": "incorrect_client_credentials",
            "error_description": "The client_id and/or client_secret passed are incorrect."
        }))
        .into_response();
    };
    let login = if exchange.grant_type.as_deref() == Some("refresh_token") {
        let Some(login) = exchange
            .refresh_token
            .filter(|token| {
                records
                    .refresh_tokens
                    .get(token)
                    .is_some_and(|(_, of)| *of == app)
            })
            .and_then(|token| records.refresh_tokens.remove(&token))
            .map(|(login, _)| login)
        else {
            return Json(json!({
                "error": "bad_refresh_token",
                "error_description": "The refresh token passed is incorrect or expired."
            }))
            .into_response();
        };
        login
    } else {
        let Some(login) = exchange
            .code
            .filter(|code| {
                records
                    .user_codes
                    .get(code)
                    .is_some_and(|(_, of)| *of == app)
            })
            .and_then(|code| records.user_codes.remove(&code))
            .map(|(login, _)| login)
        else {
            return Json(json!({
                "error": "bad_verification_code",
                "error_description": "The code passed is incorrect or expired."
            }))
            .into_response();
        };
        login
    };
    records.tokens_given += 1;
    let number = records.tokens_given;
    records
        .user_tokens
        .insert(format!("ghu_{number}"), login.clone());
    records
        .refresh_tokens
        .insert(format!("ghr_{number}"), (login, app));
    Json(json!({
        "access_token": format!("ghu_{number}"),
        "expires_in": 28800,
        "refresh_token": format!("ghr_{number}"),
        "refresh_token_expires_in": 15638400,
        "scope": "",
        "token_type": "bearer"
    }))
    .into_response()
}

async fn user(State(state): State<Shared>, headers: HeaderMap) -> Response {
    match state.lock().unwrap().user_tokens.get(bearer(&headers)) {
        Some(login) => Json(json!({ "login": login })).into_response(),
        None => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "message": "Bad credentials" })),
        )
            .into_response(),
    }
}

fn bearer(headers: &HeaderMap) -> &str {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default()
}

// The App signs a JSON Web Token, and the claim `iss` of the token is the App id.
fn signed_app(headers: &HeaderMap) -> usize {
    let claims = bearer(headers).split('.').nth(1).unwrap_or_default();
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).unwrap_or_default())
        .unwrap_or_default();
    let id = claims["iss"]
        .as_i64()
        .or_else(|| claims["iss"].as_str()?.parse().ok());
    APPS.iter().position(|app| Some(app.id) == id).unwrap()
}

fn owned(permissions: &[(&str, &str)]) -> HashMap<String, String> {
    permissions
        .iter()
        .map(|(name, level)| (name.to_string(), level.to_string()))
        .collect()
}

async fn app(State(state): State<Shared>, headers: HeaderMap) -> Response {
    let app = &APPS[signed_app(&headers)];
    let records = state.lock().unwrap();
    Json(json!({
        "id": app.id,
        "slug": app.slug,
        "permissions": Records::permissions(&records.app_permissions, app.id)
    }))
    .into_response()
}

async fn installations(State(state): State<Shared>, headers: HeaderMap) -> Response {
    let app = signed_app(&headers);
    let records = state.lock().unwrap();
    if records.failed_apps.contains(&APPS[app].id) {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if !records
        .repositories
        .iter()
        .any(|repository| records.app_index(repository) == app)
    {
        return Json(json!([])).into_response();
    }
    // The one installation of an App has the account of its first repository.
    let account = records
        .repositories
        .iter()
        .find(|repository| records.app_index(repository) == app)
        .and_then(|repository| repository.split('/').next())
        .unwrap_or_default();
    Json(json!([{
        "id": app + 1,
        "account": {
            "login": account,
            "type": records.account_types.get(account).copied().unwrap_or("User")
        },
        "permissions": Records::permissions(&records.installation_permissions, APPS[app].id)
    }]))
    .into_response()
}

async fn installation_token(Path(id): Path<usize>) -> Response {
    (
        StatusCode::CREATED,
        Json(json!({
            "token": APPS[id - 1].installation_token,
            "expires_at": "2099-01-01T00:00:00Z",
            "permissions": {}
        })),
    )
        .into_response()
}

async fn installation_repositories(State(state): State<Shared>, headers: HeaderMap) -> Response {
    let app = APPS
        .iter()
        .position(|app| app.installation_token == bearer(&headers))
        .unwrap();
    let records = state.lock().unwrap();
    let repositories: Vec<&String> = records
        .repositories
        .iter()
        .filter(|repository| records.app_index(repository) == app)
        .collect();
    Json(json!({
        "total_count": repositories.len(),
        "repositories": repositories
            .iter()
            .map(|full_name| json!({
                "full_name": full_name,
                "clone_url": format!("file://{}/{full_name}.git", records.remotes.display()),
                "default_branch": "main"
            }))
            .collect::<Vec<_>>()
    }))
    .into_response()
}

#[derive(Deserialize)]
struct Page {
    page: Option<usize>,
    per_page: Option<usize>,
}

impl Page {
    fn of(&self, items: Vec<Value>) -> Vec<Value> {
        let size = self.per_page.unwrap_or(30);
        let skip = (self.page.unwrap_or(1) - 1) * size;
        items.into_iter().skip(skip).take(size).collect()
    }
}

#[derive(Deserialize)]
struct IssueFilter {
    state: Option<String>,
    labels: Option<String>,
    since: Option<String>,
}

async fn issues(
    State(state): State<Shared>,
    Path((owner, repo)): Path<(String, String)>,
    Query(filter): Query<IssueFilter>,
    Query(page): Query<Page>,
    headers: HeaderMap,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let since = filter.since.map(|since| {
        OffsetDateTime::parse(&since, &Rfc3339)
            .unwrap()
            .unix_timestamp()
    });
    let mut records = state.lock().unwrap();
    let mut found: Vec<_> = records
        .issues
        .iter()
        .filter(|((name, _), issue)| {
            *name == repository
                && filter
                    .state
                    .as_ref()
                    .is_none_or(|state| state != "open" || issue.state == "open")
                && filter
                    .labels
                    .as_ref()
                    .is_none_or(|label| issue.labels.contains(label))
                && since.is_none_or(|since| issue.updated_at >= since)
        })
        .collect();
    found.sort_by_key(|((_, number), issue)| (issue.updated_at, *number));
    let numbers: Vec<i64> = found.into_iter().map(|((_, number), _)| *number).collect();
    let body = Value::Array(
        page.of(numbers
            .into_iter()
            .map(|number| records.issue_json(&repository, number))
            .collect()),
    );
    let mut hasher = DefaultHasher::new();
    body.to_string().hash(&mut hasher);
    let etag = format!("\"{:x}\"", hasher.finish());
    if headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|value| value.as_bytes() == etag.as_bytes())
    {
        records.not_modified += 1;
        return StatusCode::NOT_MODIFIED.into_response();
    }
    ([(header::ETAG, etag)], Json(body)).into_response()
}

async fn issue(
    State(state): State<Shared>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let records = state.lock().unwrap();
    if !records.issues.contains_key(&(repository.clone(), number)) {
        return not_found();
    }
    Json(records.issue_json(&repository, number)).into_response()
}

async fn parent(
    State(state): State<Shared>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let records = state.lock().unwrap();
    let parent = records.issues.iter().find(|((name, _), issue)| {
        *name == repository
            && issue.sub_issues.iter().any(|(child_repository, child)| {
                *child_repository == repository && *child == number
            })
    });
    match parent {
        Some(((_, parent), _)) => Json(records.issue_json(&repository, *parent)).into_response(),
        None => not_found(),
    }
}

#[derive(Deserialize)]
struct NewComment {
    body: String,
}

async fn add_issue_comment(
    State(state): State<Shared>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Json(comment): Json<NewComment>,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let mut records = state.lock().unwrap();
    let bot = records.app_login(&repository);
    let comment = records.comment(&repository, number, &bot, &comment.body, None);
    (StatusCode::CREATED, Json(comment)).into_response()
}

async fn update_issue_comment(
    State(state): State<Shared>,
    Path((owner, repo, id)): Path<(String, String, i64)>,
    Json(update): Json<NewComment>,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let mut records = state.lock().unwrap();
    let now = records.tick();
    let found = records
        .issues
        .iter_mut()
        .filter(|((name, _), _)| *name == repository)
        .find_map(|(_, issue)| {
            let comment = issue
                .comments
                .iter_mut()
                .find(|comment| comment["id"].as_i64() == Some(id))?;
            comment["body"] = json!(update.body);
            issue.updated_at = now;
            Some(comment.clone())
        });
    match found {
        Some(comment) => Json(comment).into_response(),
        None => not_found(),
    }
}

#[derive(Deserialize)]
struct NewLabels {
    labels: Vec<String>,
}

async fn add_labels(
    State(state): State<Shared>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    headers: HeaderMap,
    Json(new): Json<NewLabels>,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let mut records = state.lock().unwrap();
    let actor = records.actor(&repository, &headers);
    for label in &new.labels {
        records.label(&repository, number, label, &actor);
    }
    Json(label_list(&records, &repository, number)).into_response()
}

#[derive(Deserialize)]
struct NewPullRequest {
    title: String,
    head: String,
    base: String,
    body: String,
    draft: bool,
}

async fn create_pull_request(
    State(state): State<Shared>,
    Path((owner, repo)): Path<(String, String)>,
    Json(new): Json<NewPullRequest>,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let mut records = state.lock().unwrap();
    if let Some((status, message)) = records.failed_pull_requests.get(&repository) {
        return (*status, Json(json!({ "message": message }))).into_response();
    }
    let number = records.insert_pull_request(&repository, new);
    let mut json = pull_request_json(&records, &repository, number);
    json["mergeable"] = Value::Null;
    (StatusCode::CREATED, Json(json)).into_response()
}

// `mergeable` is `false` when `git merge-tree` of the base and the head in the remote finds a conflict.
fn pull_request_json(records: &Records, repository: &str, number: i64) -> Value {
    let (_, pull_request) = records
        .pull_requests
        .iter()
        .find(|(name, pull_request)| name == repository && pull_request.number == number)
        .unwrap();
    let git = |args: &[&str]| {
        Command::new("git")
            .current_dir(records.remotes.join(format!("{repository}.git")))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap()
    };
    let merge = git(&[
        "merge-tree",
        "--write-tree",
        &pull_request.base,
        &pull_request.head,
    ]);
    let head = git(&["rev-parse", &pull_request.head]);
    let behind = records
        .behind_pull_requests
        .contains(&(repository.to_string(), number))
        && !git(&[
            "merge-base",
            "--is-ancestor",
            &pull_request.base,
            &pull_request.head,
        ])
        .status
        .success();
    json!({
        "number": number,
        "node_id": format!("PR_{number}"),
        "html_url": format!("https://github.com/{repository}/pull/{number}"),
        "state": records.issues[&(repository.to_string(), number)].state,
        "merged": records.issues[&(repository.to_string(), number)].merged_at.is_some(),
        "head": { "sha": String::from_utf8(head.stdout).unwrap().trim() },
        "draft": pull_request.draft,
        "mergeable": merge.status.success(),
        "mergeable_state": if behind { "behind" } else { "clean" },
        "created_at": timestamp(records.pull_request_created_at[&(repository.to_string(), number)])
    })
}

async fn pull_request(
    State(state): State<Shared>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let records = state.lock().unwrap();
    if !records
        .pull_requests
        .iter()
        .any(|(name, pull_request)| *name == repository && pull_request.number == number)
    {
        return not_found();
    }
    Json(pull_request_json(&records, &repository, number)).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewCheckRun {
    name: String,
    head_sha: String,
    status: String,
    conclusion: Option<String>,
    output: Option<CheckRunOutput>,
}

async fn create_check_run(
    State(state): State<Shared>,
    Path((owner, repo)): Path<(String, String)>,
    Json(new): Json<NewCheckRun>,
) -> Response {
    let mut records = state.lock().unwrap();
    records.check_runs.push((
        format!("{owner}/{repo}"),
        CheckRun {
            name: new.name,
            head_sha: new.head_sha,
            status: new.status,
            conclusion: new.conclusion,
            output: new.output,
        },
    ));
    let id = records.check_runs.len();
    (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckRunUpdate {
    status: String,
    conclusion: String,
}

async fn update_check_run(
    State(state): State<Shared>,
    Path((owner, repo, id)): Path<(String, String, usize)>,
    Json(update): Json<CheckRunUpdate>,
) -> Response {
    let mut records = state.lock().unwrap();
    let Some((repository, check_run)) = records.check_runs.get_mut(id - 1) else {
        return not_found();
    };
    if *repository != format!("{owner}/{repo}") {
        return not_found();
    }
    check_run.status = update.status;
    check_run.conclusion = Some(update.conclusion);
    Json(json!({ "id": id })).into_response()
}

fn check_run_json(records: &Records, id: usize, repository: &str, check_run: &CheckRun) -> Value {
    json!({
        "app": records.check_run_apps.get(&id).map(|slug| json!({ "slug": slug })),
        "id": id,
        "name": check_run.name,
        "status": check_run.status,
        "conclusion": check_run.conclusion,
        "html_url": format!("https://github.com/{repository}/runs/{id}"),
        "output": {
            "title": check_run.output.as_ref().map(|output| &output.title),
            "summary": check_run.output.as_ref().map(|output| &output.summary)
        }
    })
}

async fn commit_check_runs(
    State(state): State<Shared>,
    Path((owner, repo, sha)): Path<(String, String, String)>,
    Query(page): Query<Page>,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let records = state.lock().unwrap();
    let check_runs: Vec<Value> = records
        .check_runs
        .iter()
        .enumerate()
        .filter(|(_, (name, check_run))| *name == repository && check_run.head_sha == sha)
        .map(|(index, (_, check_run))| check_run_json(&records, index + 1, &repository, check_run))
        .collect();
    Json(json!({ "total_count": check_runs.len(), "check_runs": page.of(check_runs) }))
        .into_response()
}

async fn annotations(
    State(state): State<Shared>,
    Path((_, _, id)): Path<(String, String, usize)>,
    Query(page): Query<Page>,
) -> Response {
    let records = state.lock().unwrap();
    let annotations = records.annotations.get(&id).cloned().unwrap_or_default();
    Json(page.of(annotations)).into_response()
}

async fn job_log_redirect(
    State(state): State<Shared>,
    Path((_, _, id)): Path<(String, String, usize)>,
    headers: HeaderMap,
) -> Response {
    if !state.lock().unwrap().job_logs.contains_key(&id) {
        return not_found();
    }
    let host = headers[header::HOST].to_str().unwrap();
    (
        StatusCode::FOUND,
        [(header::LOCATION, format!("http://{host}/job-logs/{id}"))],
    )
        .into_response()
}

async fn job_log(State(state): State<Shared>, Path(id): Path<usize>) -> Response {
    match state.lock().unwrap().job_logs.get(&id) {
        Some(log) => log.clone().into_response(),
        None => not_found(),
    }
}

async fn submit_review(
    State(state): State<Shared>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Json(review): Json<SubmittedReview>,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let mut records = state.lock().unwrap();
    let bot = records.app_login(&repository);
    let now = records.tick();
    records
        .issues
        .get_mut(&(repository.clone(), number))
        .unwrap()
        .reviews
        .push(json!({
            "user": { "login": bot },
            "body": review.body,
            "state": "COMMENTED",
            "submitted_at": timestamp(now)
        }));
    for comment in &review.comments {
        records.review_comment(&repository, number, None, &bot, comment);
    }
    records.submitted_reviews.push((repository, number, review));
    Json(json!({ "id": 1 })).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GraphQl {
    query: String,
    variables: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    body: String,
}

async fn reply_to_review_comment(
    State(state): State<Shared>,
    Path((owner, repo, number, id)): Path<(String, String, i64, i64)>,
    Json(reply): Json<Reply>,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let mut records = state.lock().unwrap();
    let bot = records.app_login(&repository);
    let Some(root) = records
        .issues
        .get(&(repository.clone(), number))
        .and_then(|issue| {
            issue
                .review_comments
                .iter()
                .find(|comment| comment["id"] == id && comment["in_reply_to_id"].is_null())
        })
    else {
        return not_found();
    };
    let comment = InlineComment {
        path: root["path"].as_str().unwrap().to_string(),
        line: root["line"].as_i64().unwrap(),
        body: reply.body,
    };
    let id = records.review_comment(&repository, number, Some(id), &bot, &comment);
    (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
}

// Answers the review threads query and the mutations `markPullRequestReadyForReview` and `resolveReviewThread`, each in one page. The node id of a thread is `RT_` and the id of its first comment.
async fn graphql(State(state): State<Shared>, Json(request): Json<GraphQl>) -> Response {
    let variables = &request.variables;
    let mut records = state.lock().unwrap();
    if request.query.contains("resolveReviewThread") {
        let Some(root) = variables["id"]
            .as_str()
            .and_then(|id| id.strip_prefix("RT_"))
            .and_then(|root| root.parse().ok())
        else {
            return Json(
                json!({ "data": null, "errors": [{ "message": "Could not resolve to a node" }] }),
            )
            .into_response();
        };
        records.resolved_threads.insert(root);
        return Json(json!({
            "data": { "resolveReviewThread": { "clientMutationId": null } }
        }))
        .into_response();
    }
    if request.query.contains("markPullRequestReadyForReview") {
        let Some((_, pull_request)) = records
            .pull_requests
            .iter_mut()
            .find(|(_, pull_request)| format!("PR_{}", pull_request.number) == variables["id"])
        else {
            return Json(
                json!({ "data": null, "errors": [{ "message": "Could not resolve to a node" }] }),
            )
            .into_response();
        };
        pull_request.draft = false;
        return Json(json!({
            "data": { "markPullRequestReadyForReview": { "clientMutationId": null } }
        }))
        .into_response();
    }
    let repository = format!(
        "{}/{}",
        variables["owner"].as_str().unwrap(),
        variables["name"].as_str().unwrap()
    );
    let number = variables["number"].as_i64().unwrap();
    let comments = &records.issues[&(repository, number)].review_comments;
    let threads: Vec<Value> = comments
        .iter()
        .filter(|root| root["in_reply_to_id"].is_null())
        .map(|root| {
            let authors: Vec<Value> = comments
                .iter()
                .filter(|comment| {
                    comment["id"] == root["id"] || comment["in_reply_to_id"] == root["id"]
                })
                .map(|comment| {
                    let login = comment["user"]["login"].as_str().unwrap();
                    let author = match login.strip_suffix("[bot]") {
                        Some(bot) => json!({ "__typename": "Bot", "login": bot }),
                        None => json!({ "__typename": "User", "login": login }),
                    };
                    json!({ "databaseId": comment["id"], "author": author })
                })
                .collect();
            let root = root["id"].as_i64().unwrap();
            json!({
                "id": format!("RT_{root}"),
                "isResolved": records.resolved_threads.contains(&root),
                "comments": { "nodes": authors }
            })
        })
        .collect();
    Json(json!({
        "data": {
            "repository": {
                "pullRequest": {
                    "reviewThreads": {
                        "nodes": threads,
                        "pageInfo": { "hasNextPage": false, "endCursor": null }
                    }
                }
            }
        }
    }))
    .into_response()
}

async fn remove_label(
    State(state): State<Shared>,
    Path((owner, repo, number, name)): Path<(String, String, i64, String)>,
    headers: HeaderMap,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let mut records = state.lock().unwrap();
    let actor = records.actor(&repository, &headers);
    if !records.unlabel(&repository, number, &name, &actor) {
        return not_found();
    }
    Json(label_list(&records, &repository, number)).into_response()
}

fn label_list(records: &Records, repository: &str, number: i64) -> Vec<Value> {
    records.issues[&(repository.to_string(), number)]
        .labels
        .iter()
        .map(|name| json!({ "name": name }))
        .collect()
}

fn repository_label_json(label: &RepositoryLabel) -> Value {
    json!({
        "name": label.name,
        "color": label.color,
        "description": label.description
    })
}

async fn repository_labels(
    State(state): State<Shared>,
    Path((owner, repo)): Path<(String, String)>,
    Query(page): Query<Page>,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let records = state.lock().unwrap();
    let labels: Vec<Value> = records
        .repository_labels
        .iter()
        .filter(|((name, _), _)| *name == repository)
        .map(|(_, label)| repository_label_json(label))
        .collect();
    Json(page.of(labels)).into_response()
}

async fn create_label(
    State(state): State<Shared>,
    Path((owner, repo)): Path<(String, String)>,
    Json(new): Json<RepositoryLabel>,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let mut records = state.lock().unwrap();
    // GitHub compares label names without regard to case.
    let exists = records
        .repository_labels
        .keys()
        .any(|(name_repository, name)| {
            *name_repository == repository && name.eq_ignore_ascii_case(&new.name)
        });
    if exists {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "message": "Validation Failed" })),
        )
            .into_response();
    }
    records
        .repository_labels
        .insert((repository, new.name.clone()), new.clone());
    (StatusCode::CREATED, Json(repository_label_json(&new))).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelUpdate {
    color: String,
}

async fn update_label(
    State(state): State<Shared>,
    Path((owner, repo, name)): Path<(String, String, String)>,
    Json(update): Json<LabelUpdate>,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let mut records = state.lock().unwrap();
    // GitHub finds a label by its name without regard to case.
    let key = records
        .repository_labels
        .keys()
        .find(|(name_repository, label_name)| {
            *name_repository == repository && label_name.eq_ignore_ascii_case(&name)
        })
        .cloned();
    let Some(label) = key.and_then(|key| records.repository_labels.get_mut(&key)) else {
        return not_found();
    };
    label.color = update.color;
    let json = repository_label_json(label);
    records.label_patches.push((repository, name));
    Json(json).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewIssue {
    title: String,
    body: String,
}

async fn create_issue(
    State(state): State<Shared>,
    Path((owner, repo)): Path<(String, String)>,
    Json(new): Json<NewIssue>,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let mut records = state.lock().unwrap();
    let bot = records.app_login(&repository);
    let number = records
        .issues
        .keys()
        .filter(|(name, _)| *name == repository)
        .map(|(_, number)| number + 1)
        .max()
        .unwrap_or(1);
    records.insert_issue(&repository, number, &new.title, &new.body, &bot, false);
    (
        StatusCode::CREATED,
        Json(records.issue_json(&repository, number)),
    )
        .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueUpdate {
    state: String,
    state_reason: Option<String>,
}

async fn update_issue(
    State(state): State<Shared>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Json(update): Json<IssueUpdate>,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let mut records = state.lock().unwrap();
    let bot = records.app_login(&repository);
    if update.state != "closed" || !records.issues.contains_key(&(repository.clone(), number)) {
        return not_found();
    }
    if records
        .failed_closes
        .contains(&(repository.clone(), number))
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    records.set_state(&repository, number, "closed", update.state_reason, &bot);
    Json(records.issue_json(&repository, number)).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewSubIssue {
    sub_issue_id: i64,
    replace_parent: bool,
}

async fn add_sub_issue(
    State(state): State<Shared>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Json(new): Json<NewSubIssue>,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let mut records = state.lock().unwrap();
    let child = new.sub_issue_id - ISSUE_ID_OFFSET;
    if !records.issues.contains_key(&(repository.clone(), child))
        || !records.issues.contains_key(&(repository.clone(), number))
    {
        return not_found();
    }
    let has_parent = records.issues.iter().any(|((name, _), issue)| {
        *name == repository
            && issue
                .sub_issues
                .iter()
                .any(|(child_repository, n)| *child_repository == repository && *n == child)
    });
    if has_parent && !new.replace_parent {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "message": "The issue already has a parent." })),
        )
            .into_response();
    }
    for ((name, _), issue) in records.issues.iter_mut() {
        if *name == repository {
            issue
                .sub_issues
                .retain(|(child_repository, n)| !(*child_repository == repository && *n == child));
        }
    }
    records
        .issues
        .get_mut(&(repository.clone(), number))
        .unwrap()
        .sub_issues
        .push((repository.clone(), child));
    (
        StatusCode::CREATED,
        Json(records.issue_json(&repository, number)),
    )
        .into_response()
}

async fn blocked_by(
    State(state): State<Shared>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Query(page): Query<Page>,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let records = state.lock().unwrap();
    let Some(issue) = records.issues.get(&(repository.clone(), number)) else {
        return not_found();
    };
    let blockers = issue
        .blocked_by
        .iter()
        .map(|blocker| records.issue_json(&repository, *blocker))
        .collect();
    Json(page.of(blockers)).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewBlocker {
    issue_id: i64,
}

async fn add_blocked_by(
    State(state): State<Shared>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Json(new): Json<NewBlocker>,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let mut records = state.lock().unwrap();
    let blocker = new.issue_id - ISSUE_ID_OFFSET;
    if !records.issues.contains_key(&(repository.clone(), blocker)) {
        return not_found();
    }
    let now = records.tick();
    let Some(issue) = records.issues.get_mut(&(repository.clone(), number)) else {
        return not_found();
    };
    issue.blocked_by.push(blocker);
    issue.updated_at = now;
    (
        StatusCode::CREATED,
        Json(records.issue_json(&repository, blocker)),
    )
        .into_response()
}

async fn sub_issues(
    State(state): State<Shared>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Query(page): Query<Page>,
) -> Response {
    let repository = format!("{owner}/{repo}");
    let records = state.lock().unwrap();
    let Some(parent) = records.issues.get(&(repository.clone(), number)) else {
        return not_found();
    };
    if records
        .failed_sub_issues
        .contains(&(repository.clone(), number))
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let children = parent
        .sub_issues
        .iter()
        .map(|(child_repository, child)| records.issue_json(child_repository, *child))
        .collect();
    Json(page.of(children)).into_response()
}

async fn issue_events(
    State(state): State<Shared>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Query(page): Query<Page>,
) -> Response {
    let gate = {
        let mut records = state.lock().unwrap();
        let matches = records.events_gate.as_ref().is_some_and(|gate| {
            gate.repository == format!("{owner}/{repo}") && gate.number == number
        });
        if matches {
            records.events_gate.take()
        } else {
            None
        }
    };
    if let Some(gate) = gate {
        gate.reached.notify_one();
        gate.release.notified().await;
    }
    issue_list(&state, owner, repo, number, &page, |issue| &issue.events)
}

fn issue_list(
    state: &Shared,
    owner: String,
    repo: String,
    number: i64,
    page: &Page,
    list: impl Fn(&Issue) -> &Vec<Value>,
) -> Response {
    match state
        .lock()
        .unwrap()
        .issues
        .get(&(format!("{owner}/{repo}"), number))
    {
        Some(issue) => Json(page.of(list(issue).clone())).into_response(),
        None => not_found(),
    }
}

async fn issue_comments(
    State(state): State<Shared>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Query(page): Query<Page>,
) -> Response {
    issue_list(&state, owner, repo, number, &page, |issue| &issue.comments)
}

async fn reviews(
    State(state): State<Shared>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Query(page): Query<Page>,
) -> Response {
    issue_list(&state, owner, repo, number, &page, |issue| &issue.reviews)
}

async fn review_comments(
    State(state): State<Shared>,
    Path((owner, repo, number)): Path<(String, String, i64)>,
    Query(page): Query<Page>,
) -> Response {
    issue_list(&state, owner, repo, number, &page, |issue| {
        &issue.review_comments
    })
}
