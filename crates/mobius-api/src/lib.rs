use dioxus::fullstack::{Redirect, ServerEvents, SetCookie, SetHeader};
use dioxus::prelude::*;
use mobius_domain::{
    ActiveAgents, AgentNode, ChatView, CheckupView, Devices, DrainEnd, InboxItem, Live,
    ManifestForm, NeedsHuman, TaskLine, TranscriptLine, Unread, Workstream,
};

#[cfg(feature = "server")]
use dioxus::fullstack::headers::UserAgent;
#[cfg(feature = "server")]
use dioxus::fullstack::{Cookie, TypedHeader};
#[cfg(feature = "server")]
use dioxus::server::axum::Extension;
#[cfg(feature = "server")]
use dioxus::server::axum::extract::{FromRequestParts, Query};
#[cfg(feature = "server")]
use dioxus::server::http::request::Parts;
#[cfg(feature = "server")]
use mobius_engine::{
    Engine, activity, agents, auth, chat, drain, github, inbox, limits, tasks, transcript, upgrade,
    workstreams,
};
#[cfg(feature = "server")]
use mobius_store::Store;
#[cfg(feature = "server")]
use serde::Deserialize;

#[cfg(feature = "server")]
const SESSION_MAX_AGE_SECONDS: u32 = 400 * 24 * 60 * 60;

#[cfg(feature = "server")]
pub struct DeviceId(pub i64);

#[cfg(feature = "server")]
impl<S: Send + Sync> FromRequestParts<S> for DeviceId {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, StatusCode> {
        let TypedHeader(cookie) = TypedHeader::<Cookie>::from_request_parts(parts, state)
            .await
            .map_err(|_| StatusCode::UNAUTHORIZED)?;
        let token = cookie
            .get("mobius_session")
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let engine = parts
            .extensions
            .get::<Engine>()
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
        match auth::check(engine, token).await {
            Ok(Some(device)) => Ok(DeviceId(device)),
            Ok(None) => Err(StatusCode::UNAUTHORIZED),
            Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }
}

#[cfg(feature = "server")]
#[derive(Deserialize)]
pub struct Callback {
    code: String,
}

#[post("/api/login", engine: Extension<Engine>, user_agent: TypedHeader<UserAgent>)]
pub async fn login(password: String) -> ServerFnResult<SetHeader<SetCookie>> {
    let Some(token) = auth::login(&engine, &password, user_agent.as_str())
        .await
        .map_err(ServerFnError::new)?
    else {
        return Err(
            HttpError::new(StatusCode::UNAUTHORIZED, "The access password is wrong.").into(),
        );
    };
    SetHeader::new(format!(
        "mobius_session={token}; HttpOnly; Path=/; SameSite=Lax; Max-Age={SESSION_MAX_AGE_SECONDS}"
    ))
    .map_err(ServerFnError::new)
}

#[get("/api/devices", device: DeviceId, store: Extension<Store>)]
pub async fn devices() -> ServerFnResult<Devices> {
    Ok(Devices {
        this_device: device.0,
        logins: store
            .device_logins()
            .list()
            .await
            .map_err(ServerFnError::new)?,
    })
}

#[post("/api/devices/logout", _device: DeviceId, engine: Extension<Engine>)]
pub async fn logout(id: i64) -> ServerFnResult<()> {
    auth::logout(&engine, id)
        .await
        .map_err(ServerFnError::new)?;
    Ok(())
}

#[get("/api/github/apps", _device: DeviceId, store: Extension<Store>)]
pub async fn github_apps() -> ServerFnResult<Vec<String>> {
    Ok(store
        .github_apps()
        .list()
        .await
        .map_err(ServerFnError::new)?
        .into_iter()
        .map(|app| app.slug)
        .collect())
}

#[post("/api/github/manifest", _device: DeviceId, engine: Extension<Engine>)]
pub async fn github_manifest(
    account: String,
    name: String,
    origin: String,
) -> ServerFnResult<ManifestForm> {
    github::manifest_form(&engine, &account, &name, &origin)
        .await
        .map_err(ServerFnError::new)
}

// The query is an extractor after `DeviceId`, so a request with no cookie gets 401 before the query is parsed.
#[get("/api/github/manifest-callback", _device: DeviceId, query: Query<Callback>, engine: Extension<Engine>)]
pub async fn github_manifest_callback() -> ServerFnResult<Redirect> {
    github::convert_manifest(&engine, &query.code)
        .await
        .map_err(ServerFnError::new)?;
    Ok(Redirect::to("/github"))
}

#[get("/api/github/user-callback", _device: DeviceId, query: Query<Callback>, engine: Extension<Engine>)]
pub async fn github_user_callback() -> ServerFnResult<Redirect> {
    if !github::authorize_user(&engine, &query.code)
        .await
        .map_err(ServerFnError::new)?
    {
        return Err(HttpError::new(
            StatusCode::FORBIDDEN,
            "The GitHub login is not a trusted user.",
        )
        .into());
    }
    Ok(Redirect::to("/github"))
}

#[get("/api/release", _device: DeviceId, engine: Extension<Engine>)]
pub async fn release() -> ServerFnResult<Option<String>> {
    Ok(github::new_release(&engine).await)
}

#[post("/api/release/changes", _device: DeviceId, engine: Extension<Engine>)]
pub async fn release_changes(new: String) -> ServerFnResult<Vec<String>> {
    github::release_changes(&engine, &new)
        .await
        .map_err(ServerFnError::new)
}

#[get("/api/organizations", _device: DeviceId, engine: Extension<Engine>)]
pub async fn organizations() -> ServerFnResult<Vec<String>> {
    Ok(workstreams::organizations(&engine))
}

#[get("/api/checkup?organization", _device: DeviceId, engine: Extension<Engine>)]
pub async fn checkup(organization: Option<String>) -> ServerFnResult<CheckupView> {
    mobius_engine::checkup::status(&engine, &organization.unwrap_or_default())
        .await
        .map_err(ServerFnError::new)
}

#[post("/api/checkup/fix", _device: DeviceId, engine: Extension<Engine>)]
pub async fn fix_labels(organization: String) -> ServerFnResult<()> {
    mobius_engine::checkup::fix(&engine, &organization)
        .await
        .map_err(ServerFnError::new)
}

#[get("/api/workstreams", _device: DeviceId, engine: Extension<Engine>)]
pub async fn workstreams() -> ServerFnResult<Vec<Workstream>> {
    workstreams::list(&engine).await.map_err(ServerFnError::new)
}

#[post("/api/workstreams/autopilot", _device: DeviceId, engine: Extension<Engine>)]
pub async fn workstream_autopilot(
    repository: String,
    workstream: i64,
    on: bool,
) -> ServerFnResult<()> {
    workstreams::set_autopilot(&engine, &repository, workstream, on)
        .await
        .map_err(ServerFnError::new)
}

#[post("/api/workstreams/close", _device: DeviceId, engine: Extension<Engine>)]
pub async fn workstream_close(repository: String, workstream: i64) -> ServerFnResult<()> {
    workstreams::complete(&engine, &repository, workstream)
        .await
        .map_err(ServerFnError::new)
}

#[get("/api/live?after", _device: DeviceId, engine: Extension<Engine>)]
pub async fn live(after: Option<i64>) -> ServerFnResult<ServerEvents<Live>> {
    let mut feed = activity::feed(&engine, after)
        .await
        .map_err(ServerFnError::new)?;
    Ok(ServerEvents::new(move |mut sender| async move {
        while let Some(live) = feed.next().await {
            if sender.send(live).await.is_err() {
                break;
            }
        }
    }))
}

#[post("/api/chat", _device: DeviceId, engine: Extension<Engine>)]
pub async fn chat_view(
    organization: String,
    repository: String,
    workstream: i64,
) -> ServerFnResult<ChatView> {
    chat::view(&engine, &organization, &repository, workstream)
        .await
        .map_err(ServerFnError::new)
}

#[post("/api/chat/send", _device: DeviceId, engine: Extension<Engine>)]
pub async fn chat_send(
    organization: String,
    repository: String,
    workstream: i64,
    text: String,
) -> ServerFnResult<()> {
    chat::send(&engine, &organization, &repository, workstream, &text)
        .await
        .map_err(ServerFnError::new)
}

#[post("/api/chat/stop", _device: DeviceId, engine: Extension<Engine>)]
pub async fn chat_stop(
    organization: String,
    repository: String,
    workstream: i64,
) -> ServerFnResult<()> {
    chat::stop(&engine, &organization, &repository, workstream).map_err(ServerFnError::new)
}

#[post("/api/chat/seen", _device: DeviceId, engine: Extension<Engine>)]
pub async fn chat_seen(
    organization: String,
    repository: String,
    workstream: i64,
    message: i64,
) -> ServerFnResult<()> {
    chat::seen(&engine, &organization, &repository, workstream, message)
        .await
        .map_err(ServerFnError::new)
}

#[get("/api/unread", _device: DeviceId, store: Extension<Store>)]
pub async fn unread() -> ServerFnResult<Vec<Unread>> {
    store
        .chat_messages()
        .unread()
        .await
        .map_err(ServerFnError::new)
}

#[get("/api/inbox", _device: DeviceId, engine: Extension<Engine>)]
pub async fn inbox_items() -> ServerFnResult<Vec<InboxItem>> {
    inbox::list(&engine).await.map_err(ServerFnError::new)
}

#[post("/api/inbox/dismiss", _device: DeviceId, engine: Extension<Engine>)]
pub async fn inbox_dismiss(id: i64) -> ServerFnResult<()> {
    inbox::dismiss(&engine, id)
        .await
        .map_err(ServerFnError::new)
}

#[post("/api/inbox/resume", _device: DeviceId, engine: Extension<Engine>)]
pub async fn inbox_resume(id: i64) -> ServerFnResult<()> {
    limits::resume(&engine, id)
        .await
        .map_err(ServerFnError::new)
}

#[post("/api/agents", _device: DeviceId, engine: Extension<Engine>)]
pub async fn agent_tree(repository: String, workstream: i64) -> ServerFnResult<Vec<AgentNode>> {
    agents::tree(&engine, &repository, workstream)
        .await
        .map_err(ServerFnError::new)
}

#[get("/api/active-agents", _device: DeviceId, engine: Extension<Engine>)]
pub async fn active_agents() -> ServerFnResult<ActiveAgents> {
    agents::groups(&engine).await.map_err(ServerFnError::new)
}

#[post("/api/tasks", _device: DeviceId, engine: Extension<Engine>)]
pub async fn task_list(repository: String, workstream: i64) -> ServerFnResult<Vec<TaskLine>> {
    tasks::list(&engine, &repository, workstream)
        .await
        .map_err(ServerFnError::new)
}

#[post("/api/tasks/needs-human", _device: DeviceId, engine: Extension<Engine>)]
pub async fn needs_human_list(
    repository: String,
    workstream: i64,
) -> ServerFnResult<Vec<NeedsHuman>> {
    tasks::needs_human(&engine, &repository, workstream)
        .await
        .map_err(ServerFnError::new)
}

#[get("/api/tasks/needs-human-workstreams", _device: DeviceId, engine: Extension<Engine>)]
pub async fn needs_human_workstreams() -> ServerFnResult<Vec<(String, i64)>> {
    tasks::needs_human_workstreams(&engine)
        .await
        .map_err(ServerFnError::new)
}

#[post("/api/tasks/resume", _device: DeviceId, engine: Extension<Engine>)]
pub async fn task_resume(repository: String, issue: i64) -> ServerFnResult<()> {
    tasks::resume(&engine, &repository, issue)
        .await
        .map_err(ServerFnError::new)
}

#[post("/api/transcript", _device: DeviceId, engine: Extension<Engine>)]
pub async fn transcript_lines(session: i64) -> ServerFnResult<Vec<TranscriptLine>> {
    transcript::lines(&engine, session)
        .await
        .map_err(ServerFnError::new)
}

// The call returns `Drained` when the upgrade restarts Mobius, `Cancelled` when the Owner cancels the drain, or an error.
#[post("/api/upgrade", _device: DeviceId, engine: Extension<Engine>)]
pub async fn upgrade() -> ServerFnResult<DrainEnd> {
    upgrade::run(&engine).await.map_err(ServerFnError::new)
}

#[get("/api/upgrade/error", _device: DeviceId, engine: Extension<Engine>)]
pub async fn upgrade_error() -> ServerFnResult<Option<String>> {
    Ok(upgrade::last_error(&engine))
}

#[get("/api/drain", _device: DeviceId, engine: Extension<Engine>)]
pub async fn drain_state() -> ServerFnResult<Option<usize>> {
    Ok(drain::waiting(&engine))
}

// The call returns only when the drain ends, so the Owner can cancel it with `drain_cancel`.
#[post("/api/drain/start", _device: DeviceId, engine: Extension<Engine>)]
pub async fn drain_start() -> ServerFnResult<DrainEnd> {
    Ok(drain::start(&engine).await)
}

#[post("/api/drain/cancel", _device: DeviceId, engine: Extension<Engine>)]
pub async fn drain_cancel() -> ServerFnResult<()> {
    drain::cancel(&engine).await.map_err(ServerFnError::new)
}
