use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;

use http::StatusCode;
use http::header::{ACCEPT, ETAG, HeaderMap, HeaderValue, IF_NONE_MATCH};
use jsonwebtoken::EncodingKey;
use octocrab::Octocrab;
use octocrab::models::{AppId, InstallationId};
use secrecy::ExposeSecret;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const PAGE_SIZE: usize = 100;

#[derive(Clone)]
pub struct GitHub {
    api: Octocrab,
    web: Octocrab,
    api_url: String,
    web_url: String,
}

#[derive(Clone)]
pub struct Repository {
    pub full_name: String,
    pub clone_url: String,
    pub default_branch: String,
    pub app_id: i64,
    pub app_slug: String,
    client: Octocrab,
    token: String,
}

#[derive(Deserialize)]
struct Installation {
    id: u64,
    account: InstallationAccount,
    permissions: HashMap<String, String>,
}

#[derive(Deserialize)]
struct InstallationAccount {
    login: String,
    #[serde(rename = "type")]
    account_type: String,
}

#[derive(Deserialize)]
struct AppPermissions {
    permissions: HashMap<String, String>,
}

#[derive(Deserialize)]
struct InstallationRepositories {
    repositories: Vec<RepositoryName>,
}

#[derive(Deserialize)]
struct RepositoryName {
    full_name: String,
    clone_url: String,
    default_branch: String,
}

#[derive(Deserialize)]
pub struct Issue {
    // Sub-issues and dependencies take this id, not the number.
    pub id: i64,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub html_url: String,
    // After a transfer, the URL names the new repository, because the client follows the redirect.
    pub repository_url: String,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    pub labels: Vec<Label>,
    pub pull_request: Option<serde_json::Value>,
    pub user: User,
    // GitHub gives no summary for a pull request.
    #[serde(default)]
    pub issue_dependencies_summary: DependenciesSummary,
}

#[derive(Default, Deserialize)]
pub struct DependenciesSummary {
    // The open blockers only.
    pub blocked_by: i64,
}

impl Issue {
    pub fn has_label(&self, name: &str) -> bool {
        self.labels.iter().any(|label| label.name == name)
    }
}

#[derive(Deserialize)]
pub struct Label {
    pub name: String,
}

#[derive(Deserialize)]
pub struct RepositoryLabel {
    pub name: String,
    pub color: String,
}

#[derive(Deserialize)]
pub struct IssueEvent {
    pub event: String,
    pub actor: Option<User>,
    pub label: Option<Label>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Deserialize)]
pub struct Comment {
    pub id: i64,
    pub user: User,
    pub body: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub performed_via_github_app: Option<AppRef>,
}

#[derive(Deserialize)]
pub struct AppRef {
    pub slug: String,
}

#[derive(Deserialize)]
pub struct Review {
    pub user: User,
    pub body: String,
    pub state: String,
    // GitHub gives no time for a pending review.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub submitted_at: Option<OffsetDateTime>,
}

#[derive(Deserialize)]
pub struct ReviewComment {
    pub id: i64,
    pub user: User,
    pub body: String,
    pub path: String,
    pub line: Option<i64>,
    pub in_reply_to_id: Option<i64>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Clone, Deserialize)]
pub struct PullRequest {
    pub number: i64,
    pub node_id: String,
    pub html_url: String,
    pub state: String,
    pub merged: bool,
    pub head: Head,
    pub draft: bool,
    // GitHub gives `null` while it computes the value.
    pub mergeable: Option<bool>,
    // The single pull request endpoint gives the value. The list endpoint does not.
    pub mergeable_state: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Clone, Deserialize)]
pub struct Head {
    pub sha: String,
}

#[derive(Deserialize)]
struct Created {
    id: i64,
}

#[derive(Clone, Deserialize)]
pub struct CheckRun {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub html_url: Option<String>,
    pub output: CheckRunOutput,
    pub app: Option<CheckRunApp>,
}

#[derive(Clone, Deserialize)]
pub struct CheckRunApp {
    pub slug: String,
}

#[derive(Clone, Deserialize)]
pub struct CheckRunOutput {
    pub title: Option<String>,
    pub summary: Option<String>,
}

#[derive(Deserialize)]
struct CheckRuns {
    check_runs: Vec<CheckRun>,
}

#[derive(Deserialize)]
pub struct Annotation {
    pub path: String,
    pub start_line: i64,
    pub message: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NewReviewComment {
    pub path: String,
    pub line: i64,
    pub body: String,
}

pub struct ReviewThread {
    // The GraphQL node id.
    pub id: String,
    // The REST id of the first comment.
    pub comment: i64,
    pub resolved: bool,
    // The REST login of the author of each comment, in order.
    pub authors: Vec<String>,
}

// The links of an open issue to the other open issues of its repository.
#[derive(Clone, PartialEq)]
pub struct IssueLinks {
    pub parent: Option<i64>,
    // The number of sub-issues, open and closed, in all repositories.
    pub sub_issues: i64,
    pub blockers: BTreeSet<i64>,
}

pub struct IssuePage {
    pub issues: Vec<Issue>,
    pub etag: Option<String>,
}

#[derive(Deserialize)]
struct Account {
    id: i64,
    #[serde(rename = "type")]
    account_type: String,
}

#[derive(Deserialize)]
pub struct NewApp {
    pub id: i64,
    pub slug: String,
    pub pem: String,
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
}

#[derive(Deserialize)]
struct Comparison {
    commits: Vec<ComparedCommit>,
}

#[derive(Deserialize)]
struct ComparedCommit {
    commit: CommitData,
}

#[derive(Deserialize)]
struct CommitData {
    message: String,
}

#[derive(Deserialize)]
pub struct UserTokens {
    pub access_token: String,
    pub refresh_token: String,
    // Seconds.
    pub expires_in: i64,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CodeExchange {
    Tokens(UserTokens),
    Refused { error_description: String },
}

#[derive(Deserialize)]
pub struct User {
    pub login: String,
}

// The permission names with the levels that the Mobius App needs.
pub const REQUIRED_PERMISSIONS: [(&str, &str); 7] = [
    ("issues", "write"),
    ("pull_requests", "write"),
    ("contents", "write"),
    ("checks", "write"),
    ("workflows", "write"),
    ("actions", "read"),
    ("metadata", "read"),
];

fn level_rank(level: &str) -> u8 {
    match level {
        "read" => 1,
        "write" => 2,
        "admin" => 3,
        _ => 0,
    }
}

// The levels are in the order `read`, `write`, `admin`, and a level includes the lower levels.
pub fn grants(permissions: &HashMap<String, String>, name: &str, required: &str) -> bool {
    permissions
        .get(name)
        .is_some_and(|level| level_rank(level) >= level_rank(required))
}

pub struct AppAccess {
    pub app_permissions: HashMap<String, String>,
    pub installation_permissions: HashMap<String, String>,
    // The page of the App where the Owner adds a permission.
    pub app_permissions_url: String,
    // The page of the installation where the Owner accepts the new permissions.
    pub installation_url: String,
}

pub fn manifest(origin: &str, name: &str) -> String {
    json!({
        "name": name,
        "url": "https://github.com/Mobius-Toolkit/Mobius",
        "redirect_url": format!("{origin}/api/github/manifest-callback"),
        "callback_urls": [format!("{origin}/api/github/user-callback")],
        "request_oauth_on_install": true,
        "public": false,
        "default_permissions": REQUIRED_PERMISSIONS.iter().copied().collect::<BTreeMap<_, _>>()
    })
    .to_string()
}

impl GitHub {
    pub fn new(api_url: &str, web_url: &str) -> Result<GitHub, Box<dyn Error + Send + Sync>> {
        Ok(GitHub {
            api: Octocrab::builder().base_uri(api_url)?.build()?,
            web: Octocrab::builder()
                .base_uri(web_url)?
                .add_header(ACCEPT, "application/json".to_string())
                .build()?,
            api_url: api_url.to_string(),
            web_url: web_url.to_string(),
        })
    }

    pub async fn repositories(
        &self,
        app_id: i64,
        app_slug: &str,
        private_key: &str,
    ) -> Result<Vec<Repository>, Box<dyn Error + Send + Sync>> {
        let app = self.app_client(app_id, private_key)?;
        let installations =
            all_pages(&app, "/app/installations", |page: Vec<Installation>| page).await?;
        let mut repositories = Vec::new();
        for installation in installations {
            // This call gets the token before the clones, so each clone holds the token.
            let (client, token) = app
                .installation_and_token(InstallationId(installation.id))
                .await?;
            let names = all_pages(
                &client,
                "/installation/repositories",
                |page: InstallationRepositories| page.repositories,
            )
            .await?;
            repositories.extend(names.into_iter().map(|repository| Repository {
                full_name: repository.full_name,
                clone_url: repository.clone_url,
                default_branch: repository.default_branch,
                app_id,
                app_slug: app_slug.to_string(),
                client: client.clone(),
                token: token.expose_secret().to_string(),
            }));
        }
        Ok(repositories)
    }

    fn app_client(
        &self,
        app_id: i64,
        private_key: &str,
    ) -> Result<Octocrab, Box<dyn Error + Send + Sync>> {
        Ok(Octocrab::builder()
            .base_uri(self.api_url.as_str())?
            .app(
                AppId(u64::try_from(app_id)?),
                EncodingKey::from_rsa_pem(private_key.as_bytes())?,
            )
            .build()?)
    }

    pub async fn app_access(
        &self,
        app_id: i64,
        app_slug: &str,
        private_key: &str,
        account: &str,
    ) -> Result<AppAccess, Box<dyn Error + Send + Sync>> {
        let app = self.app_client(app_id, private_key)?;
        let installations =
            all_pages(&app, "/app/installations", |page: Vec<Installation>| page).await?;
        let installation = installations
            .into_iter()
            .find(|installation| installation.account.login.eq_ignore_ascii_case(account))
            .ok_or_else(|| format!("The Mobius App has no installation in {account}."))?;
        let found: AppPermissions = app.get("/app", None::<&()>).await?;
        let organization = installation.account.account_type == "Organization";
        let (app_permissions_url, installation_url) = if organization {
            (
                format!(
                    "{}/organizations/{account}/settings/apps/{app_slug}/permissions",
                    self.web_url
                ),
                format!(
                    "{}/organizations/{account}/settings/installations/{}",
                    self.web_url, installation.id
                ),
            )
        } else {
            (
                format!("{}/settings/apps/{app_slug}/permissions", self.web_url),
                format!(
                    "{}/settings/installations/{}",
                    self.web_url, installation.id
                ),
            )
        };
        Ok(AppAccess {
            app_permissions: found.permissions,
            installation_permissions: installation.permissions,
            app_permissions_url,
            installation_url,
        })
    }

    pub async fn manifest_url(
        &self,
        account: &str,
    ) -> Result<String, Box<dyn Error + Send + Sync>> {
        let found: Account = self
            .api
            .get(format!("/users/{account}"), None::<&()>)
            .await?;
        Ok(if found.account_type == "Organization" {
            format!("{}/organizations/{account}/settings/apps/new", self.web_url)
        } else {
            format!("{}/settings/apps/new", self.web_url)
        })
    }

    pub async fn user_id(&self, login: &str) -> Result<i64, Box<dyn Error + Send + Sync>> {
        let login = login.replace('[', "%5B").replace(']', "%5D");
        let found: Account = self.api.get(format!("/users/{login}"), None::<&()>).await?;
        Ok(found.id)
    }

    pub async fn convert_manifest(
        &self,
        code: &str,
    ) -> Result<NewApp, Box<dyn Error + Send + Sync>> {
        Ok(self
            .api
            .post(format!("/app-manifests/{code}/conversions"), None::<&()>)
            .await?)
    }

    pub fn authorize_url(&self, client_id: &str) -> String {
        format!(
            "{}/login/oauth/authorize?client_id={client_id}",
            self.web_url
        )
    }

    pub async fn user_tokens(
        &self,
        client_id: &str,
        client_secret: &str,
        code: &str,
    ) -> Result<UserTokens, Box<dyn Error + Send + Sync>> {
        self.exchange(json!({
            "client_id": client_id,
            "client_secret": client_secret,
            "code": code
        }))
        .await
    }

    pub async fn refresh_user_tokens(
        &self,
        client_id: &str,
        client_secret: &str,
        refresh_token: &str,
    ) -> Result<UserTokens, Box<dyn Error + Send + Sync>> {
        self.exchange(json!({
            "client_id": client_id,
            "client_secret": client_secret,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token
        }))
        .await
    }

    async fn exchange(
        &self,
        body: serde_json::Value,
    ) -> Result<UserTokens, Box<dyn Error + Send + Sync>> {
        let exchange: CodeExchange = self
            .web
            .post("/login/oauth/access_token", Some(&body))
            .await?;
        match exchange {
            CodeExchange::Tokens(tokens) => Ok(tokens),
            CodeExchange::Refused { error_description } => Err(error_description.into()),
        }
    }

    pub async fn user_login(
        &self,
        user_token: &str,
    ) -> Result<String, Box<dyn Error + Send + Sync>> {
        let user: User = self
            .api
            .user_access_token(user_token.to_string())?
            .get("/user", None::<&()>)
            .await?;
        Ok(user.login)
    }

    // The repository is public, so the call needs no token.
    pub async fn latest_release(&self) -> Result<Release, Box<dyn Error + Send + Sync>> {
        Ok(self
            .api
            .get("/repos/Mobius-Toolkit/Mobius/releases/latest", None::<&()>)
            .await?)
    }

    // The messages of the commits after `current` up to `new`, the oldest first. The response holds at most 250 commits.
    pub async fn commit_messages(
        &self,
        current: &str,
        new: &str,
    ) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        let comparison: Comparison = self
            .api
            .get(
                format!("/repos/Mobius-Toolkit/Mobius/compare/{current}...{new}"),
                None::<&()>,
            )
            .await?;
        Ok(comparison
            .commits
            .into_iter()
            .map(|compared| compared.commit.message)
            .collect())
    }

    pub fn release_url(&self, tag: &str, asset: &str) -> String {
        format!(
            "{}/Mobius-Toolkit/Mobius/releases/download/{tag}/{asset}",
            self.web_url
        )
    }
}

impl Repository {
    // The installation token of the last poll.
    pub fn token(&self) -> &str {
        &self.token
    }

    // A copy whose writes name the user of `user_token` as the actor, not the App.
    pub fn with_user_token(
        &self,
        user_token: &str,
    ) -> Result<Repository, Box<dyn Error + Send + Sync>> {
        Ok(Repository {
            client: self.client.user_access_token(user_token.to_string())?,
            ..self.clone()
        })
    }

    pub async fn create_draft_pull_request(
        &self,
        title: &str,
        head: &str,
        base: &str,
        body: &str,
    ) -> Result<PullRequest, Box<dyn Error + Send + Sync>> {
        Ok(self
            .client
            .post(
                format!("/repos/{}/pulls", self.full_name),
                Some(&json!({
                    "title": title,
                    "head": head,
                    "base": base,
                    "body": body,
                    "draft": true
                })),
            )
            .await?)
    }

    pub async fn pull_request(
        &self,
        number: i64,
    ) -> Result<PullRequest, Box<dyn Error + Send + Sync>> {
        Ok(self
            .client
            .get(
                format!("/repos/{}/pulls/{number}", self.full_name),
                None::<&()>,
            )
            .await?)
    }

    // Gives the id of the check run.
    pub async fn create_check_run(
        &self,
        name: &str,
        head_sha: &str,
        status: &str,
    ) -> Result<i64, Box<dyn Error + Send + Sync>> {
        let created: Created = self
            .client
            .post(
                format!("/repos/{}/check-runs", self.full_name),
                Some(&json!({ "name": name, "head_sha": head_sha, "status": status })),
            )
            .await?;
        Ok(created.id)
    }

    pub async fn set_check_run_conclusion(
        &self,
        id: i64,
        conclusion: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let _: serde_json::Value = self
            .client
            .patch(
                format!("/repos/{}/check-runs/{id}", self.full_name),
                Some(&json!({ "status": "completed", "conclusion": conclusion })),
            )
            .await?;
        Ok(())
    }

    pub async fn create_failed_check_run(
        &self,
        name: &str,
        head_sha: &str,
        title: &str,
        summary: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let _: serde_json::Value = self
            .client
            .post(
                format!("/repos/{}/check-runs", self.full_name),
                Some(&json!({
                    "name": name,
                    "head_sha": head_sha,
                    "status": "completed",
                    "conclusion": "failure",
                    "output": { "title": title, "summary": summary }
                })),
            )
            .await?;
        Ok(())
    }

    pub async fn check_runs(
        &self,
        head_sha: &str,
    ) -> Result<Vec<CheckRun>, Box<dyn Error + Send + Sync>> {
        all_pages(
            &self.client,
            &format!("/repos/{}/commits/{head_sha}/check-runs", self.full_name),
            |page: CheckRuns| page.check_runs,
        )
        .await
    }

    pub async fn check_run_annotations(
        &self,
        id: i64,
    ) -> Result<Vec<Annotation>, Box<dyn Error + Send + Sync>> {
        all_pages(
            &self.client,
            &format!("/repos/{}/check-runs/{id}/annotations", self.full_name),
            |page: Vec<Annotation>| page,
        )
        .await
    }

    // The job id of a check run of GitHub Actions is the id of the check run.
    pub async fn job_log(&self, id: i64) -> Result<String, Box<dyn Error + Send + Sync>> {
        let response = self
            .client
            ._get(format!("/repos/{}/actions/jobs/{id}/logs", self.full_name))
            .await?;
        let response = self.client.follow_location_to_data(response).await?;
        let response = octocrab::map_github_error(response).await?;
        Ok(self.client.body_to_string(response).await?)
    }

    pub async fn open_issues_with_label(
        &self,
        label: &str,
    ) -> Result<Vec<Issue>, Box<dyn Error + Send + Sync>> {
        let issues = all_pages(
            &self.client,
            &format!("/repos/{}/issues?state=open&labels={label}", self.full_name),
            |page: Vec<Issue>| page,
        )
        .await?;
        Ok(issues
            .into_iter()
            .filter(|issue| issue.pull_request.is_none())
            .collect())
    }

    // Gives `None` when the repository has no issue or pull request with this number.
    pub async fn issue(&self, number: i64) -> Result<Option<Issue>, Box<dyn Error + Send + Sync>> {
        found(
            self.client
                .get(
                    format!("/repos/{}/issues/{number}", self.full_name),
                    None::<&()>,
                )
                .await,
        )
    }

    // Gives `None` when the issue has no parent.
    pub async fn parent(&self, number: i64) -> Result<Option<Issue>, Box<dyn Error + Send + Sync>> {
        found(
            self.client
                .get(
                    format!("/repos/{}/issues/{number}/parent", self.full_name),
                    None::<&()>,
                )
                .await,
        )
    }

    pub async fn add_label(
        &self,
        number: i64,
        label: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let _: serde_json::Value = self
            .client
            .post(
                format!("/repos/{}/issues/{number}/labels", self.full_name),
                Some(&json!({ "labels": [label] })),
            )
            .await?;
        Ok(())
    }

    // An issue that does not have the label is not an error.
    pub async fn remove_label(
        &self,
        number: i64,
        label: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        found(
            self.client
                .delete::<serde_json::Value, _, _>(
                    format!("/repos/{}/issues/{number}/labels/{label}", self.full_name),
                    None::<&()>,
                )
                .await,
        )?;
        Ok(())
    }

    pub async fn labels(&self) -> Result<Vec<RepositoryLabel>, Box<dyn Error + Send + Sync>> {
        all_pages(
            &self.client,
            &format!("/repos/{}/labels", self.full_name),
            |page: Vec<RepositoryLabel>| page,
        )
        .await
    }

    // The color goes without `#`.
    pub async fn create_label(
        &self,
        name: &str,
        color: &str,
        description: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let _: serde_json::Value = self
            .client
            .post(
                format!("/repos/{}/labels", self.full_name),
                Some(&json!({ "name": name, "color": color, "description": description })),
            )
            .await?;
        Ok(())
    }

    pub async fn set_label_color(
        &self,
        name: &str,
        color: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let _: serde_json::Value = self
            .client
            .patch(
                format!(
                    "/repos/{}/labels/{}",
                    self.full_name,
                    name.replace(':', "%3A")
                ),
                Some(&json!({ "color": color })),
            )
            .await?;
        Ok(())
    }

    pub async fn close_as_not_planned(
        &self,
        number: i64,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let _: serde_json::Value = self
            .client
            .patch(
                format!("/repos/{}/issues/{number}", self.full_name),
                Some(&json!({ "state": "closed", "state_reason": "not_planned" })),
            )
            .await?;
        Ok(())
    }

    pub async fn close_as_completed(
        &self,
        number: i64,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let _: serde_json::Value = self
            .client
            .patch(
                format!("/repos/{}/issues/{number}", self.full_name),
                Some(&json!({ "state": "closed", "state_reason": "completed" })),
            )
            .await?;
        Ok(())
    }

    pub async fn close_pull_request(
        &self,
        number: i64,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let _: serde_json::Value = self
            .client
            .patch(
                format!("/repos/{}/pulls/{number}", self.full_name),
                Some(&json!({ "state": "closed" })),
            )
            .await?;
        Ok(())
    }

    // Gives the id of the new comment.
    pub async fn add_comment(
        &self,
        number: i64,
        body: &str,
    ) -> Result<i64, Box<dyn Error + Send + Sync>> {
        let created: Created = self
            .client
            .post(
                format!("/repos/{}/issues/{number}/comments", self.full_name),
                Some(&json!({ "body": body })),
            )
            .await?;
        Ok(created.id)
    }

    pub async fn update_comment(
        &self,
        id: i64,
        body: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let _: serde_json::Value = self
            .client
            .patch(
                format!("/repos/{}/issues/comments/{id}", self.full_name),
                Some(&json!({ "body": body })),
            )
            .await?;
        Ok(())
    }

    pub async fn issue_comments(
        &self,
        number: i64,
    ) -> Result<Vec<Comment>, Box<dyn Error + Send + Sync>> {
        all_pages(
            &self.client,
            &format!("/repos/{}/issues/{number}/comments", self.full_name),
            |page: Vec<Comment>| page,
        )
        .await
    }

    pub async fn reviews(&self, number: i64) -> Result<Vec<Review>, Box<dyn Error + Send + Sync>> {
        all_pages(
            &self.client,
            &format!("/repos/{}/pulls/{number}/reviews", self.full_name),
            |page: Vec<Review>| page,
        )
        .await
    }

    pub async fn review_comments(
        &self,
        number: i64,
    ) -> Result<Vec<ReviewComment>, Box<dyn Error + Send + Sync>> {
        all_pages(
            &self.client,
            &format!("/repos/{}/pulls/{number}/comments", self.full_name),
            |page: Vec<ReviewComment>| page,
        )
        .await
    }

    pub async fn submit_review(
        &self,
        number: i64,
        commit_id: &str,
        body: &str,
        comments: &[NewReviewComment],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let _: serde_json::Value = self
            .client
            .post(
                format!("/repos/{}/pulls/{number}/reviews", self.full_name),
                Some(&json!({
                    "commit_id": commit_id,
                    "body": body,
                    "event": "COMMENT",
                    "comments": comments
                })),
            )
            .await?;
        Ok(())
    }

    pub async fn review_threads(
        &self,
        number: i64,
    ) -> Result<Vec<ReviewThread>, Box<dyn Error + Send + Sync>> {
        let (owner, name) = self
            .full_name
            .split_once('/')
            .ok_or_else(|| format!("{} has no owner.", self.full_name))?;
        let mut threads = Vec::new();
        let mut after = serde_json::Value::Null;
        loop {
            let data: serde_json::Value = self
                .client
                .graphql(&json!({
                    "query": "query($owner: String!, $name: String!, $number: Int!, $after: String) {
                        repository(owner: $owner, name: $name) {
                            pullRequest(number: $number) {
                                reviewThreads(first: 100, after: $after) {
                                    nodes {
                                        id
                                        isResolved
                                        comments(first: 100) { nodes { databaseId author { __typename login } } }
                                    }
                                    pageInfo { hasNextPage endCursor }
                                }
                            }
                        }
                    }",
                    "variables": { "owner": owner, "name": name, "number": number, "after": after }
                }))
                .await?;
            let page = &data["repository"]["pullRequest"]["reviewThreads"];
            for node in page["nodes"]
                .as_array()
                .ok_or("GitHub gave no review threads.")?
            {
                let comments = node["comments"]["nodes"]
                    .as_array()
                    .ok_or("GitHub gave no review thread comments.")?;
                threads.push(ReviewThread {
                    id: node["id"]
                        .as_str()
                        .ok_or("GitHub gave no review thread id.")?
                        .to_string(),
                    comment: comments
                        .first()
                        .and_then(|comment| comment["databaseId"].as_i64())
                        .ok_or("GitHub gave a review thread with no comment.")?,
                    resolved: node["isResolved"] == true,
                    authors: comments
                        .iter()
                        .map(|comment| rest_login(&comment["author"]))
                        .collect(),
                });
            }
            if page["pageInfo"]["hasNextPage"] != true {
                return Ok(threads);
            }
            after = page["pageInfo"]["endCursor"].clone();
        }
    }

    pub async fn reply_to_review_comment(
        &self,
        number: i64,
        comment: i64,
        body: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let _: serde_json::Value = self
            .client
            .post(
                format!(
                    "/repos/{}/pulls/{number}/comments/{comment}/replies",
                    self.full_name
                ),
                Some(&json!({ "body": body })),
            )
            .await?;
        Ok(())
    }

    pub async fn resolve_review_thread(
        &self,
        id: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let _: serde_json::Value = self
            .client
            .graphql(&json!({
                "query": "mutation($id: ID!) {
                    resolveReviewThread(input: { threadId: $id }) { clientMutationId }
                }",
                "variables": { "id": id }
            }))
            .await?;
        Ok(())
    }

    pub async fn mark_ready_for_review(
        &self,
        node_id: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let _: serde_json::Value = self
            .client
            .graphql(&json!({
                "query": "mutation($id: ID!) {
                    markPullRequestReadyForReview(input: { pullRequestId: $id }) { clientMutationId }
                }",
                "variables": { "id": node_id }
            }))
            .await?;
        Ok(())
    }

    pub async fn create_issue(
        &self,
        title: &str,
        body: &str,
    ) -> Result<Issue, Box<dyn Error + Send + Sync>> {
        Ok(self
            .client
            .post(
                format!("/repos/{}/issues", self.full_name),
                Some(&json!({ "title": title, "body": body })),
            )
            .await?)
    }

    pub async fn add_sub_issue(
        &self,
        parent: i64,
        child_id: i64,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let _: serde_json::Value = self
            .client
            .post(
                format!("/repos/{}/issues/{parent}/sub_issues", self.full_name),
                Some(&json!({ "sub_issue_id": child_id, "replace_parent": true })),
            )
            .await?;
        Ok(())
    }

    pub async fn add_blocked_by(
        &self,
        number: i64,
        blocker_id: i64,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let _: serde_json::Value = self
            .client
            .post(
                format!(
                    "/repos/{}/issues/{number}/dependencies/blocked_by",
                    self.full_name
                ),
                Some(&json!({ "issue_id": blocker_id })),
            )
            .await?;
        Ok(())
    }

    pub async fn blocked_by(
        &self,
        number: i64,
    ) -> Result<Vec<Issue>, Box<dyn Error + Send + Sync>> {
        all_pages(
            &self.client,
            &format!(
                "/repos/{}/issues/{number}/dependencies/blocked_by",
                self.full_name
            ),
            |page: Vec<Issue>| page,
        )
        .await
    }

    pub async fn sub_issues(
        &self,
        number: i64,
    ) -> Result<Vec<Issue>, Box<dyn Error + Send + Sync>> {
        all_pages(
            &self.client,
            &format!("/repos/{}/issues/{number}/sub_issues", self.full_name),
            |page: Vec<Issue>| page,
        )
        .await
    }

    // GitHub allows 50 blockers for an issue. A query costs one point for each 100 open issues.
    pub async fn links(&self) -> Result<BTreeMap<i64, IssueLinks>, Box<dyn Error + Send + Sync>> {
        let (owner, name) = self
            .full_name
            .split_once('/')
            .ok_or_else(|| format!("{} has no owner.", self.full_name))?;
        let mut links = BTreeMap::new();
        let mut after = serde_json::Value::Null;
        loop {
            let data: serde_json::Value = self
                .client
                .graphql(&json!({
                    "query": "query($owner: String!, $name: String!, $after: String) {
                        repository(owner: $owner, name: $name) {
                            issues(first: 100, states: OPEN, after: $after) {
                                nodes {
                                    number
                                    parent { number repository { nameWithOwner } }
                                    subIssuesSummary { total }
                                    blockedBy(first: 50) {
                                        nodes { number state repository { nameWithOwner } }
                                    }
                                }
                                pageInfo { hasNextPage endCursor }
                            }
                        }
                    }",
                    "variables": { "owner": owner, "name": name, "after": after }
                }))
                .await?;
            let page = &data["repository"]["issues"];
            for node in page["nodes"].as_array().ok_or("GitHub gave no issues.")? {
                let in_repository = |issue: &serde_json::Value| {
                    issue["repository"]["nameWithOwner"]
                        .as_str()
                        .is_some_and(|name| name.eq_ignore_ascii_case(&self.full_name))
                };
                let parent = Some(&node["parent"])
                    .filter(|parent| in_repository(parent))
                    .and_then(|parent| parent["number"].as_i64());
                let blockers = node["blockedBy"]["nodes"]
                    .as_array()
                    .ok_or("GitHub gave no blockers.")?
                    .iter()
                    .filter(|blocker| blocker["state"] == "OPEN" && in_repository(blocker))
                    .filter_map(|blocker| blocker["number"].as_i64())
                    .collect();
                let sub_issues = node["subIssuesSummary"]["total"]
                    .as_i64()
                    .ok_or("GitHub gave no sub-issue total.")?;
                links.insert(
                    node["number"].as_i64().ok_or("GitHub gave no number.")?,
                    IssueLinks {
                        parent,
                        sub_issues,
                        blockers,
                    },
                );
            }
            if page["pageInfo"]["hasNextPage"] != true {
                return Ok(links);
            }
            after = page["pageInfo"]["endCursor"].clone();
        }
    }

    // Gives `None` when GitHub answers `304 Not Modified` to `etag`.
    pub async fn issues_since(
        &self,
        since: Option<OffsetDateTime>,
        etag: Option<&str>,
    ) -> Result<Option<IssuePage>, Box<dyn Error + Send + Sync>> {
        let since = match since {
            Some(since) => format!("&since={}", since.format(&Rfc3339)?),
            None => String::new(),
        };
        self.issue_pages(
            &format!("state=all&sort=updated&direction=asc{since}"),
            etag,
        )
        .await
    }

    // Gives the open issues and pull requests with the label, or `None` when GitHub answers `304 Not Modified` to `etag`.
    pub async fn labeled_issues(
        &self,
        label: &str,
        etag: Option<&str>,
    ) -> Result<Option<IssuePage>, Box<dyn Error + Send + Sync>> {
        self.issue_pages(&format!("state=open&labels={label}"), etag)
            .await
    }

    async fn issue_pages(
        &self,
        query: &str,
        etag: Option<&str>,
    ) -> Result<Option<IssuePage>, Box<dyn Error + Send + Sync>> {
        let mut issues = Vec::new();
        let mut first_etag = None;
        let mut pages = 0;
        for page in 1.. {
            pages = page;
            let mut headers = HeaderMap::new();
            if page == 1
                && let Some(etag) = etag
            {
                headers.insert(IF_NONE_MATCH, HeaderValue::from_str(etag)?);
            }
            let uri = format!(
                "/repos/{}/issues?{query}&per_page={PAGE_SIZE}&page={page}",
                self.full_name
            );
            let response = self.client._get_with_headers(uri, Some(headers)).await?;
            if response.status() == StatusCode::NOT_MODIFIED {
                return Ok(None);
            }
            let response = octocrab::map_github_error(response).await?;
            if page == 1 {
                first_etag = response
                    .headers()
                    .get(ETAG)
                    .map(|value| value.to_str())
                    .transpose()?
                    .map(str::to_string);
            }
            let page: Vec<Issue> =
                serde_json::from_str(&self.client.body_to_string(response).await?)?;
            let last_page = page.len() < PAGE_SIZE;
            issues.extend(page);
            if last_page {
                break;
            }
        }
        // A `304` for page 1 says nothing about the pages after it.
        Ok(Some(IssuePage {
            issues,
            etag: first_etag.filter(|_| pages == 1),
        }))
    }

    pub async fn issue_events(
        &self,
        number: i64,
    ) -> Result<Vec<IssueEvent>, Box<dyn Error + Send + Sync>> {
        all_pages(
            &self.client,
            &format!("/repos/{}/issues/{number}/events", self.full_name),
            |page: Vec<IssueEvent>| page,
        )
        .await
    }
}

// GraphQL gives a bot login with no `[bot]`, and no author for a deleted account.
fn rest_login(author: &serde_json::Value) -> String {
    match (author["__typename"].as_str(), author["login"].as_str()) {
        (Some("Bot"), Some(login)) => format!("{login}[bot]"),
        (_, Some(login)) => login.to_string(),
        _ => "ghost".to_string(),
    }
}

fn found<T>(
    response: Result<T, octocrab::Error>,
) -> Result<Option<T>, Box<dyn Error + Send + Sync>> {
    match response {
        Ok(value) => Ok(Some(value)),
        Err(octocrab::Error::GitHub { source, .. })
            if source.status_code == StatusCode::NOT_FOUND
                || source.status_code == StatusCode::GONE =>
        {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

async fn all_pages<P: DeserializeOwned, T>(
    client: &Octocrab,
    route: &str,
    items_of: impl Fn(P) -> Vec<T>,
) -> Result<Vec<T>, Box<dyn Error + Send + Sync>> {
    let mut items = Vec::new();
    for page in 1.. {
        let page: P = client
            .get(route, Some(&[("per_page", PAGE_SIZE), ("page", page)]))
            .await?;
        let page = items_of(page);
        let last_page = page.len() < PAGE_SIZE;
        items.extend(page);
        if last_page {
            break;
        }
    }
    Ok(items)
}
