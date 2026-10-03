mod markdown;

use std::cmp::Reverse;
use std::collections::HashMap;
use std::pin::pin;

use dioxus::fullstack::ServerEvents;
use dioxus::prelude::*;
use futures_util::future::{Either, select};
use markdown::Markdown;
use mobius_api::{
    active_agents, agent_tree, chat_seen, chat_send, chat_stop, chat_view, checkup, devices,
    drain_cancel, drain_state, fix_labels, github_apps, github_manifest, inbox_dismiss,
    inbox_items, inbox_resume, live, login, logout, needs_human_list, needs_human_workstreams,
    organizations, release, release_changes, task_list, task_resume, transcript_lines, unread,
    upgrade, upgrade_error as upgrade_error_state, workstream_autopilot, workstream_close,
    workstreams,
};
use mobius_domain::{
    ActiveAgent, AgentNode, Author, ChatMessage, DrainEnd, FeedRow, InboxItem, InboxKind,
    LabelStatus, Live, PAUSED, PermissionStatus, RepositoryCheckup, TaskLine, TranscriptLine,
    Workstream, agent_rows, shown_agents,
};
use time::UtcOffset;
use time::macros::format_description;

const MAIN_CSS: Asset = asset!("/assets/main.css");

// The identifier of this web UI build. The server gives the same identifier at
// `/ui-version`; a difference means a new version of the web UI is on the server.
pub const BUILD: &str = env!("MOBIUS_BUILD");

// Asks the server for its web UI build on start, when the window gets focus or
// becomes visible again, and every five minutes. A request that fails or is
// refused stays quiet; the live loop handles a lost session.
const VERSION_POLL: &str = r#"
const check = async () => {
    try {
        const response = await fetch("/ui-version", { cache: "no-store" });
        if (response.ok) {
            dioxus.send(await response.text());
        }
    } catch {}
};
window.addEventListener("focus", check);
document.addEventListener("visibilitychange", () => {
    if (!document.hidden) check();
});
setInterval(check, 5 * 60 * 1000);
check();
"#;

// Sends a message when the page becomes visible again or the browser comes back online. A phone
// that stops the page in the background can leave a live stream open with no error. Only the
// newest call of this script receives the messages, and `window.liveWake = null` stops them.
const LIVE_WAKE: &str = r#"
if (!window.liveWakeListening) {
    window.liveWakeListening = true;
    window.addEventListener("online", () => window.liveWake?.());
    document.addEventListener("visibilitychange", () => {
        if (!document.hidden) {
            window.liveWake?.();
        }
    });
}
window.liveWake = () => dioxus.send(true);
"#;

const LIVE_WAKE_STOP: &str = "window.liveWake = null;";

// Runs after an upgrade call returns. The old server answers until it restarts, so only another build ends the wait.
fn reload_on_new_build() -> String {
    let build = serde_json::to_string(BUILD).unwrap_or_default();
    format!(
        r#"
const wait = async () => {{
    try {{
        const response = await fetch("/ui-version", {{ cache: "no-store" }});
        if (response.ok && (await response.text()).trim() !== {build}) {{
            location.reload();
            return;
        }}
    }} catch {{}}
    setTimeout(wait, 1000);
}};
wait();
"#
    )
}

#[derive(Clone, PartialEq, Routable)]
#[rustfmt::skip]
pub enum Route {
    #[layout(Shell)]
        #[redirect("/", || Route::WorkstreamList {})]
        #[route("/workstreams")]
        WorkstreamList {},
        #[route("/workstreams/:owner/:repo/:number")]
        Chat { owner: String, repo: String, number: i64 },
        #[route("/workstreams/new")]
        NewWorkstream {},
        #[route("/agents")]
        AgentsPage {},
        #[route("/inbox")]
        Inbox {},
        #[route("/activity")]
        Activity {},
        #[route("/devices")]
        Devices {},
        #[route("/github")]
        GitHub {},
        #[route("/settings")]
        Settings {},
        #[route("/settings/checkup")]
        Checkup {},
}

#[component]
pub fn App() -> Element {
    // The on-screen keyboard shrinks the layout viewport instead of scrolling the
    // page, so the chat input stays above the keyboard; the tab bar hides on focus.
    use_effect(|| {
        document::eval(
            r#"let meta = document.head.querySelector('meta[name="viewport"]');
if (!meta) {
    meta = document.createElement("meta");
    meta.name = "viewport";
    document.head.append(meta);
}
meta.content = "width=device-width, initial-scale=1, interactive-widget=resizes-content";"#,
        );
    });
    rsx! {
        document::Link { rel: "icon", r#type: "image/svg+xml", href: "/icon.svg" }
        document::Link { rel: "apple-touch-icon", href: "/apple-touch-icon.png" }
        document::Link { rel: "manifest", href: "/manifest.webmanifest" }
        document::Meta { name: "theme-color", content: "#2d5f8b" }
        document::Meta { name: "mobile-web-app-capable", content: "yes" }
        document::Meta { name: "apple-mobile-web-app-capable", content: "yes" }
        document::Meta { name: "apple-mobile-web-app-title", content: "Mobius" }
        document::Meta { name: "apple-mobile-web-app-status-bar-style", content: "default" }
        // The service worker makes the app installable. It keeps no cache.
        document::Script { "navigator.serviceWorker?.register('/sw.js');" }
        document::Stylesheet { href: MAIN_CSS }
        Router::<Route> {}
    }
}

/// The axum router of the web UI. The app shell, the service worker, the web
/// app manifest, and the build identifier get `Cache-Control: no-cache`, so the
/// browser always asks the server for them.
#[cfg(feature = "server")]
pub fn router() -> dioxus::server::axum::Router {
    use dioxus::server::axum::extract::Request;
    use dioxus::server::axum::http::HeaderValue;
    use dioxus::server::axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
    use dioxus::server::axum::middleware::{Next, from_fn};
    use dioxus::server::axum::response::Response;
    use dioxus::server::axum::routing::get;
    use mobius_api::DeviceId;

    async fn ui_build(_device: DeviceId) -> &'static str {
        BUILD
    }

    async fn no_cache(request: Request, next: Next) -> Response {
        let always_fresh = matches!(
            request.uri().path(),
            "/" | "/sw.js" | "/manifest.webmanifest" | "/ui-version"
        );
        let mut response = next.run(request).await;
        // Every other path of the app renders the same shell.
        let shell = response
            .headers()
            .get(CONTENT_TYPE)
            .is_some_and(|value| value.as_bytes().starts_with(b"text/html"));
        if always_fresh || shell {
            response
                .headers_mut()
                .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        }
        response
    }

    dioxus::server::router(App)
        .route("/ui-version", get(ui_build))
        .layer(from_fn(no_cache))
}

#[derive(Clone, Copy)]
struct LoginShown(Signal<bool>);

#[derive(Clone, Copy)]
struct AppSlugs(Resource<ServerFnResult<Vec<String>>>);

#[derive(Clone, Copy)]
struct Organizations(Resource<ServerFnResult<Vec<String>>>);

// The organization of the repositories that the pages show.
#[derive(Clone, Copy)]
struct Organization(Signal<String>);

// The offset of the time zone of the browser. The server gives each time in UTC.
#[derive(Clone, Copy)]
struct LocalOffset(Signal<UtcOffset>);

#[derive(Clone, Copy)]
struct Workstreams(Resource<ServerFnResult<Vec<Workstream>>>);

// The organization, the repository, and the Workstream. The Triager chat has the empty repository.
type ChatKey = (String, String, i64);

#[derive(Clone, PartialEq)]
struct LeadState {
    writing: bool,
    error: Option<String>,
}

#[derive(Clone, Copy)]
struct LiveState {
    feed: Signal<Vec<FeedRow>>,
    messages: Signal<Vec<ChatMessage>>,
    leads: Signal<HashMap<ChatKey, LeadState>>,
    unread: Signal<HashMap<ChatKey, i64>>,
    // True after the first read of the unread counts, whether it worked or not.
    unread_read: Signal<bool>,
    // The number of live connections after the first. Data that only live events update reads again when it changes.
    reconnects: Signal<u32>,
    agents: Signal<HashMap<i64, AgentNode>>,
    inbox: Signal<HashMap<i64, InboxItem>>,
    // The number of agents the upgrade drain waits for. `None` means no drain.
    drain: Signal<Option<usize>>,
    // The error of the last upgrade. An empty text means no error.
    upgrade_error: Signal<String>,
}

fn select_organization(mut organization: Signal<String>, name: String) {
    if *organization.peek() == name {
        return;
    }
    let value = serde_json::to_string(&name).unwrap_or_default();
    document::eval(&format!(
        "try {{ localStorage.setItem('organization', {value}); }} catch {{}}"
    ));
    organization.set(name);
}

fn switchable() -> bool {
    let Organizations(list) = use_context();
    matches!(&*list.read(), Some(Ok(list)) if list.len() > 1)
}

fn unauthorized(error: &ServerFnError) -> bool {
    matches!(error, ServerFnError::ServerError { code: 401, .. })
}

fn is_ime_key(event: &KeyboardEvent) -> bool {
    event
        .downcast::<web_sys::KeyboardEvent>()
        .is_some_and(|event| event.key_code() == 229)
}

fn fold_event(author: &Author, text: &str) -> Option<(String, String)> {
    if *author != Author::Event {
        return None;
    }
    let (summary, body) = text.split_once('\n')?;
    let body = body.trim_start_matches('\n');
    (!body.is_empty()).then(|| (summary.to_string(), body.to_string()))
}

fn error_text(error: &ServerFnError) -> String {
    match error {
        ServerFnError::ServerError { message, .. } => message.clone(),
        _ => error.to_string(),
    }
}

#[component]
fn Shell() -> Element {
    let LoginShown(login_shown) = use_context_provider(|| LoginShown(Signal::new(false)));
    if login_shown() {
        return rsx! { Login {} };
    }
    rsx! { Frame {} }
}

fn upsert(messages: &mut Vec<ChatMessage>, message: ChatMessage) {
    match messages.iter_mut().find(|known| known.id == message.id) {
        Some(known) => *known = message,
        None => messages.push(message),
    }
}

// Gives `None` when the stream ends or the page wakes up, so the caller connects again.
async fn next_event(
    events: &mut ServerEvents<Live>,
    wake: &mut document::Eval,
) -> Option<Result<Live, ServerFnError>> {
    match select(pin!(events.recv()), pin!(wake.recv::<bool>())).await {
        Either::Left((event, _)) => event,
        Either::Right(_) => None,
    }
}

async fn follow_live(
    mut state: LiveState,
    mut workstream_list: Resource<ServerFnResult<Vec<Workstream>>>,
    mut login_shown: Signal<bool>,
) {
    let mut after = None;
    let mut first = true;
    loop {
        if let Ok(items) = inbox_items().await {
            state
                .inbox
                .set(items.into_iter().map(|item| (item.id, item)).collect());
        }
        // The broadcast can run while no client listens, so a connect reads the current drain state.
        if let Ok(waiting) = drain_state().await {
            state.drain.set(waiting);
        }
        if let Ok(error) = upgrade_error_state().await {
            state.upgrade_error.set(error.unwrap_or_default());
        }
        match live(after).await {
            Ok(mut events) => {
                let mut wake = document::eval(LIVE_WAKE);
                if !first {
                    state.reconnects += 1;
                    workstream_list.restart();
                    state.leads.write().clear();
                    state.agents.write().clear();
                }
                // The server subscribes before `live` returns, so a count read now misses no later event.
                if let Ok(counts) = unread().await {
                    state.unread.set(
                        counts
                            .into_iter()
                            .map(|unread| {
                                (
                                    (unread.organization, unread.repository, unread.workstream),
                                    unread.count,
                                )
                            })
                            .collect(),
                    );
                }
                state.unread_read.set(true);
                while let Some(Ok(event)) = next_event(&mut events, &mut wake).await {
                    match event {
                        Live::Feed(row) => {
                            after = Some(row.id);
                            let known = match &*workstream_list.peek() {
                                Some(Ok(list)) => list.iter().any(|workstream| {
                                    workstream.repository == row.repository
                                        && workstream.number == row.workstream
                                }),
                                _ => false,
                            };
                            if !known && !workstream_list.pending() {
                                workstream_list.restart();
                            }
                            state.feed.write().push(row);
                        }
                        Live::Message(message) => upsert(&mut state.messages.write(), message),
                        Live::Lead {
                            organization,
                            repository,
                            workstream,
                            writing,
                            error,
                        } => {
                            state.leads.write().insert(
                                (organization, repository, workstream),
                                LeadState { writing, error },
                            );
                        }
                        Live::Unread(unread) => {
                            state.unread.write().insert(
                                (unread.organization, unread.repository, unread.workstream),
                                unread.count,
                            );
                        }
                        Live::Agent(node) => {
                            state.agents.write().insert(node.session.id, node);
                        }
                        Live::Inbox(item) if item.dismissed_at.is_some() => {
                            state.inbox.write().remove(&item.id);
                        }
                        Live::Inbox(item) => {
                            state.inbox.write().insert(item.id, item);
                        }
                        Live::Workstreams | Live::WorkstreamCreated { .. } => {
                            workstream_list.restart();
                        }
                        Live::Drain { waiting } => state.drain.set(waiting),
                        Live::UpgradeError(error) => {
                            state.upgrade_error.set(error.unwrap_or_default());
                        }
                    }
                }
                document::eval(LIVE_WAKE_STOP);
            }
            Err(error) if unauthorized(&error) => {
                login_shown.set(true);
                return;
            }
            Err(_) => {}
        }
        // The web build has no timer crate, so a JavaScript timer makes the delay.
        let _ = document::eval("await new Promise(resolve => setTimeout(resolve, 1000));").await;
        first = false;
    }
}

#[component]
fn Frame() -> Element {
    let LoginShown(mut login_shown) = use_context();
    let app_slugs = use_resource(github_apps);
    use_context_provider(|| AppSlugs(app_slugs));
    let workstream_list = use_resource(workstreams);
    use_context_provider(|| Workstreams(workstream_list));
    // The poll finds a new organization when it finds its repositories, and the Workstream list then loads again.
    let organization_list = use_resource(move || async move {
        workstream_list.read();
        organizations().await
    });
    use_context_provider(|| Organizations(organization_list));
    let mut new_release = use_resource(release);
    let mut release_version = use_signal(|| None::<String>);
    use_effect(move || {
        // A failed check leaves the version of the last good check.
        if let Some(Ok(version)) = &*new_release.read() {
            release_version.set(version.clone());
        }
    });
    use_effect(move || {
        spawn(async move {
            loop {
                let _ = document::eval(
                    "await new Promise(resolve => setTimeout(resolve, 60 * 60 * 1000));",
                )
                .await;
                new_release.restart();
            }
        });
    });
    let mut upgrading = use_signal(|| false);
    let mut changes_shown = use_signal(|| false);
    let Organization(organization) =
        use_context_provider(|| Organization(Signal::new(String::new())));
    let LocalOffset(mut local_offset) =
        use_context_provider(|| LocalOffset(Signal::new(UtcOffset::UTC)));
    use_hook(move || {
        spawn(async move {
            // `getTimezoneOffset` gives the minutes from the local time to UTC, so the sign is the reverse of `UtcOffset`.
            let minutes: i32 = document::eval("return new Date().getTimezoneOffset();")
                .join()
                .await
                .unwrap_or_default();
            if let Ok(offset) = UtcOffset::from_whole_seconds(-minutes * 60) {
                local_offset.set(offset);
            }
        })
    });
    let state = use_context_provider(|| LiveState {
        feed: Signal::new(Vec::new()),
        messages: Signal::new(Vec::new()),
        leads: Signal::new(HashMap::new()),
        unread: Signal::new(HashMap::new()),
        unread_read: Signal::new(false),
        reconnects: Signal::new(0),
        agents: Signal::new(HashMap::new()),
        inbox: Signal::new(HashMap::new()),
        drain: Signal::new(None),
        upgrade_error: Signal::new(String::new()),
    });
    let mut upgrade_error = state.upgrade_error;
    use_effect(move || {
        if let Some(Err(error)) = &*app_slugs.read()
            && unauthorized(error)
        {
            login_shown.set(true);
        }
    });
    use_effect(move || {
        let Some(Ok(list)) = organization_list() else {
            return;
        };
        if list.contains(&organization.peek()) {
            return;
        }
        spawn(async move {
            let saved: String = document::eval(
                "try { return localStorage.getItem('organization') ?? ''; } catch { return ''; }",
            )
            .join()
            .await
            .unwrap_or_default();
            // A Chat page can select the organization of its route during the read.
            if list.contains(&organization.peek()) {
                return;
            }
            let name = if list.contains(&saved) {
                saved
            } else {
                list.first().cloned().unwrap_or_default()
            };
            select_organization(organization, name);
        });
    });
    use_effect(move || {
        spawn(follow_live(state, workstream_list, login_shown));
    });
    let mut new_build = use_signal(|| false);
    use_effect(move || {
        spawn(async move {
            let mut poll = document::eval(VERSION_POLL);
            while let Ok(build) = poll.recv::<String>().await {
                if build.trim() != BUILD {
                    new_build.set(true);
                }
            }
        });
    });
    let inbox_count = state
        .inbox
        .read()
        .values()
        .filter(|item| item.organization == organization())
        .count();
    let drain_waiting = *state.drain.read();
    let switch = switchable();
    let on_workstreams = matches!(
        use_route::<Route>(),
        Route::WorkstreamList {} | Route::Chat { .. } | Route::NewWorkstream {}
    );
    match &*app_slugs.read() {
        Some(Ok(slugs)) if !slugs.is_empty() => rsx! {
            div { class: "shell",
                nav { class: "rail",
                    if switch {
                        OrganizationSwitch {}
                    } else {
                        div { class: "brand", "Mobius" }
                    }
                    Link { class: "entry", active_class: "sel", to: Route::Inbox {},
                        span { class: "grow", "Inbox" }
                        if inbox_count > 0 {
                            span { class: "count", "{inbox_count}" }
                        }
                    }
                    Link { class: "navbtn", active_class: "sel", to: Route::Activity {}, "Activity" }
                    div { class: "label section", "Workstreams" }
                    WorkstreamEntries {}
                    Link { class: "navbtn", active_class: "sel", to: Route::NewWorkstream {}, "+ New Workstream" }
                    div { class: "grow" }
                    if let Some(waiting) = drain_waiting {
                        div { class: "entry",
                            span { class: "dot queued" }
                            span { class: "grow muted small", DrainText { waiting } }
                        }
                    }
                    if let Some(version) = release_version() {
                        if drain_waiting.is_some() {
                            button { class: "entry upd",
                                onclick: move |_| async move {
                                    if let Err(failure) = drain_cancel().await {
                                        upgrade_error.set(error_text(&failure));
                                    }
                                },
                                span { class: "grow", "Cancel upgrade" }
                            }
                        } else {
                            button { class: "entry upd",
                                disabled: upgrading(),
                                onclick: move |_| changes_shown.set(true),
                                span { class: "grow", "Upgrade" }
                                span { class: "muted", "{version}" }
                            }
                        }
                        if !upgrade_error().is_empty() {
                            p { class: "error note", "{upgrade_error}" }
                        }
                    }
                    if new_build() {
                        UpdateNote { class: "navbtn upd" }
                    }
                    Link { class: "navbtn", active_class: "sel", to: Route::GitHub {}, "GitHub" }
                    Link { class: "navbtn", active_class: "sel", to: Route::Devices {}, "Devices" }
                    Link { class: "navbtn", active_class: "sel", to: Route::Checkup {}, "Checkup" }
                    Link { class: "navbtn", active_class: "sel", to: Route::AgentsPage {}, "Agents" }
                }
                // The rail hides on a phone, so the upgrade line repeats above the page.
                if let Some(waiting) = drain_waiting {
                    div { class: "upd phone", DrainText { waiting } }
                }
                if let Some(version) = release_version() {
                    if changes_shown() {
                        div { class: "backdrop dim", onclick: move |_| changes_shown.set(false) }
                        div { class: "modal",
                            div { class: "head",
                                h2 { "Upgrade to {version}" }
                                button { class: "btn ghost", onclick: move |_| changes_shown.set(false), "Close" }
                            }
                            ReleaseChanges { version: version.clone() }
                            div { class: "actions",
                                button { class: "btn primary",
                                    disabled: upgrading(),
                                    onclick: move |_| async move {
                                        changes_shown.set(false);
                                        upgrading.set(true);
                                        upgrade_error.set(String::new());
                                        match upgrade().await {
                                            Ok(DrainEnd::Drained) => {
                                                document::eval(&reload_on_new_build());
                                                return;
                                            }
                                            Ok(DrainEnd::Cancelled) => {}
                                            Err(failure) => upgrade_error.set(error_text(&failure)),
                                        }
                                        upgrading.set(false);
                                    },
                                    "Upgrade"
                                }
                            }
                        }
                    }
                    if drain_waiting.is_some() {
                        button { class: "upd phone",
                            onclick: move |_| async move {
                                if let Err(failure) = drain_cancel().await {
                                    upgrade_error.set(error_text(&failure));
                                }
                            },
                            "Cancel upgrade"
                        }
                    } else {
                        button { class: "upd phone",
                            disabled: upgrading(),
                            onclick: move |_| changes_shown.set(true),
                            "Upgrade {version}"
                        }
                    }
                    if !upgrade_error().is_empty() {
                        p { class: "error note phone", "{upgrade_error}" }
                    }
                }
                // The rail hides on a phone, so the note repeats above the page.
                if new_build() {
                    UpdateNote { class: "upd phone" }
                }
                main { class: "center", Outlet::<Route> {} }
                nav { class: "tabs",
                    Link { class: if on_workstreams { "on" } else { "" }, to: Route::WorkstreamList {},
                        span { class: "glyph", "◎" }
                        "Workstreams"
                    }
                    Link { active_class: "on", to: Route::Inbox {},
                        span { class: "glyph", "▤" }
                        span {
                            "Inbox"
                            if inbox_count > 0 {
                                " "
                                span { class: "count", "{inbox_count}" }
                            }
                        }
                    }
                    Link { active_class: "on", to: Route::Activity {},
                        span { class: "glyph", "≡" }
                        "Activity"
                    }
                    Link { active_class: "on", to: Route::Settings {},
                        // U+FE0E selects the text form of the gear, not the emoji.
                        span { class: "glyph", "\u{2699}\u{fe0e}" }
                        "Settings"
                    }
                }
            }
        },
        Some(Ok(_)) => rsx! { main { class: "center", GitHub {} } },
        Some(Err(error)) => rsx! { p { class: "error note", {error_text(error)} } },
        None => rsx! {},
    }
}

#[component]
fn DrainText(waiting: usize) -> Element {
    rsx! {
        if waiting == 0 {
            "Upgrade is ready to restart"
        } else {
            "Upgrade waits for {waiting} agents"
        }
    }
}

#[component]
fn ReleaseChanges(version: String) -> Element {
    let changes = use_resource(use_reactive(&version, |version| async move {
        release_changes(version).await
    }));
    match &*changes.read() {
        Some(Ok(titles)) => rsx! {
            ul { class: "changes",
                for (index, title) in titles.iter().enumerate() {
                    li { key: "{index}", "{title}" }
                }
            }
        },
        Some(Err(error)) => rsx! { p { class: "error note", {error_text(error)} } },
        None => rsx! { p { class: "muted note", "Loading changes…" } },
    }
}

// The note for a new version of the web UI on the server. A click reloads the app.
#[component]
fn UpdateNote(class: &'static str) -> Element {
    rsx! {
        button {
            class,
            onclick: move |_| {
                document::eval("location.reload();");
            },
            "New version"
        }
    }
}

#[component]
fn OrganizationSwitch() -> Element {
    let Organizations(organization_list) = use_context();
    let Organization(organization) = use_context();
    let state: LiveState = use_context();
    let mut open = use_signal(|| false);
    let list = match &*organization_list.read() {
        Some(Ok(list)) => list.clone(),
        _ => Vec::new(),
    };
    let mut work: HashMap<String, i64> = HashMap::new();
    for item in state.inbox.read().values() {
        *work.entry(item.organization.clone()).or_default() += 1;
    }
    for ((organization, _, _), count) in state.unread.read().iter() {
        *work.entry(organization.clone()).or_default() += count;
    }
    let selected = organization();
    let elsewhere = work
        .iter()
        .any(|(name, count)| *name != selected && *count > 0);
    rsx! {
        div { class: "orgs",
            button { class: "switch", onclick: move |_| open.toggle(),
                span { class: "ellip", "{selected}" }
                if elsewhere {
                    span { class: "dot live" }
                }
                span { class: "muted", "▾" }
            }
            if open() {
                div { class: "backdrop", onclick: move |_| open.set(false) }
                div { class: "orgmenu",
                    div { class: "label", "Organizations" }
                    for name in list {
                        button {
                            key: "{name}",
                            class: if name == selected { "entry sel" } else { "entry" },
                            onclick: {
                                let name = name.clone();
                                move |_| {
                                    select_organization(organization, name.clone());
                                    open.set(false);
                                }
                            },
                            span { class: "grow", "{name}" }
                            if name != selected && work.get(&name).is_some_and(|count| *count > 0) {
                                span { class: "count", "{work[&name]}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn WorkstreamEntries() -> Element {
    let Workstreams(workstream_list) = use_context();
    let Organization(organization) = use_context();
    let needs_human = use_resource(move || async move {
        workstream_list.read();
        needs_human_workstreams().await
    });
    match &*workstream_list.read() {
        Some(Ok(list)) => rsx! {
            for workstream in list.iter().filter(|workstream| mobius_domain::organization(&workstream.repository) == organization()).cloned() {
                WorkstreamEntry {
                    key: "{workstream.repository}#{workstream.number}",
                    needs_human: matches!(
                        &*needs_human.read(),
                        Some(Ok(keys)) if keys.contains(&(workstream.repository.clone(), workstream.number))
                    ),
                    workstream,
                }
            }
        },
        Some(Err(error)) => rsx! { div { class: "error entry", {error_text(error)} } },
        None => rsx! {},
    }
}

#[component]
fn WorkstreamEntry(workstream: Workstream, needs_human: bool) -> Element {
    let state: LiveState = use_context();
    let (owner, repo) = workstream.repository.split_once('/').unwrap_or_default();
    let unread = state
        .unread
        .read()
        .get(&(
            owner.to_string(),
            workstream.repository.clone(),
            workstream.number,
        ))
        .copied()
        .unwrap_or(0);
    rsx! {
        Link {
            class: if needs_human { "entry hinted" } else { "entry" },
            active_class: "sel",
            to: Route::Chat { owner: owner.to_string(), repo: repo.to_string(), number: workstream.number },
            span { class: "grow", "{workstream.title}" }
            span { class: "muted small", "#{workstream.number}" }
            if workstream.all_tasks_closed {
                span { class: "chip plain", "done" }
            }
            if needs_human {
                span { class: "chip warn", "needs you" }
            }
            if unread > 0 {
                span { class: "count", "{unread}" }
            }
        }
    }
}

#[component]
fn WorkstreamList() -> Element {
    rsx! {
        div { class: "head",
            if switchable() {
                div { class: "phone grow", OrganizationSwitch {} }
                h2 { class: "desktop grow", "Workstreams" }
            } else {
                h2 { class: "grow", "Workstreams" }
            }
            Link { class: "btn primary", to: Route::NewWorkstream {}, "+ New" }
        }
        div { class: "list",
            WorkstreamEntries {}
        }
    }
}

#[component]
fn Settings() -> Element {
    rsx! {
        div { class: "head", h2 { "Settings" } }
        div { class: "list",
            Link { class: "entry", to: Route::GitHub {},
                span { class: "grow", "GitHub" }
            }
            Link { class: "entry", to: Route::Devices {},
                span { class: "grow", "Devices" }
            }
            Link { class: "entry", to: Route::Checkup {},
                span { class: "grow", "Checkup" }
            }
            Link { class: "entry", to: Route::AgentsPage {},
                span { class: "grow", "Agents" }
            }
        }
    }
}

// The text of the button that fixes the labels of the shown repositories, or `None` if no label is missing or has a wrong color.
pub fn fix_button(repositories: &[RepositoryCheckup]) -> Option<&'static str> {
    if !repositories
        .iter()
        .any(|repository| repository.labels.iter().any(|label| label.status.fixable()))
    {
        return None;
    }
    let all_missing = repositories.iter().all(|repository| {
        repository
            .labels
            .iter()
            .all(|label| label.status == LabelStatus::Missing)
    });
    Some(if all_missing {
        "Create labels"
    } else {
        "Fix labels"
    })
}

fn label_status(status: &LabelStatus) -> Element {
    match status {
        LabelStatus::Present => rsx! { span { class: "chip plain", "present" } },
        LabelStatus::WrongColor(color) => rsx! {
            span { class: "chip warn", "wrong color: #{color}" }
        },
        // Mobius does not rename labels, so a human fixes the name.
        LabelStatus::WrongCase(name) => rsx! {
            span { class: "chip warn", "wrong case: {name}" }
        },
        LabelStatus::Missing => rsx! { span { class: "chip warn", "missing" } },
    }
}

fn permission_status(status: &PermissionStatus) -> Element {
    match status {
        PermissionStatus::Present => rsx! { span { class: "chip plain", "present" } },
        PermissionStatus::NotAccepted(url) => rsx! {
            a { class: "chip warn", href: "{url}", target: "_blank", "not accepted: accept on GitHub" }
        },
        PermissionStatus::Missing(url) => rsx! {
            a { class: "chip warn", href: "{url}", target: "_blank", "missing: add on GitHub" }
        },
    }
}

#[component]
fn Checkup() -> Element {
    let Organization(organization) = use_context();
    let LoginShown(mut login_shown) = use_context();
    let mut error = use_signal(String::new);
    let mut fixing = use_signal(|| false);
    let mut resource = use_resource(use_reactive(&organization(), |organization| async move {
        checkup(Some(organization)).await
    }));
    use_effect(move || {
        if let Some(Err(error)) = &*resource.read()
            && unauthorized(error)
        {
            login_shown.set(true);
        }
    });
    let button = match &*resource.read() {
        Some(Ok(view)) => fix_button(&view.repositories),
        _ => None,
    };
    rsx! {
        div { class: "head",
            h2 { class: "grow", "Checkup" }
            if let Some(text) = button {
                button {
                    class: "btn primary",
                    disabled: fixing(),
                    onclick: move |_| async move {
                        fixing.set(true);
                        match fix_labels(organization()).await {
                            Ok(()) => error.set(String::new()),
                            Err(failure) => error.set(error_text(&failure)),
                        }
                        // The fix can change labels before it fails: load the status again.
                        resource.restart();
                        fixing.set(false);
                    },
                    "{text}"
                }
            }
        }
        div { class: "error note", {error} }
        match &*resource.read() {
            Some(Ok(view)) => rsx! {
                if view.repositories.is_empty() {
                    p { class: "muted small note", "The Mobius App has no repository in this organization." }
                }
                div { class: "checkup",
                    match &view.permissions {
                        Ok(permissions) if permissions.is_empty() => rsx! {},
                        Ok(permissions) => rsx! {
                            div { class: "label section", "App permissions" }
                            div { class: "list",
                                for permission in permissions {
                                    div { key: "{permission.name}", class: "item",
                                        span { class: "grow", "{permission.name}: {permission.level}" }
                                        {permission_status(&permission.status)}
                                    }
                                }
                            }
                        },
                        Err(message) => rsx! {
                            div { class: "label section", "App permissions" }
                            p { class: "error note", "{message}" }
                        },
                    }
                    for repository in &view.repositories {
                        div { key: "{repository.repository}", class: "label section", "{repository.repository}" }
                        div { class: "list",
                            for label in &repository.labels {
                                div { key: "{label.name}", class: "item",
                                    span { class: "dot", style: "background: #{label.color};" }
                                    span { class: "grow", "{label.name}" }
                                    {label_status(&label.status)}
                                }
                            }
                        }
                    }
                }
            },
            Some(Err(failure)) => rsx! { p { class: "error note", {error_text(failure)} } },
            None => rsx! {},
        }
    }
}

#[component]
fn NewWorkstream() -> Element {
    let Organizations(organization_list) = use_context();
    let Organization(organization) = use_context();
    if !matches!(&*organization_list.read(), Some(Ok(list)) if list.contains(&organization())) {
        return rsx! {
            div { class: "head", h2 { "New Workstream" } }
            p { class: "muted note", "Mobius reads the repositories from GitHub. The Triager chat opens after this step." }
        };
    }
    rsx! {
        div { class: "page",
            Conversation {
                organization: organization(),
                repository: String::new(),
                number: 0,
                agent: "Triager",
                brief: None,
                head: rsx! { h2 { class: "ellip", "New Workstream" } },
                tail: rsx! {},
                note: rsx! {},
            }
        }
    }
}

// All active agents of the server in one group for each role, of all organizations.
// The resource runs again after a new live connection and when a `Live::Agent` event changes
// `LiveState.agents`: at the start of a session, at the slot start, at a change of the queue reason, when the pull request opens, and at the end.
#[component]
fn AgentsPage() -> Element {
    let state: LiveState = use_context();
    let LoginShown(mut login_shown) = use_context();
    let resource = use_resource(move || async move {
        state.reconnects.read();
        state.agents.read();
        active_agents().await
    });
    use_effect(move || {
        if let Some(Err(error)) = &*resource.read()
            && unauthorized(error)
        {
            login_shown.set(true);
        }
    });
    let body = match &*resource.read() {
        Some(Ok(overview)) => rsx! {
            for group in overview.groups.iter() {
                div { key: "{group.name}", class: "label section", "{group.name} {group.count} / {group.max}" }
                div { class: "list",
                    for agent in group.agents.iter() {
                        AgentRow { key: "{agent.node.session.id}", agent: agent.clone() }
                    }
                }
            }
        },
        Some(Err(error)) => rsx! { p { class: "error note", {error_text(error)} } },
        None => rsx! {},
    };
    rsx! {
        div { class: "head",
            h2 { class: "grow", "Agents" }
            if let Some(Ok(overview)) = &*resource.read() {
                span { class: "muted", "{overview.count} / {overview.max}" }
            }
        }
        {body}
    }
}

#[component]
fn AgentRow(agent: ActiveAgent) -> Element {
    let node = &agent.node;
    let session = &node.session;
    let numbered = |number: i64, title: &Option<String>| match title {
        Some(title) => format!("#{number} {title}"),
        None => format!("#{number}"),
    };
    rsx! {
        div { class: "item",
            span { class: if session.queue_reason.is_some() { "dot queued" } else { "dot live" } }
            div { class: "grow",
                div { "{node.role} {node.title}" }
                div { class: "muted small",
                    "{session.role} · {session.organization}"
                    if session.workstream != 0 || session.issue.is_some() {
                        " · {session.repository}"
                    }
                    if let Some(reason) = &session.queue_reason {
                        " · {reason}"
                    }
                }
                if session.workstream != 0 {
                    div { class: "muted small ellip", "Workstream {numbered(session.workstream, &agent.workstream_title)}" }
                }
                if let Some(issue) = session.issue {
                    div { class: "muted small ellip", "Ticket {numbered(issue, &agent.issue_title)}" }
                }
                if let Some(pull_request) = agent.pull_request {
                    div { class: "muted small", "Pull request #{pull_request}" }
                }
            }
            if let Some(reason) = &session.queue_reason {
                span { class: "chip warn",
                    if reason.starts_with(PAUSED) { "paused" } else { "queued" }
                }
            }
        }
    }
}

#[component]
fn Inbox() -> Element {
    let LocalOffset(local_offset) = use_context();
    let Workstreams(workstream_list) = use_context();
    let LiveState { inbox, .. } = use_context();
    let Organization(organization) = use_context();
    let mut error = use_signal(String::new);
    let mut items: Vec<InboxItem> = inbox
        .read()
        .values()
        .filter(|item| item.organization == organization())
        .cloned()
        .collect();
    items.sort_by_key(|item| Reverse(item.id));
    let titles: HashMap<(String, i64), String> = match &*workstream_list.read() {
        Some(Ok(list)) => list
            .iter()
            .map(|workstream| {
                (
                    (workstream.repository.clone(), workstream.number),
                    workstream.title.clone(),
                )
            })
            .collect(),
        _ => HashMap::new(),
    };
    rsx! {
        div { class: "head", h2 { "Inbox" } }
        div { class: "error note", {error} }
        div { class: "list",
            if items.is_empty() {
                div { class: "muted small note", "Nothing waits for you." }
            }
            for item in items {
                div { key: "{item.id}", class: "item",
                    span { class: "chip", {item.kind.name()} }
                    div { class: "grow",
                        div { "{item.text}" }
                        div { class: "muted small",
                            {titles.get(&(item.repository.clone(), item.workstream)).cloned().unwrap_or_else(|| format!("#{}", item.workstream))}
                            " · #{item.issue} · "
                            {item.time.to_offset(local_offset()).format(format_description!("[month]-[day] [hour]:[minute]")).unwrap_or_default()}
                        }
                    }
                    div { class: "actions",
                        if item.kind == InboxKind::UsageLimit {
                            button {
                                class: "btn primary",
                                onclick: move |_| async move {
                                    match inbox_resume(item.id).await {
                                        Ok(()) => error.set(String::new()),
                                        Err(failure) => error.set(error_text(&failure)),
                                    }
                                },
                                "Resume now"
                            }
                        } else {
                            a { href: "{item.link}", target: "_blank", "Open on GitHub" }
                        }
                        button {
                            class: "btn",
                            onclick: move |_| async move {
                                match inbox_dismiss(item.id).await {
                                    Ok(()) => error.set(String::new()),
                                    Err(failure) => error.set(error_text(&failure)),
                                }
                            },
                            "Dismiss"
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn Activity() -> Element {
    let LocalOffset(local_offset) = use_context();
    let Workstreams(workstream_list) = use_context();
    let LiveState { feed, .. } = use_context();
    let Organization(organization) = use_context();
    let mut selected = use_signal(|| None::<(String, i64)>);
    let chips: Vec<Workstream> = match &*workstream_list.read() {
        Some(Ok(list)) => list
            .iter()
            .filter(|workstream| {
                mobius_domain::organization(&workstream.repository) == organization()
            })
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    let rows: Vec<FeedRow> = feed
        .read()
        .iter()
        .rev()
        .filter(|row| mobius_domain::organization(&row.repository) == organization())
        .filter(|row| {
            selected.read().as_ref().is_none_or(|(repository, number)| {
                row.repository == *repository && row.workstream == *number
            })
        })
        .cloned()
        .collect();
    rsx! {
        div { class: "head", h2 { "Activity" } }
        div { class: "chips",
            button {
                class: if selected.read().is_none() { "chip on" } else { "chip" },
                onclick: move |_| selected.set(None),
                "All"
            }
            for workstream in chips {
                button {
                    key: "{workstream.repository}#{workstream.number}",
                    class: if selected.read().as_ref() == Some(&(workstream.repository.clone(), workstream.number)) { "chip on" } else { "chip" },
                    onclick: move |_| selected.set(Some((workstream.repository.clone(), workstream.number))),
                    "{workstream.title}"
                }
            }
        }
        div { class: "list",
            for row in rows {
                div { key: "{row.id}", class: "item",
                    span { class: "muted small",
                        {row.time.to_offset(local_offset()).format(format_description!("[month]-[day] [hour]:[minute]")).unwrap_or_default()}
                    }
                    span { class: "grow", "@{row.actor} {row.text}" }
                    a { href: "{row.link}", target: "_blank", "#{row.issue}" }
                }
            }
        }
    }
}

#[component]
fn Chat(owner: String, repo: String, number: i64) -> Element {
    let repository = format!("{owner}/{repo}");
    let Workstreams(mut workstream_list) = use_context();
    let Organization(organization) = use_context();
    use_effect(use_reactive((&owner,), move |(owner,)| {
        select_organization(organization, owner)
    }));
    let mut sheet = use_signal(|| false);
    // The router keeps this component when the Workstream changes. The call and the
    // error are keyed by the Workstream, so a late answer cannot touch another chat.
    let mut autopilot_call = use_signal(|| None::<(String, i64)>);
    let mut autopilot_error = use_signal(|| None::<((String, i64), String)>);
    let mut close_call = use_signal(|| None::<(String, i64)>);
    let mut close_error = use_signal(|| None::<((String, i64), String)>);
    let workstream = match &*workstream_list.read() {
        Some(Ok(list)) => list
            .iter()
            .find(|workstream| workstream.repository == repository && workstream.number == number)
            .cloned(),
        _ => None,
    };
    let autopilot_on = workstream
        .as_ref()
        .is_some_and(|workstream| workstream.autopilot);
    let autopilot_busy = autopilot_call().is_some_and(|key| key.0 == repository && key.1 == number);
    let autopilot_note = autopilot_error()
        .and_then(|(key, text)| (key.0 == repository && key.1 == number).then_some(text));
    let all_tasks_closed = workstream
        .as_ref()
        .is_some_and(|workstream| workstream.all_tasks_closed);
    let close_busy = close_call().is_some_and(|key| key.0 == repository && key.1 == number);
    let close_note = close_error()
        .and_then(|(key, text)| (key.0 == repository && key.1 == number).then_some(text));
    let close_repository = repository.clone();
    let here = Route::Chat {
        owner: owner.clone(),
        repo: repo.clone(),
        number,
    };
    let switch_repository = repository.clone();
    rsx! {
        div { class: "page",
            Conversation {
                organization: owner.clone(),
                repository: repository.clone(),
                number,
                agent: "Lead",
                brief: workstream.clone(),
                head: rsx! {
                    h2 { class: "ellip", {workstream.as_ref().map(|workstream| workstream.title.clone())} }
                    span { class: "num", "#{number}" }
                    button {
                        class: "autopilot",
                        role: "switch",
                        aria_checked: autopilot_on,
                        disabled: autopilot_busy,
                        onclick: move |_| {
                            let repository = switch_repository.clone();
                            async move {
                                let key = (repository.clone(), number);
                                autopilot_call.set(Some(key.clone()));
                                match workstream_autopilot(repository, number, !autopilot_on).await {
                                    Ok(()) => {
                                        if autopilot_error().is_some_and(|(other, _)| other == key) {
                                            autopilot_error.set(None);
                                        }
                                    }
                                    Err(failure) => {
                                        autopilot_error
                                            .set(Some((key.clone(), error_text(&failure))))
                                    }
                                }
                                workstream_list.restart();
                                if autopilot_call() == Some(key) {
                                    autopilot_call.set(None);
                                }
                            }
                        },
                        span { class: "track", span { class: "knob" } }
                        "Autopilot"
                    }
                },
                note: rsx! {
                    if let Some(note) = autopilot_note {
                        div { class: "error note", {note} }
                    }
                    if all_tasks_closed {
                        div { class: "note closing",
                            span { "All tasks are closed." }
                            button {
                                class: "btn primary",
                                disabled: close_busy,
                                onclick: move |_| {
                                    let repository = close_repository.clone();
                                    let here = here.clone();
                                    async move {
                                        let key = (repository.clone(), number);
                                        close_error.set(None);
                                        close_call.set(Some(key.clone()));
                                        match workstream_close(repository, number).await {
                                            Ok(()) => {
                                                if dioxus::router::router().current::<Route>() == here {
                                                    navigator().replace(Route::WorkstreamList {});
                                                }
                                            }
                                            Err(failure) => {
                                                close_error
                                                    .set(Some((key.clone(), error_text(&failure))))
                                            }
                                        }
                                        workstream_list.restart();
                                        if close_call() == Some(key) {
                                            close_call.set(None);
                                        }
                                    }
                                },
                                "Close Workstream"
                            }
                            if let Some(note) = close_note {
                                span { class: "error", {note} }
                            }
                        }
                    }
                },
                tail: rsx! {
                    button { class: "btn phone", onclick: move |_| sheet.set(true), "Agents" }
                },
            }
            aside { class: "side",
                Agents { repository: repository.clone(), number }
            }
            if sheet() {
                div { class: "backdrop", onclick: move |_| sheet.set(false) }
                div { class: "sheet",
                    Agents { repository: repository.clone(), number, on_close: move |_| sheet.set(false) }
                }
            }
        }
    }
}

// The messages of the voice input: "started", "text" with the transcript, "error" with the code,
// "stopping" when a tap only asks the live session to stop, and "end" when the session ends.
const MIC_SCRIPT: &str = r#"
const Recognition = window.SpeechRecognition || window.webkitSpeechRecognition;
if (!Recognition) {
    dioxus.send({ type: "error", value: "unsupported" });
    dioxus.send({ type: "end" });
} else if (window.__mobiusMic) {
    const mic = window.__mobiusMic;
    try {
        mic.stop();
        dioxus.send({ type: "stopping" });
    } catch {
        mic.onend = mic.onresult = mic.onerror = null;
        window.__mobiusMic = null;
        dioxus.send({ type: "end" });
    }
} else {
    const recognition = new Recognition();
    window.__mobiusMic = recognition;
    recognition.lang = "en-US";
    recognition.continuous = false;
    recognition.interimResults = false;
    recognition.onresult = (event) => {
        const parts = [];
        for (const result of event.results) {
            parts.push(result[0].transcript);
        }
        dioxus.send({ type: "text", value: parts.join(" ") });
    };
    recognition.onerror = (event) => {
        if (event.error !== "aborted") {
            dioxus.send({ type: "error", value: event.error || "unknown" });
        }
    };
    recognition.onend = () => {
        if (window.__mobiusMic === recognition) {
            window.__mobiusMic = null;
            dioxus.send({ type: "end" });
        }
    };
    try {
        recognition.start();
        dioxus.send({ type: "started" });
    } catch {
        window.__mobiusMic = null;
        dioxus.send({ type: "error", value: "start" });
        dioxus.send({ type: "end" });
    }
}
"#;

fn mic_error(code: Option<&str>) -> String {
    match code {
        Some("unsupported") => "Voice input is not supported in this browser.".to_string(),
        Some("not-allowed") | Some("service-not-allowed") => {
            "Microphone access is denied. Allow the microphone in the browser settings.".to_string()
        }
        Some(code) => format!("Voice input failed: {code}"),
        None => "Voice input failed.".to_string(),
    }
}

#[component]
fn Conversation(
    organization: String,
    repository: String,
    number: i64,
    agent: &'static str,
    brief: Option<Workstream>,
    head: Element,
    tail: Element,
    note: Element,
) -> Element {
    let key = (organization.clone(), repository.clone(), number);
    let state: LiveState = use_context();
    let LocalOffset(local_offset) = use_context();
    // The resource keeps the result of the old chat until the result of the new chat
    // arrives, so this key names the chat that the result belongs to.
    let mut loaded_chat = use_signal(|| None::<ChatKey>);
    let reconnects = state.reconnects;
    let history = use_resource(use_reactive(
        (&organization, &repository, &number),
        move |(organization, repository, number)| async move {
            reconnects.read();
            let view = chat_view(organization.clone(), repository.clone(), number).await;
            loaded_chat.set(Some((organization, repository, number)));
            view
        },
    ));
    let mut text = use_signal(String::new);
    let mut send_error = use_signal(String::new);
    let mut sending = use_signal(|| false);
    let mut mic_active = use_signal(|| false);
    let mut brief_open = use_signal(|| None::<bool>);
    let mut touch = use_signal(|| true);
    use_hook(move || {
        spawn(async move {
            let coarse: bool =
                document::eval("return window.matchMedia('(pointer: coarse)').matches;")
                    .join()
                    .await
                    .unwrap_or(true);
            touch.set(coarse);
        })
    });
    // The route keeps this scope when it moves to another Workstream, so the
    // choice of the Owner resets and the default of the new Workstream applies.
    use_effect(use_reactive(
        (&organization, &repository, &number),
        move |_| brief_open.set(None),
    ));
    use_drop(|| {
        document::eval("window.__mobiusMic?.stop();");
    });

    let (mut messages, history_writing, harness) = match &*history.read() {
        Some(Ok(view)) => (view.messages.clone(), view.writing, Some(view.lead)),
        _ => (Vec::new(), false, None),
    };
    for message in state.messages.read().iter() {
        if message.organization != organization
            || message.repository != repository
            || message.workstream != number
        {
            continue;
        }
        // The Lead only appends text to a message, so the longer text is the newer text.
        match messages.iter_mut().find(|held| held.id == message.id) {
            Some(held) if message.text.len() > held.text.len() => *held = message.clone(),
            Some(_) => {}
            None => messages.push(message.clone()),
        }
    }
    messages.sort_by_key(|message| message.id);
    // While the history loads, `messages` is still empty, so the default waits for it.
    let open =
        brief_open().unwrap_or(matches!(&*history.read(), Some(Ok(_))) && messages.is_empty());
    let lead_state = state.leads.read().get(&key).cloned().unwrap_or(LeadState {
        writing: history_writing,
        error: None,
    });
    let last_agent_message = messages
        .iter()
        .rev()
        .find(|message| message.author != Author::Owner)
        .map(|message| message.id);
    let unread = state.unread.read().get(&key).copied().unwrap_or(0);
    let unread_messages: Vec<i64> = messages
        .iter()
        .filter(|message| {
            !matches!(
                message.author,
                Author::Owner | Author::Researcher | Author::Event
            )
        })
        .map(|message| message.id)
        .collect();
    // A new last message always scrolls the list to the bottom. A switch to another chat
    // scrolls to the first unread message, or to the bottom without unread messages. The
    // growth of the last message or a new state of the agent scrolls only while the
    // owner is pinned at the bottom, so a reply in parts does not move an owner who
    // scrolled up. The script waits for a frame, so the scroll uses the DOM with the
    // update.
    let message_count = messages.len();
    let last_message = messages
        .last()
        .map(|message| (message.id, message.text.len()));
    let history_error = matches!(&*history.read(), Some(Err(_)));
    let loaded_chat_key = loaded_chat();
    let unread_read = *state.unread_read.read();
    let mut scroll_mark = use_signal(|| (String::new(), String::new(), 0i64, 0usize, 0i64));
    // The chat of the last switch, and its unread count until the switch gets its position.
    let mut opened = use_signal(|| (ChatKey::default(), None::<i64>));
    use_effect(use_reactive(
        (
            &organization,
            &repository,
            &number,
            &message_count,
            &last_message,
            &(lead_state.clone(), history_error),
            &loaded_chat_key,
            &(unread_messages.clone(), unread_read),
        ),
        move |(
            organization,
            repository,
            number,
            message_count,
            last_message,
            _,
            loaded_chat_key,
            (unread_messages, unread_read),
        )| {
            let key = (organization.clone(), repository.clone(), number);
            let unread = state.unread.peek().get(&key).copied().unwrap_or(0);
            // The count at the switch is kept, because the Owner can mark the chat as seen
            // before the history arrives. The switch waits for the first read of the counts,
            // and the current count covers a count that arrives later.
            if opened.peek().0 != key {
                opened.set((key.clone(), Some(unread)));
            }
            let switched = if unread_read && loaded_chat_key.as_ref() == Some(&key) {
                opened.write().1.take()
            } else {
                None
            };
            let first_unread = switched
                .and_then(|at_switch| {
                    unread_messages
                        .iter()
                        .rev()
                        .take(at_switch.max(unread) as usize)
                        .next_back()
                })
                .copied()
                .unwrap_or(0);
            let switched = switched.is_some();
            let mark = (
                organization,
                repository,
                number,
                message_count,
                last_message.map(|(id, _)| id).unwrap_or(0),
            );
            let force = if *scroll_mark.peek() != mark {
                scroll_mark.set(mark);
                true
            } else {
                false
            };
            // The observer keeps the list at the bottom when it shrinks or grows while the
            // owner is already at the bottom, for example when the keyboard opens.
            document::eval(&format!(
                r#"
                requestAnimationFrame(() => {{
                    const list = document.querySelector(".msgs");
                    if (!list) {{
                        return;
                    }}
                    if (!list.__mobiusScroll) {{
                        const state = {{ pinned: true }};
                        list.__mobiusScroll = state;
                        list.addEventListener("scroll", () => {{
                            state.pinned =
                                list.scrollHeight - list.scrollTop - list.clientHeight < 40;
                        }});
                        state.observer = new ResizeObserver(() => {{
                            if (state.pinned) {{
                                list.scrollTop = list.scrollHeight;
                            }}
                        }});
                        state.observer.observe(list);
                    }}
                    const first = list.querySelector('[data-message="{first_unread}"]');
                    if (first) {{
                        list.scrollTop +=
                            first.getBoundingClientRect().top - list.getBoundingClientRect().top;
                        list.__mobiusScroll.pinned =
                            list.scrollHeight - list.scrollTop - list.clientHeight < 40;
                        return;
                    }}
                    if ({force} || {switched}) {{
                        list.__mobiusScroll.pinned = true;
                    }}
                    if (list.__mobiusScroll.pinned) {{
                        list.scrollTop = list.scrollHeight;
                    }}
                }});
                "#,
            ));
        },
    ));
    use_effect(use_reactive(
        (
            &organization,
            &repository,
            &number,
            &last_agent_message,
            &unread,
        ),
        |(organization, repository, number, last_agent_message, unread)| {
            if unread > 0
                && let Some(message) = last_agent_message
            {
                spawn(async move {
                    // A failed call keeps the count, and the next message of the agent calls again.
                    let _ = chat_seen(organization, repository, number, message).await;
                });
            }
        },
    ));

    use_effect(move || {
        text();
        document::eval(
            r#"
            if (!window.__mobiusFit) {
                window.__mobiusFit = () => {
                    const box = document.querySelector('.composer textarea');
                    if (box) {
                        box.style.height = 'auto';
                        box.style.height =
                            (box.scrollHeight + box.offsetHeight - box.clientHeight) + 'px';
                    }
                };
                window.addEventListener('resize', window.__mobiusFit);
            }
            window.__mobiusFit();
            "#,
        );
    });

    let send_key = (organization.clone(), repository.clone());
    let send = use_callback(move |_: ()| {
        let (organization, repository) = send_key.clone();
        spawn(async move {
            if sending() || text().trim().is_empty() {
                return;
            }
            sending.set(true);
            let sent = text();
            text.set(String::new());
            match chat_send(organization, repository, number, sent.clone()).await {
                Ok(()) => send_error.set(String::new()),
                Err(failure) => {
                    text.set(format!("{sent}{}", text()));
                    send_error.set(error_text(&failure));
                }
            }
            sending.set(false);
        });
    });
    let stop_key = (organization.clone(), repository.clone());
    let lead_chat = brief.is_some();
    rsx! {
        div { class: "column",
            div { class: "head",
                {head}
                span { class: "grow" }
                if let Some(harness) = harness {
                    span { class: "muted small ellip", "{agent}: {harness.name()}" }
                }
                {tail}
            }
            div { class: "chat",
                if let Some(workstream) = brief {
                    div { class: "brief",
                        button {
                            class: "briefhead",
                            onclick: move |_| brief_open.set(Some(!open)),
                            span { class: "grow ellip", "{workstream.title}" }
                            span { class: "muted", if open { "▾" } else { "▸" } }
                        }
                        if open {
                            Markdown { text: workstream.body }
                        }
                    }
                }
                {note}
                div { class: "msgs",
                    if let Some(Err(error)) = &*history.read() {
                        div { class: "error", {error_text(error)} }
                    }
                    if messages.is_empty() {
                        div { class: "muted small empty", "No messages. Write to start a chat session." }
                    }
                    for message in messages {
                        div {
                            key: "{message.id}",
                            "data-message": "{message.id}",
                            class: match message.author {
                                Author::Owner => "msg owner",
                                Author::Event => "msg event",
                                _ => "msg",
                            },
                            div { class: "meta",
                                span {
                                    if message.author == Author::TellOwner {
                                        "Lead"
                                    } else {
                                        {message.author.name()}
                                    }
                                }
                                span {
                                    {message.time.to_offset(local_offset()).format(format_description!("[hour]:[minute]")).unwrap_or_default()}
                                }
                            }
                            if let Some((summary, body)) = fold_event(&message.author, &message.text) {
                                details {
                                    summary { "{summary}" }
                                    Markdown { text: body }
                                }
                            } else {
                                Markdown { text: message.text }
                            }
                        }
                    }
                    if lead_state.writing {
                        div { class: "typing",
                            span { class: "dot live" }
                            "The {agent} writes a reply."
                        }
                    }
                    if let Some(error) = lead_state.error {
                        div { class: "error", "The chat session failed: {error}" }
                    }
                }
                if lead_chat {
                    NeedsHumanList { repository: repository.clone(), number }
                }
                form {
                    class: "composer",
                    onsubmit: move |event: FormEvent| {
                        event.prevent_default();
                        send(());
                    },
                    div { class: "grow",
                        textarea {
                            rows: "1",
                            placeholder: "Write to the {agent}",
                            value: text,
                            oninput: move |event| text.set(event.value()),
                            // Safari reports the Enter that commits an IME candidate with
                            // isComposing false and keyCode 229.
                            onkeydown: move |event| {
                                if event.key() == Key::Enter
                                    && !event.modifiers().shift()
                                    && !event.is_composing()
                                    && !is_ime_key(&event)
                                    && !touch()
                                {
                                    event.prevent_default();
                                    send(());
                                }
                            },
                        }
                        div { class: "error", {send_error} }
                    }
                    if lead_state.writing {
                        button {
                            class: "btn danger",
                            r#type: "button",
                            onmousedown: move |event| event.prevent_default(),
                            onclick: move |_| {
                                let (organization, repository) = stop_key.clone();
                                async move {
                                    if let Err(failure) = chat_stop(organization, repository, number).await {
                                        send_error.set(error_text(&failure));
                                    }
                                }
                            },
                            "Stop"
                        }
                    }
                    button {
                        class: if mic_active() { "btn mic live" } else { "btn mic" },
                        r#type: "button",
                        onclick: move |_| {
                            let mut eval = document::eval(MIC_SCRIPT);
                            spawn(async move {
                                while let Ok(message) = eval.recv::<serde_json::Value>().await {
                                    let Some(kind) =
                                        message.get("type").and_then(|kind| kind.as_str())
                                    else {
                                        break;
                                    };
                                    match kind {
                                        "started" => {
                                            send_error.set(String::new());
                                            mic_active.set(true);
                                        }
                                        "text" => {
                                            if let Some(spoken) = message
                                                .get("value")
                                                .and_then(|value| value.as_str())
                                            {
                                                let mut current = text.peek().clone();
                                                if !current.is_empty() && !current.ends_with(' ') {
                                                    current.push(' ');
                                                }
                                                current.push_str(spoken.trim());
                                                text.set(current);
                                            }
                                        }
                                        "error" => send_error.set(mic_error(
                                            message
                                                .get("value")
                                                .and_then(|value| value.as_str()),
                                        )),
                                        "stopping" => break,
                                        _ => {}
                                    }
                                    if kind == "end" {
                                        mic_active.set(false);
                                        break;
                                    }
                                }
                            });
                        },
                        if mic_active() { "Stop mic" } else { "Mic" }
                    }
                    button {
                        class: "btn primary",
                        r#type: "submit",
                        disabled: sending(),
                        onmousedown: move |event| event.prevent_default(),
                        "Send"
                    }
                }
            }
        }
    }
}

#[component]
fn Agents(repository: String, number: i64, on_close: Option<EventHandler>) -> Element {
    let state: LiveState = use_context();
    let reconnects = state.reconnects;
    let tree = use_resource(use_reactive(
        (&repository, &number),
        move |(repository, number)| async move {
            reconnects.read();
            agent_tree(repository, number).await
        },
    ));
    let mut tasks_tab = use_signal(|| false);
    let mut selected = use_signal(|| None::<i64>);
    let mut show_stopped = use_signal(|| false);

    let mut nodes: HashMap<i64, AgentNode> = match &*tree.read() {
        Some(Ok(list)) => list
            .iter()
            .map(|node| (node.session.id, node.clone()))
            .collect(),
        _ => HashMap::new(),
    };
    for node in state.agents.read().values() {
        if node.session.repository != repository || node.session.workstream != number {
            continue;
        }
        // A session never starts again, so an ended node is newer than a live node.
        let known = nodes.get(&node.session.id);
        if known.is_none_or(|known| known.session.ended_at.is_none()) {
            nodes.insert(node.session.id, node.clone());
        }
    }
    let selected_node = selected().and_then(|id| nodes.get(&id).cloned());
    let rows = agent_rows(shown_agents(nodes.into_values().collect(), show_stopped()));
    let close = on_close.map(|on_close| {
        rsx! {
            button { class: "btn ghost", onclick: move |_| on_close.call(()), "Close" }
        }
    });

    if let Some(node) = selected_node {
        return rsx! {
            div { class: "head",
                button { class: "back", onclick: move |_| selected.set(None), "‹ Agents" }
                h2 { class: "ellip grow", "{node.role} {node.title}" }
                {close}
            }
            Transcript { session: node.session.id }
        };
    }
    rsx! {
        div { class: "sidetabs",
            button { class: if !tasks_tab() { "on" }, onclick: move |_| tasks_tab.set(false), "Agents" }
            button { class: if tasks_tab() { "on" }, onclick: move |_| tasks_tab.set(true), "Tasks" }
            span { class: "grow" }
            {close}
        }
        div { class: "scroll",
            if tasks_tab() {
                Tasks { repository: repository.clone(), number }
            } else {
                if let Some(Err(error)) = &*tree.read() {
                    div { class: "error note", {error_text(error)} }
                }
                div { class: "chips",
                    button {
                        class: if show_stopped() { "chip on" } else { "chip" },
                        aria_pressed: show_stopped(),
                        onclick: move |_| show_stopped.set(!show_stopped()),
                        "Show stopped agents"
                    }
                }
                for (depth, node) in rows.iter().cloned() {
                    AgentEntry {
                        key: "{node.session.id}",
                        node: node.clone(),
                        depth,
                        onclick: move |_| selected.set(Some(node.session.id)),
                    }
                }
            }
        }
    }
}

// A `Live::Workstreams` event reads the list again.
#[component]
fn NeedsHumanList(repository: String, number: i64) -> Element {
    let Workstreams(mut workstream_list) = use_context();
    let mut error = use_signal(String::new);
    let issues = use_resource(use_reactive(
        (&repository, &number),
        move |(repository, number)| async move {
            workstream_list.read();
            needs_human_list(repository, number).await
        },
    ));
    match &*issues.read() {
        None => rsx! {},
        Some(Err(failure)) => rsx! {
            div { class: "error note", {error_text(failure)} }
        },
        Some(Ok(issues)) if issues.is_empty() => rsx! {},
        Some(Ok(issues)) => rsx! {
            div { class: "needs-human",
                if !error().is_empty() {
                    div { class: "error", {error} }
                }
                for issue in issues.iter().cloned() {
                    div { key: "{issue.number}", class: "item",
                        a { class: "grow", href: "{issue.url}", target: "_blank", "#{issue.number} {issue.title}" }
                        if let (Some(number), Some(url)) = (issue.pull_request, issue.pull_request_url) {
                            a { href: "{url}", target: "_blank", "PR #{number}" }
                        }
                        button {
                            class: "btn primary",
                            onclick: {
                                let repository = repository.clone();
                                move |_| {
                                    let repository = repository.clone();
                                    async move {
                                        match task_resume(repository, issue.number).await {
                                            Ok(()) => error.set(String::new()),
                                            Err(failure) => error.set(error_text(&failure)),
                                        }
                                        workstream_list.restart();
                                    }
                                }
                            },
                            "Resume"
                        }
                    }
                }
            }
        },
    }
}

// The tab mounts this component each time it opens, and a `Live::Workstreams` event reads the list again.
#[component]
fn Tasks(repository: String, number: i64) -> Element {
    let Workstreams(workstream_list) = use_context();
    let lines = use_resource(use_reactive(
        (&repository, &number),
        move |(repository, number)| async move {
            workstream_list.read();
            task_list(repository, number).await
        },
    ));
    match &*lines.read() {
        None => rsx! {},
        Some(Err(error)) => rsx! {
            div { class: "error note", {error_text(error)} }
        },
        Some(Ok(lines)) if lines.is_empty() => rsx! {
            div { class: "muted small note", "No tasks." }
        },
        Some(Ok(lines)) => rsx! {
            for line in lines.iter().cloned() {
                TaskEntry { key: "{line.url}", line }
            }
        },
    }
}

#[component]
fn TaskEntry(line: TaskLine) -> Element {
    rsx! {
        a { class: "node", href: "{line.url}", target: "_blank", style: "--depth: {line.depth}",
            span { class: "grow", "#{line.number} {line.title}" }
            for blocker in line.blocked_by.iter() {
                span { class: "muted small",
                    match &blocker.workstream_title {
                        Some(title) => format!("blocked by #{} (Workstream \"{title}\")", blocker.number),
                        None => format!("blocked by #{}", blocker.number),
                    }
                }
            }
            span { class: if line.state == "open" { "chip plain" } else { "chip" }, "{line.state}" }
        }
    }
}

#[component]
fn AgentEntry(node: AgentNode, depth: usize, onclick: EventHandler<MouseEvent>) -> Element {
    let LocalOffset(local_offset) = use_context();
    let session = &node.session;
    let time = format_description!("[month]-[day] [hour]:[minute]");
    let start = session
        .started_at
        .to_offset(local_offset())
        .format(time)
        .unwrap_or_default();
    let end = session
        .ended_at
        .map(|ended_at| {
            format!(
                "–{}",
                ended_at
                    .to_offset(local_offset())
                    .format(format_description!("[hour]:[minute]"))
                    .unwrap_or_default()
            )
        })
        .unwrap_or_default();
    let dot = match (&session.queue_reason, session.ended_at) {
        (Some(_), _) => "dot queued",
        (None, None) => "dot live",
        (None, Some(_)) => "dot ended",
    };
    let detail = session
        .queue_reason
        .clone()
        .unwrap_or_else(|| format!("{start}{end}"));
    rsx! {
        button { class: "node", style: "--depth: {depth}", onclick: move |event| onclick.call(event),
            span { class: dot }
            span { class: "grow",
                span { class: "role", "{node.role}" }
                " {node.title}"
                div { class: "muted small", "{session.harness.name()} · {session.model} · {detail}" }
            }
            if session.ended_at.is_some() {
                span { class: "chip plain", "stopped" }
            }
            if let Some(reason) = &session.queue_reason {
                if reason.starts_with(PAUSED) {
                    span { class: "chip warn", "paused" }
                } else {
                    span { class: "chip warn", "queued" }
                }
            }
        }
    }
}

#[component]
fn Transcript(session: i64) -> Element {
    let lines = use_resource(use_reactive(&session, |session| async move {
        transcript_lines(session).await
    }));
    rsx! {
        div { class: "scroll tx",
            match &*lines.read() {
                Some(Ok(lines)) => rsx! {
                    for line in lines.clone() {
                        TranscriptEntry { key: "{line.id}", line }
                    }
                },
                Some(Err(error)) => rsx! { div { class: "error", {error_text(error)} } },
                None => rsx! {},
            }
        }
        div { class: "readonly", "Read only. The Owner talks only to the Lead." }
    }
}

#[component]
fn TranscriptEntry(line: TranscriptLine) -> Element {
    let LocalOffset(local_offset) = use_context();
    let mut open = use_signal(|| !line.folded);
    let mut raw = use_signal(|| false);
    rsx! {
        div { class: if line.error { "tr crit" } else { "tr" },
            span { class: "num",
                {line.time.to_offset(local_offset()).format(format_description!("[hour]:[minute]")).unwrap_or_default()}
            }
            span { class: "k", "{line.kind}" }
            div {
                span { "{line.text}" }
                if let Some(name) = &line.harness_tool_name {
                    span { class: "muted small", " {name}" }
                }
                if line.folded && line.body.is_some() {
                    button { class: "btn ghost small", onclick: move |_| open.toggle(),
                        if open() { "Hide" } else { "Show" }
                    }
                }
                button { class: "btn ghost small", onclick: move |_| raw.toggle(), "Raw" }
                if let Some(body) = line.body.as_ref().filter(|_| open()) {
                    pre { "{body}" }
                }
                if raw() {
                    pre { "{line.raw}" }
                }
            }
        }
    }
}

#[component]
fn Login() -> Element {
    let LoginShown(mut login_shown) = use_context();
    let mut password = use_signal(String::new);
    let mut error = use_signal(String::new);
    rsx! {
        div { class: "login",
            form {
                onsubmit: move |event: FormEvent| async move {
                    event.prevent_default();
                    match login(password()).await {
                        Ok(_) => login_shown.set(false),
                        Err(failure) => error.set(error_text(&failure)),
                    }
                },
                div { class: "brand", "Mobius" }
                label { class: "label", r#for: "password", "Access password" }
                input {
                    id: "password",
                    r#type: "password",
                    autocomplete: "current-password",
                    value: password,
                    oninput: move |event| password.set(event.value()),
                }
                div { class: "error", {error} }
                button { class: "btn primary", r#type: "submit", "Log in" }
            }
        }
    }
}

#[component]
fn Devices() -> Element {
    let LocalOffset(local_offset) = use_context();
    let LoginShown(mut login_shown) = use_context();
    let mut resource = use_resource(devices);
    use_effect(move || {
        if let Some(Err(error)) = &*resource.read()
            && unauthorized(error)
        {
            login_shown.set(true);
        }
    });
    let list = match &*resource.read() {
        Some(Ok(devices)) => rsx! {
            div { class: "list",
                for login in devices.logins.clone() {
                    div { key: "{login.id}", class: "item",
                        div { class: "grow",
                            div { "{login.user_agent}" }
                            div { class: "muted small",
                                "logged in "
                                {login.created_at.to_offset(local_offset()).format(format_description!("[year]-[month]-[day] [hour]:[minute]")).unwrap_or_default()}
                            }
                        }
                        if login.id == devices.this_device {
                            span { class: "chip", "this device" }
                        }
                        button {
                            class: "btn",
                            onclick: move |_| async move {
                                if logout(login.id).await.is_ok() {
                                    resource.restart();
                                }
                            },
                            "Log out"
                        }
                    }
                }
            }
        },
        Some(Err(error)) => rsx! { p { class: "error note", {error_text(error)} } },
        None => rsx! {},
    };
    rsx! {
        div { class: "head", h2 { "Devices" } }
        {list}
    }
}

async fn create_app(account: String, name: String) -> Result<(), String> {
    let origin: String = document::eval("return window.location.origin;")
        .join()
        .await
        .map_err(|failure| failure.to_string())?;
    let form = github_manifest(account, name, origin)
        .await
        .map_err(|failure| error_text(&failure))?;
    let url = serde_json::to_string(&form.url).map_err(|failure| failure.to_string())?;
    let manifest = serde_json::to_string(&form.manifest).map_err(|failure| failure.to_string())?;
    document::eval(&format!(
        r#"
        const form = document.createElement("form");
        form.method = "post";
        form.action = {url};
        const field = document.createElement("input");
        field.type = "hidden";
        field.name = "manifest";
        field.value = {manifest};
        form.append(field);
        document.body.append(form);
        form.submit();
        "#
    ));
    Ok(())
}

#[component]
fn GitHub() -> Element {
    let AppSlugs(app_slugs) = use_context();
    let mut account = use_signal(String::new);
    let mut name = use_signal(String::new);
    let mut error = use_signal(String::new);
    let slugs = match &*app_slugs.read() {
        Some(Ok(slugs)) => slugs.clone(),
        _ => Vec::new(),
    };
    rsx! {
        div { class: "head", h2 { "Connect GitHub" } }
        for slug in slugs.iter() {
            p { key: "{slug}", class: "note",
                a { href: "https://github.com/apps/{slug}/installations/new", "Install {slug} on your repositories" }
            }
        }
        if !slugs.is_empty() {
            div { class: "label note", "Add an organization" }
        }
        form {
            class: "connect",
            onsubmit: move |event: FormEvent| async move {
                event.prevent_default();
                if let Err(text) = create_app(account(), name()).await {
                    error.set(text);
                }
            },
            label { class: "label", r#for: "account", "Account or organization" }
            input {
                id: "account",
                value: account,
                oninput: move |event| account.set(event.value()),
            }
            label { class: "label", r#for: "name", "App name" }
            input {
                id: "name",
                placeholder: "Mobius {account}",
                value: name,
                oninput: move |event| name.set(event.value()),
            }
            p { class: "muted small", "GitHub App names are unique on all of GitHub. Use a name that no other App has, for example with your account name." }
            div { class: "error", {error} }
            button { class: "btn primary", r#type: "submit", "Create the App" }
        }
    }
}
