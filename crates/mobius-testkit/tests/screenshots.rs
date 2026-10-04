use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::time::Duration;

use chromiumoxide::cdp::browser_protocol::emulation::{
    SetDeviceMetricsOverrideParams, SetTouchEmulationEnabledParams,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchTouchEventParams,
    DispatchTouchEventReturns, DispatchTouchEventType, TouchPoint,
};
use chromiumoxide::cdp::browser_protocol::network::{
    EmulateNetworkConditionsByRuleParams, NetworkConditions,
};
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::types::{Command, Method, MethodId};
use chromiumoxide::{Browser, BrowserConfig, Page};
use dioxus::server::axum::Extension;
use dioxus::server::axum::http::header::CONTENT_TYPE;
use dioxus::server::axum::routing::get;
use futures_util::StreamExt;
use mobius_domain::Author;
use mobius_engine::{Engine, chat, github, inbox, workstreams};
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::{install_fake_harness, start, wait_for};
use tempfile::TempDir;
use time::macros::datetime;
use tokio::net::TcpListener;

const REPOSITORY: &str = "owner/shop";
// The organization `plants` has the second App. The switcher selects `owner` first, because `owner` comes first in the sorted list.
const GARDEN: &str = "plants/garden";
const FAKE_AGENT: &str = env!("CARGO_BIN_EXE_fake-agent");
// The prompt of a Lead holds the earlier events, so the rule of the newest dispatch comes first.
const CLAUDE: &str = r##"
[options]
model = ["sonnet", "opus", "haiku"]
thought_level = ["low", "medium", "high"]
mode = ["default", "bypassPermissions"]

[[prompts]]
when = "dispatch of #42"
call = { tool = "tell_owner", arguments = { text = "#42 needs a decision: one plan for each customer, or many?" } }

[[prompts]]
when = "dispatch of #41"
call = { tool = "start_implementer", arguments = { n = 41, instructions = "Store the price in cents." } }

[[prompts]]
when = "Start a Workstream for gift cards."
reply = ["Title: Gift cards\n\nBrief: Sell gift cards in the shop."]

# The prompt of a new Triager session holds the earlier Owner messages, and the first rule that matches wins. The later message comes first.
[[prompts]]
when = "Move #8 to the Workstream."
call = { tool = "move_issue", arguments = { n = 8, workstream = 12 } }

[[prompts]]
when = "Create the phone Workstream."
call = { tool = "create_workstream", arguments = { title = "Phone plans", brief = "Plans for the phone." } }

[[prompts]]
when = "Move #7 to the Workstream."
call = { tool = "move_issue", arguments = { n = 7, workstream = 12 } }

[[prompts]]
when = "Create the desktop Workstream."
call = { tool = "create_workstream", arguments = { title = "Desktop plans", brief = "Plans for the desktop." } }

[[prompts]]
when = "Which roses sell best?"
reply = ["Red roses sell best."]

[[prompts]]
when = "You are the Reviewer"
hang = true

[[prompts]]
reply = ["The Implementer works on #41. #42 waits for your decision."]
"##;
const IMPLEMENTER: &str = r#"
[options]
model = ["swe-1.5"]
thought_level = ["high"]

[[prompts]]
hang = true
"#;
const COMMITTING_IMPLEMENTER: &str = r#"
[options]
model = ["swe-1.5"]
thought_level = ["high"]

[[prompts]]
shell = "echo cents > plan.txt && git add plan.txt && git commit -q -m 'Add plan model'"
"#;
// The name, the width, the height, and the mobile flag.
type Viewport = (&'static str, u32, u32, bool);
const DESKTOP: Viewport = ("desktop", 1280, 800, false);
const PHONE: Viewport = ("phone", 390, 844, true);

#[derive(Clone, Copy)]
struct Shot<'a> {
    name: &'a str,
    path: &'a str,
    // The CSS selectors of the elements to click, in sequence.
    clicks: &'a [&'a str],
    expected: &'a str,
    // The page shows the Inbox count after the first load of its live data.
    inbox_count: bool,
}

// Outside a `dx` build, `asset!` gives the absolute source path of the file, and the page links that path.
async fn serve_ui(engine: &Engine) -> String {
    let css = fs::canonicalize(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../mobius-ui/assets/main.css"
    ))
    .unwrap();
    let path = css.to_str().unwrap().to_string();
    let router = dioxus::server::router(mobius_ui::App)
        .route(
            &path,
            get(async move || ([(CONTENT_TYPE, "text/css")], fs::read(&css).unwrap())),
        )
        .layer(Extension(engine.clone()))
        .layer(Extension(engine.store.clone()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { dioxus::server::axum::serve(listener, router).await.unwrap() });
    url
}

async fn seed(engine: &Engine, github: &FakeGitHub) {
    github::convert_manifest(engine, "second-code")
        .await
        .unwrap();
    wait_for(async || (workstreams::list(engine).await.unwrap().len() == 2).then_some(())).await;

    github.add_issue(REPOSITORY, 13, "Seasonal prices");
    github.set_body(REPOSITORY, 13, "Change the prices for each season.");
    github.add_label(REPOSITORY, 13, "mobius:workstream", "owner");
    wait_for(async || (workstreams::list(engine).await.unwrap().len() == 3).then_some(())).await;
    github.add_issue(REPOSITORY, 43, "Add season table");
    github.add_sub_issue(REPOSITORY, 13, 43);
    github.close_issue(REPOSITORY, 43);
    wait_for(async || {
        let list = workstreams::list(engine).await.unwrap();
        list.iter()
            .any(|workstream| workstream.number == 13 && workstream.all_tasks_closed)
            .then_some(())
    })
    .await;

    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    wait_for(async || {
        let task = engine.store.tasks().live(REPOSITORY, 41).await.unwrap()?;
        (task.state == "working").then_some(())
    })
    .await;

    github.add_issue(REPOSITORY, 42, "Let customers change plans");
    github.add_sub_issue(REPOSITORY, 12, 42);
    github.add_label(REPOSITORY, 42, "mobius:ready", "owner");
    wait_for(async || (!inbox::list(engine).await.unwrap().is_empty()).then_some(())).await;

    let pull_request = github.open_pull_request(REPOSITORY, "Add plan model", "mobius/41");
    let task = engine
        .store
        .tasks()
        .live(REPOSITORY, 41)
        .await
        .unwrap()
        .unwrap();
    engine
        .store
        .tasks()
        .set_pull_request(task.id, pull_request)
        .await
        .unwrap();
    for number in [41, 42] {
        github.add_label(REPOSITORY, number, "mobius:needs-human", "owner");
    }

    // The event entries of the labels hold the time of the run, so the screenshots keep only the fixed event entry.
    sqlx::query("DELETE FROM chat_messages WHERE author = 'Event'")
        .execute(&engine.store.pool)
        .await
        .unwrap();
    // The first line of the event text and the long URL wrap inside the muted entry, and the rest is collapsed.
    engine
        .store
        .chat_messages()
        .add(
            "owner",
            REPOSITORY,
            12,
            Author::Event,
            "A trusted user commented on #41 \"Add plan model\": https://example.com/reports/loyalty/plans/every-customer-segment-and-billing-period\n\nThe plans need a seat limit for each billing period.",
        )
        .await
        .unwrap();

    // The wide code block, the wide table, and the long URL scroll or break inside the bubble.
    chat::send(
        engine,
        "owner",
        REPOSITORY,
        12,
        r#"What is the state of the plans? The full report is at https://example.com/reports/loyalty/plans/every-customer-segment-and-billing-period.

```text
summary = [{ plan: "standard", seats: 10, price_per_seat: 100, discount_code: "SPRING-SALE-EXTRA-LONG-2026", renewal: "monthly" }]
```

| Plan | Seats | Price per seat | Discount code | Region | Renewal |
| --- | --- | --- | --- | --- | --- |
| Standard | 10 | $100 | SPRING-SALE-EXTRA-LONG-2026 | Worldwide | Monthly |
| Extended | 40 | $80 | AUTUMN-SALE-EXTRA-LONG-2026 | Europe | Yearly |"#,
    )
    .await
    .unwrap();
    wait_for(async || {
        let messages = engine
            .store
            .chat_messages()
            .list("owner", REPOSITORY, 12)
            .await
            .unwrap();
        messages
            .iter()
            .any(|message| message.author == Author::Lead)
            .then_some(())
    })
    .await;

    // The switcher shows the unread reply in `plants`.
    chat::send(engine, "plants", GARDEN, 12, "Which roses sell best?")
        .await
        .unwrap();
    wait_for(async || {
        chat::view(engine, "plants", GARDEN, 12)
            .await
            .unwrap()
            .messages
            .into_iter()
            .find(|message| message.author == Author::Lead)
    })
    .await;

    chat::send(engine, "owner", "", 0, "Start a Workstream for gift cards.")
        .await
        .unwrap();
    wait_for(async || {
        chat::view(engine, "owner", "", 0)
            .await
            .unwrap()
            .messages
            .into_iter()
            .find(|message| message.author == Author::Triager)
    })
    .await;

    // The Chat page marks the messages as seen, so an unread count changes while a screenshot waits.
    for (organization, repository, workstream) in [("owner", REPOSITORY, 12), ("owner", "", 0)] {
        let messages = chat::view(engine, organization, repository, workstream)
            .await
            .unwrap()
            .messages;
        let last = messages.last().unwrap().id;
        chat::seen(engine, organization, repository, workstream, last)
            .await
            .unwrap();
    }

    // The ended sessions get their end time before `fix_times` changes it.
    wait_for(async || {
        let open: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sessions WHERE ended_at IS NULL AND role != 'implementer'",
        )
        .fetch_one(&engine.store.pool)
        .await
        .unwrap();
        (open == 0).then_some(())
    })
    .await;
}

// The UI shows these times, so each run must give the same values.
async fn fix_times(engine: &Engine) {
    let time = datetime!(2026-09-28 09:30 UTC);
    for sql in [
        "UPDATE device_logins SET created_at = ?1",
        "UPDATE events SET time = ?1",
        "UPDATE sessions SET started_at = ?1, ended_at = iif(ended_at IS NULL, NULL, ?1)",
        "UPDATE transcript SET time = ?1",
        "UPDATE chat_messages SET time = ?1",
        "UPDATE inbox_items SET time = ?1",
    ] {
        sqlx::query(sql)
            .bind(time)
            .execute(&engine.store.pool)
            .await
            .unwrap();
    }
}

// A script fails while the page loads the next document.
async fn check(page: &Page, script: String) -> bool {
    match page.evaluate(script).await {
        Ok(result) => result.into_value().unwrap(),
        Err(_) => false,
    }
}

async fn wait_until_ready(page: &Page, expected: &str, inbox_count: bool) {
    let script = format!(
        "document.body.textContent.includes({expected:?}) && \
         (!{inbox_count} || !!document.querySelector('a[href=\"/inbox\"] .count'))"
    );
    wait_for(async || check(page, script.clone()).await.then_some(())).await;
}

// A new tab has no text, so the check in `wait_until_ready` cannot match the page before.
async fn open(browser: &Browser, url: &str, (_, width, height, mobile): Viewport) -> Page {
    let page = browser.new_page("about:blank").await.unwrap();
    page.set_user_agent("Mobius screenshots").await.unwrap();
    page.execute(SetDeviceMetricsOverrideParams::new(
        width, height, 1.0, mobile,
    ))
    .await
    .unwrap();
    // Headless Chrome claims a touch pointer (`pointer: coarse`) on every viewport.
    // The touch emulation sets the pointer type of each device class: on for the
    // phone shots, off for the desktop shots so they report `pointer: fine`.
    page.execute(SetTouchEmulationEnabledParams::new(mobile))
        .await
        .unwrap();
    page.evaluate(format!("location.href = {url:?}"))
        .await
        .unwrap();
    page
}

async fn capture(page: &Page) -> Vec<u8> {
    let params = ScreenshotParams::builder()
        .format(CaptureScreenshotFormat::Png)
        .build();
    page.screenshot(params).await.unwrap()
}

fn decode(png: &[u8]) -> (png::OutputInfo, Vec<u8>) {
    let mut reader = png::Decoder::new(Cursor::new(png)).read_info().unwrap();
    let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut pixels).unwrap();
    (info, pixels)
}

fn looks_same(old: &[u8], new: &[u8]) -> bool {
    let (old_info, old) = decode(old);
    let (new_info, new) = decode(new);
    (old_info.width, old_info.height) == (new_info.width, new_info.height)
        && old
            .iter()
            .zip(&new)
            .all(|(old, new)| old.abs_diff(*new) <= 16)
}

async fn screenshot(browser: &Browser, url: &str, shot: Shot<'_>, viewport: Viewport) {
    let page = open(browser, &format!("{url}{}", shot.path), viewport).await;
    for selector in shot.clicks {
        let script = format!(
            "(() => {{ const element = document.querySelector({selector:?}); element?.click(); return !!element; }})()"
        );
        wait_for(async || check(&page, script.clone()).await.then_some(())).await;
    }
    wait_until_ready(&page, shot.expected, shot.inbox_count).await;
    let mut last = capture(&page).await;
    let png = wait_for(async || {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let next = capture(&page).await;
        let same = next == last;
        last = next;
        same.then(|| last.clone())
    })
    .await;
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mobius-ui/screenshots");
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(format!("{}-{}.png", shot.name, viewport.0));
    // Chrome can draw the edge pixels of text and of round corners a little differently in each run.
    if !fs::read(&path).is_ok_and(|old| looks_same(&old, &png)) {
        fs::write(path, png).unwrap();
    }
    page.close().await.unwrap();
}

async fn log_in(browser: &Browser, url: &str) {
    let page = open(browser, url, DESKTOP).await;
    wait_until_ready(&page, "Access password", false).await;
    page.find_element("#password")
        .await
        .unwrap()
        .click()
        .await
        .unwrap()
        .type_str("correct horse")
        .await
        .unwrap();
    page.find_element("button[type=submit]")
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    wait_until_ready(&page, "App name", false).await;
    page.close().await.unwrap();
}

// The server-side render shows the page before the app hydrates. The scroll effect sets
// `__mobiusScroll` only when the app runs, so it marks a live page.
async fn wait_until_live(page: &Page) {
    wait_for(async || {
        check(
            page,
            "(() => { const list = document.querySelector(\".msgs\");\
             return !!(list && list.__mobiusScroll); })()"
                .to_string(),
        )
        .await
        .then_some(())
    })
    .await;
}

#[tokio::test]
#[ignore = "starts Chrome and serves the web bundle in DIOXUS_PUBLIC_PATH"]
async fn message_list_scrolls_to_the_bottom() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", CLAUDE);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    let url = serve_ui(&engine).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    wait_for(async || (workstreams::list(&engine).await.unwrap().len() == 1).then_some(())).await;
    let (mut browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .launch_timeout(Duration::from_secs(60))
            .no_sandbox()
            .arg("--hide-scrollbars")
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    tokio::spawn(async move { while handler.next().await.is_some() {} });
    log_in(&browser, &format!("{url}/github")).await;
    for (name, width, height, mobile) in [DESKTOP, PHONE] {
        let page = open(
            &browser,
            &format!("{url}/workstreams/owner/shop/12"),
            (name, width, height, mobile),
        )
        .await;
        wait_until_ready(&page, "Integrate loyalty plans", false).await;
        wait_until_live(&page).await;
        for n in 0..20 {
            let text = format!("spam {name} {n} {}", "word ".repeat(40));
            chat::send(&engine, "owner", REPOSITORY, 12, &text)
                .await
                .unwrap();
        }
        // The list must show the last message and overflow, or the scroll position proves nothing.
        let script = format!(
            "(() => {{ const list = document.querySelector(\".msgs\");\
             return list.textContent.includes(\"spam {name} 19\") \
             && list.scrollHeight > list.clientHeight \
             && list.scrollHeight - list.scrollTop - list.clientHeight < 5; }})()"
        );
        wait_for(async || check(&page, script.clone()).await.then_some(())).await;
        page.close().await.unwrap();
    }
    browser.close().await.unwrap();
}

#[tokio::test]
#[ignore = "starts Chrome and serves the web bundle in DIOXUS_PUBLIC_PATH"]
async fn an_event_shows_in_the_chat_with_the_muted_style_and_wraps_on_the_phone() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", CLAUDE);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    let url = serve_ui(&engine).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    wait_for(async || (workstreams::list(&engine).await.unwrap().len() == 1).then_some(())).await;
    let text = format!(
        "A comment arrived: {}\n\nThe rest of the comment.",
        "word".repeat(60)
    );
    engine
        .store
        .chat_messages()
        .add("owner", REPOSITORY, 12, Author::Event, &text)
        .await
        .unwrap();
    engine
        .store
        .chat_messages()
        .add("owner", REPOSITORY, 12, Author::Event, "A label changed.")
        .await
        .unwrap();
    let (mut browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .launch_timeout(Duration::from_secs(60))
            .no_sandbox()
            .arg("--hide-scrollbars")
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    tokio::spawn(async move { while handler.next().await.is_some() {} });
    log_in(&browser, &format!("{url}/github")).await;
    for viewport in [DESKTOP, PHONE] {
        let page = open(
            &browser,
            &format!("{url}/workstreams/owner/shop/12"),
            viewport,
        )
        .await;
        wait_until_ready(&page, "A comment arrived", false).await;
        let script = "(() => { const event = document.querySelector(\".msg.event\");\
             const root = getComputedStyle(document.documentElement);\
             const probe = document.createElement(\"span\");\
             probe.style.color = root.getPropertyValue(\"--muted\");\
             document.body.append(probe);\
             const muted = getComputedStyle(probe).color;\
             probe.remove();\
             const list = document.querySelector(\".msgs\");\
             return !!event && getComputedStyle(event).color === muted \
             && list.scrollWidth <= list.clientWidth; })()"
            .to_string();
        wait_for(async || check(&page, script.clone()).await.then_some(())).await;
        let collapsed = "(() => { const events = [...document.querySelectorAll(\".msg.event\")];\
             const folded = events.find((event) => event.textContent.includes(\"A comment arrived\"));\
             const single = events.find((event) => event.textContent.includes(\"A label changed.\"));\
             return !!folded && !!single && !single.querySelector(\"details\") \
             && !folded.querySelector(\"details\").open \
             && folded.querySelector(\"summary\").textContent.startsWith(\"A comment arrived\") \
             && !folded.querySelector(\".md\").checkVisibility(); })()"
            .to_string();
        wait_for(async || check(&page, collapsed.clone()).await.then_some(())).await;
        page.find_element(".msg.event summary")
            .await
            .unwrap()
            .click()
            .await
            .unwrap();
        let opened = "(() => { const details = document.querySelector(\".msg.event details\");\
             return details.open && details.querySelector(\".md\").checkVisibility() \
             && document.querySelector(\".msgs\").scrollWidth <= document.querySelector(\".msgs\").clientWidth; })()"
            .to_string();
        wait_for(async || check(&page, opened.clone()).await.then_some(())).await;
        page.close().await.unwrap();
    }
    browser.close().await.unwrap();
}

// An Owner message in the chat of the Workstream, as the engine stores it.
async fn owner_sent(engine: &Engine, text: &str) -> bool {
    let view = chat::view(engine, "owner", REPOSITORY, 12).await.unwrap();
    view.messages
        .iter()
        .any(|message| message.author == Author::Owner && message.text == text)
}

async fn press_enter(page: &Page, shift: bool) {
    let params = DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::KeyDown)
        .key("Enter")
        .code("Enter")
        .windows_virtual_key_code(13)
        .text("\r")
        .modifiers(if shift { 8 } else { 0 })
        .build()
        .unwrap();
    page.execute(params).await.unwrap();
}

// On the desktop Enter sends and Shift+Enter adds a new line; on the phone Enter adds a new line.
#[tokio::test]
#[ignore = "starts Chrome and serves the web bundle in DIOXUS_PUBLIC_PATH"]
async fn enter_key_sends_on_the_desktop_only() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", CLAUDE);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    let url = serve_ui(&engine).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    wait_for(async || (workstreams::list(&engine).await.unwrap().len() == 1).then_some(())).await;
    let (mut browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .launch_timeout(Duration::from_secs(60))
            .no_sandbox()
            .arg("--hide-scrollbars")
            .user_data_dir(data_dir.path().join("chrome"))
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    tokio::spawn(async move { while handler.next().await.is_some() {} });
    log_in(&browser, &format!("{url}/github")).await;
    let page = open(
        &browser,
        &format!("{url}/workstreams/owner/shop/12"),
        DESKTOP,
    )
    .await;
    wait_until_live(&page).await;
    let input = page.find_element(".composer textarea").await.unwrap();
    input.click().await.unwrap().type_str("one").await.unwrap();
    press_enter(&page, true).await;
    input.type_str("two").await.unwrap();
    press_enter(&page, false).await;
    wait_for(async || owner_sent(&engine, "one\ntwo").await.then_some(())).await;
    page.close().await.unwrap();
    let page = open(&browser, &format!("{url}/workstreams/owner/shop/12"), PHONE).await;
    wait_until_live(&page).await;
    let input = page.find_element(".composer textarea").await.unwrap();
    input
        .click()
        .await
        .unwrap()
        .type_str("three")
        .await
        .unwrap();
    press_enter(&page, false).await;
    input.type_str("four").await.unwrap();
    // A CDP mouse click does not activate the button while touch emulation is on.
    page.evaluate("document.querySelector('.composer button[type=submit]').click()")
        .await
        .unwrap();
    wait_for(async || owner_sent(&engine, "three\nfour").await.then_some(())).await;
    page.close().await.unwrap();
    browser.close().await.unwrap();
}

async fn tap_send(page: &Page, taps: usize) {
    page.evaluate(format!(
        "for (let n = 0; n < {taps}; n++) document.querySelector('.composer button[type=submit]').click()"
    ))
    .await
    .unwrap();
}

// `DispatchTouchEventParams` leaves out the empty `touchPoints` list, and Chrome requires it on `touchEnd`.
#[derive(serde::Serialize)]
struct TouchEnd {
    r#type: &'static str,
    #[serde(rename = "touchPoints")]
    touch_points: [TouchPoint; 0],
}

impl Method for TouchEnd {
    fn identifier(&self) -> MethodId {
        "Input.dispatchTouchEvent".into()
    }
}

impl Command for TouchEnd {
    type Response = DispatchTouchEventReturns;
}

async fn touch_send(page: &Page) {
    let center: Vec<f64> = page
        .evaluate(
            "(() => { const box = document.querySelector('.composer button[type=submit]').getBoundingClientRect();\
             return [box.left + box.width / 2, box.top + box.height / 2]; })()",
        )
        .await
        .unwrap()
        .into_value()
        .unwrap();
    page.execute(DispatchTouchEventParams::new(
        DispatchTouchEventType::TouchStart,
        vec![TouchPoint::new(center[0], center[1])],
    ))
    .await
    .unwrap();
    page.execute(TouchEnd {
        r#type: "touchEnd",
        touch_points: [],
    })
    .await
    .unwrap();
}

fn send_enabled() -> String {
    "!document.querySelector('.composer button[type=submit]').disabled".to_string()
}

fn input_is_focused() -> String {
    "document.activeElement === document.querySelector('.composer textarea')".to_string()
}

fn input_is(value: &str) -> String {
    format!("document.querySelector('.composer textarea').value === {value:?}")
}

// Two taps on Send in one turn of the page send one message, and the buttons are easy to tap.
#[tokio::test]
#[ignore = "starts Chrome and serves the web bundle in DIOXUS_PUBLIC_PATH"]
async fn send_taps_once_and_the_buttons_are_easy_to_tap_on_the_phone() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", CLAUDE);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    let url = serve_ui(&engine).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    wait_for(async || (workstreams::list(&engine).await.unwrap().len() == 1).then_some(())).await;
    let (mut browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .no_sandbox()
            .arg("--hide-scrollbars")
            .launch_timeout(Duration::from_secs(60))
            .user_data_dir(data_dir.path().join("chrome"))
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    tokio::spawn(async move { while handler.next().await.is_some() {} });
    log_in(&browser, &format!("{url}/github")).await;
    let page = open(&browser, &format!("{url}/workstreams/owner/shop/12"), PHONE).await;
    wait_until_live(&page).await;
    let input = page.find_element(".composer textarea").await.unwrap();
    input
        .click()
        .await
        .unwrap()
        .type_str("double")
        .await
        .unwrap();
    tap_send(&page, 2).await;
    wait_for(async || owner_sent(&engine, "double").await.then_some(())).await;
    input.type_str("after").await.unwrap();
    wait_for(async || check(&page, send_enabled()).await.then_some(())).await;
    tap_send(&page, 1).await;
    wait_for(async || owner_sent(&engine, "after").await.then_some(())).await;
    let view = chat::view(&engine, "owner", REPOSITORY, 12).await.unwrap();
    let sent = view
        .messages
        .iter()
        .filter(|message| message.author == Author::Owner)
        .count();
    assert_eq!(sent, 2);
    let apart = check(
        &page,
        "(() => { const field = document.querySelector('.composer textarea').getBoundingClientRect();\
         const buttons = [...document.querySelectorAll('.composer .btn')];\
         return buttons.length > 0 && buttons.every((button) => {\
           const box = button.getBoundingClientRect();\
           return box.height >= 40 && (box.left >= field.right || box.right <= field.left);\
         }); })()"
            .to_string(),
    )
    .await;
    assert!(apart);
    page.evaluate(
        "window.__failSend = false; const fetchOriginal = window.fetch;\
         window.fetch = async (...args) => {\
           if (!String(args[0].url ?? args[0]).includes('/api/chat/send')) return fetchOriginal(...args);\
           await new Promise((resolve) => setTimeout(resolve, 1000));\
           return window.__failSend ? new Response('failed', { status: 500 }) : fetchOriginal(...args);\
         }",
    )
    .await
    .unwrap();
    input.type_str("hello").await.unwrap();
    tap_send(&page, 1).await;
    wait_for(async || check(&page, input_is("")).await.then_some(())).await;
    input.type_str("more").await.unwrap();
    wait_for(async || owner_sent(&engine, "hello").await.then_some(())).await;
    assert!(check(&page, input_is("more")).await);
    wait_for(async || check(&page, send_enabled()).await.then_some(())).await;
    tap_send(&page, 1).await;
    wait_for(async || owner_sent(&engine, "more").await.then_some(())).await;
    page.evaluate("window.__failSend = true").await.unwrap();
    input.type_str("lost").await.unwrap();
    wait_for(async || check(&page, send_enabled()).await.then_some(())).await;
    tap_send(&page, 1).await;
    wait_for(async || check(&page, input_is("")).await.then_some(())).await;
    input.type_str("!").await.unwrap();
    wait_for(async || check(&page, input_is("lost!")).await.then_some(())).await;
    assert!(!owner_sent(&engine, "lost").await);
    page.close().await.unwrap();
    browser.close().await.unwrap();
}

// The tap on Send does not move the focus away from the chat input, so the layout stays and the tap sends.
#[tokio::test]
#[ignore = "starts Chrome and serves the web bundle in DIOXUS_PUBLIC_PATH"]
async fn one_tap_on_send_sends_while_the_chat_input_has_the_focus_on_the_phone() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", CLAUDE);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    let url = serve_ui(&engine).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    wait_for(async || (workstreams::list(&engine).await.unwrap().len() == 1).then_some(())).await;
    let (mut browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .no_sandbox()
            .arg("--hide-scrollbars")
            .launch_timeout(Duration::from_secs(60))
            .user_data_dir(data_dir.path().join("chrome"))
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    tokio::spawn(async move { while handler.next().await.is_some() {} });
    log_in(&browser, &format!("{url}/github")).await;
    let page = open(&browser, &format!("{url}/workstreams/owner/shop/12"), PHONE).await;
    wait_until_live(&page).await;
    let input = page.find_element(".composer textarea").await.unwrap();
    input
        .click()
        .await
        .unwrap()
        .type_str("one tap")
        .await
        .unwrap();
    assert!(check(&page, input_is_focused()).await);
    touch_send(&page).await;
    wait_for(async || owner_sent(&engine, "one tap").await.then_some(())).await;
    let view = chat::view(&engine, "owner", REPOSITORY, 12).await.unwrap();
    let sent = view
        .messages
        .iter()
        .filter(|message| message.author == Author::Owner)
        .count();
    assert_eq!(sent, 1);
    assert!(check(&page, input_is_focused()).await);
    page.close().await.unwrap();
    browser.close().await.unwrap();
}

// A message list that overflows, with the Lead messages after the fifth one unread.
// It returns the ids of the first unread message and of the last message.
async fn seed_unread(engine: &Engine, number: i64) -> (i64, i64) {
    let chat_messages = engine.store.chat_messages();
    let mut ids = Vec::new();
    for n in 0..12 {
        let text = format!("note {number} {n} {}", "word ".repeat(100));
        let message = chat_messages
            .add("owner", REPOSITORY, number, Author::Lead, &text)
            .await
            .unwrap();
        ids.push(message.id);
    }
    chat::seen(engine, "owner", REPOSITORY, number, ids[4])
        .await
        .unwrap();
    (ids[5], ids[11])
}

fn message_at_top(id: i64) -> String {
    format!(
        "(() => {{ const list = document.querySelector(\".msgs\");\
         const message = list.querySelector('[data-message=\"{id}\"]');\
         return !!message && list.scrollHeight > list.clientHeight \
         && Math.abs(message.getBoundingClientRect().top - list.getBoundingClientRect().top) < 5; }})()"
    )
}

fn list_at_bottom(id: i64) -> String {
    format!(
        "(() => {{ const list = document.querySelector(\".msgs\");\
         return !!list.querySelector('[data-message=\"{id}\"]') \
         && list.scrollHeight > list.clientHeight \
         && list.scrollHeight - list.scrollTop - list.clientHeight < 5; }})()"
    )
}

// The position must stay after the script first holds, or a later scroll could hide a wrong position.
async fn expect_position(page: &Page, script: String) {
    wait_for(async || check(page, script.clone()).await.then_some(())).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(check(page, script).await);
}

async fn switch_to(page: &Page, number: i64) {
    let script = format!(
        "(() => {{ const link = document.querySelector('a[href=\"/workstreams/owner/shop/{number}\"]');\
         link?.click(); return !!link; }})()"
    );
    wait_for(async || check(page, script.clone()).await.then_some(())).await;
}

// A switch to a chat shows its first unread message, or its last message without unread messages.
#[tokio::test]
#[ignore = "starts Chrome and serves the web bundle in DIOXUS_PUBLIC_PATH"]
async fn chat_switch_shows_the_first_unread_message() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", CLAUDE);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    let url = serve_ui(&engine).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    github.add_repository(REPOSITORY);
    for number in 12..=15 {
        github.add_issue(REPOSITORY, number, &format!("Workstream {number}"));
        github.add_label(REPOSITORY, number, "mobius:workstream", "owner");
    }
    wait_for(async || (workstreams::list(&engine).await.unwrap().len() == 4).then_some(())).await;
    let (mut browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .launch_timeout(Duration::from_secs(60))
            .no_sandbox()
            .arg("--hide-scrollbars")
            .user_data_dir(data_dir.path().join("chrome"))
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    tokio::spawn(async move { while handler.next().await.is_some() {} });
    log_in(&browser, &format!("{url}/github")).await;
    for (viewport, first, second) in [(DESKTOP, 12, 13), (PHONE, 14, 15)] {
        let (first_unread, first_last) = seed_unread(&engine, first).await;
        let (second_unread, second_last) = seed_unread(&engine, second).await;
        let page = open(
            &browser,
            &format!("{url}/workstreams/owner/shop/{first}"),
            viewport,
        )
        .await;
        wait_until_live(&page).await;
        expect_position(&page, message_at_top(first_unread)).await;
        switch_to(&page, second).await;
        expect_position(&page, message_at_top(second_unread)).await;
        switch_to(&page, first).await;
        expect_position(&page, list_at_bottom(first_last)).await;
        switch_to(&page, second).await;
        expect_position(&page, list_at_bottom(second_last)).await;
        page.close().await.unwrap();
    }
    browser.close().await.unwrap();
}

// An empty chat opens with the Brief expanded; a click on its head leaves only the title.
#[tokio::test]
#[ignore = "starts Chrome and serves the web bundle in DIOXUS_PUBLIC_PATH"]
async fn brief() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.set_body(
        REPOSITORY,
        12,
        "Reward repeat customers with **points** on every order.",
    );
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 13, "Add discount codes");
    github.set_body(REPOSITORY, 13, "Apply **codes** at checkout.");
    github.add_label(REPOSITORY, 13, "mobius:workstream", "owner");
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || {
        let list = workstreams::list(&engine).await.unwrap();
        (list.len() == 2 && list.iter().all(|workstream| !workstream.body.is_empty())).then_some(())
    })
    .await;
    let url = serve_ui(&engine).await;
    let (mut browser, mut handler) = Browser::launch(
        // Without a data dir, every launch uses the same `chromiumoxide-runner` directory, so a second Chrome exits while the first runs.
        BrowserConfig::builder()
            .launch_timeout(Duration::from_secs(60))
            .no_sandbox()
            .user_data_dir(data_dir.path().join("chrome"))
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    tokio::spawn(async move { while handler.next().await.is_some() {} });
    log_in(&browser, &format!("{url}/github")).await;
    let page = open(
        &browser,
        &format!("{url}/workstreams/owner/shop/12"),
        DESKTOP,
    )
    .await;
    let expanded = String::from(
        "(() => {
            const brief = document.querySelector('.brief .md');
            return !!brief && brief.textContent.includes('points on every order')
                && !!brief.querySelector('strong');
        })()",
    );
    wait_for(async || check(&page, expanded.clone()).await.then_some(())).await;
    let click = String::from(
        "(() => {
            const head = document.querySelector('.briefhead');
            head?.click();
            return !!head;
        })()",
    );
    wait_for(async || check(&page, click.clone()).await.then_some(())).await;
    let collapsed = String::from(
        "(() => {
            const brief = document.querySelector('.brief');
            return !!brief && brief.textContent.includes('Integrate loyalty plans')
                && !brief.querySelector('.md');
        })()",
    );
    wait_for(async || check(&page, collapsed.clone()).await.then_some(())).await;
    // The chat of another Workstream keeps no state of the one before: its Brief shows expanded.
    let second = String::from(
        "(() => {
            const link = document.querySelector('a[href=\"/workstreams/owner/shop/13\"]');
            link?.click();
            return !!link;
        })()",
    );
    wait_for(async || check(&page, second.clone()).await.then_some(())).await;
    let expanded_second = String::from(
        "(() => {
            const brief = document.querySelector('.brief .md');
            return !!brief && brief.textContent.includes('codes at checkout')
                && !!brief.querySelector('strong');
        })()",
    );
    wait_for(async || check(&page, expanded_second.clone()).await.then_some(())).await;
    page.close().await.unwrap();
    browser.close().await.unwrap();
}

#[tokio::test]
#[ignore = "starts Chrome and serves the web bundle in DIOXUS_PUBLIC_PATH"]
async fn screenshots() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    github.add_manifest_code("second-code");
    github.install_second_app("plants");
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", CLAUDE);
    install_fake_harness(data_dir.path(), FAKE_AGENT, "devin", IMPLEMENTER);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    let url = serve_ui(&engine).await;
    let (mut browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .launch_timeout(Duration::from_secs(60))
            .no_sandbox()
            .user_data_dir(data_dir.path().join("chrome"))
            .arg("--hide-scrollbars")
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    tokio::spawn(async move { while handler.next().await.is_some() {} });

    let login = Shot {
        name: "login",
        path: "/github",
        clicks: &[],
        expected: "Access password",
        inbox_count: false,
    };
    let connect = Shot {
        name: "github-connect",
        path: "/github",
        clicks: &[],
        expected: "App name",
        inbox_count: false,
    };
    for viewport in [DESKTOP, PHONE] {
        screenshot(&browser, &url, login, viewport).await;
    }
    log_in(&browser, &format!("{url}/github")).await;
    for viewport in [DESKTOP, PHONE] {
        screenshot(&browser, &url, connect, viewport).await;
    }

    // The App has no repository yet, so Mobius knows no organization.
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    let no_organization = Shot {
        name: "new-workstream-no-organization",
        path: "/workstreams/new",
        clicks: &[],
        expected: "Mobius reads the repositories from GitHub.",
        inbox_count: false,
    };
    for viewport in [DESKTOP, PHONE] {
        screenshot(&browser, &url, no_organization, viewport).await;
    }

    for (repository, title, brief) in [
        (
            REPOSITORY,
            "Integrate loyalty plans",
            "Reward repeat customers.\n\n- Points on every order\n- One **free** plan for staff",
        ),
        (
            GARDEN,
            "Plant roses",
            "Plant **roses** along the south fence.",
        ),
    ] {
        github.add_repository(repository);
        github.add_issue(repository, 12, title);
        github.set_body(repository, 12, brief);
        github.add_label(repository, 12, "mobius:workstream", "owner");
    }
    seed(&engine, &github).await;
    // The first poll created each missing Mobius label. For the Checkup shot,
    // one label is missing again and one has a wrong color.
    github.delete_repository_label(REPOSITORY, "mobius:no-workstream");
    github.add_repository_label(
        REPOSITORY,
        "mobius:working",
        "ededed",
        "A Mobius agent works on this task",
    );
    fix_times(&engine).await;
    for viewport in [DESKTOP, PHONE] {
        let (tasks_clicks, switch_clicks): (&[&str], &[&str]) = if viewport == PHONE {
            (
                &[".btn.phone", ".sheet .sidetabs button:nth-child(2)"],
                &[".head .switch"],
            )
        } else {
            (&[".side .sidetabs button:nth-child(2)"], &[".rail .switch"])
        };
        let shots = [
            Shot {
                name: "workstreams",
                path: "/workstreams",
                clicks: &[],
                expected: "needs you",
                inbox_count: true,
            },
            Shot {
                name: "organizations",
                path: "/workstreams",
                clicks: switch_clicks,
                expected: "Organizations",
                inbox_count: true,
            },
            Shot {
                name: "activity",
                path: "/activity",
                clicks: &[],
                expected: "Dispatched",
                inbox_count: true,
            },
            Shot {
                name: "chat",
                path: "/workstreams/owner/shop/12",
                clicks: &[],
                expected: "Resume",
                inbox_count: true,
            },
            Shot {
                name: "chat-all-tasks-closed",
                path: "/workstreams/owner/shop/13",
                clicks: &[],
                expected: "All tasks are closed.",
                inbox_count: true,
            },
            Shot {
                name: "chat-tasks",
                path: "/workstreams/owner/shop/12",
                clicks: tasks_clicks,
                expected: "#41 Add plan model",
                inbox_count: true,
            },
            Shot {
                name: "new-workstream",
                path: "/workstreams/new",
                clicks: &[],
                expected: "Sell gift cards in the shop.",
                inbox_count: true,
            },
            Shot {
                name: "settings",
                path: "/settings",
                clicks: &[],
                expected: "Devices",
                inbox_count: true,
            },
            Shot {
                name: "checkup",
                path: "/settings/checkup",
                clicks: &[],
                expected: "missing: add on GitHub",
                inbox_count: true,
            },
            Shot {
                name: "agents",
                path: "/agents",
                clicks: &[],
                expected: "Ticket #41",
                inbox_count: true,
            },
            Shot {
                name: "inbox",
                path: "/inbox",
                clicks: &[],
                expected: "one plan for each customer",
                inbox_count: true,
            },
            Shot {
                name: "devices",
                path: "/devices",
                clicks: &[],
                expected: "this device",
                inbox_count: true,
            },
            Shot {
                name: "github",
                path: "/github",
                clicks: &[],
                expected: "Install mobius-second",
                inbox_count: true,
            },
        ];
        for shot in shots {
            screenshot(&browser, &url, shot, viewport).await;
        }
        if viewport == PHONE {
            // A wide message scrolls inside the bubble; the page itself never scrolls sideways.
            let page = open(
                &browser,
                &format!("{url}/workstreams/owner/shop/12"),
                ("check", 375, 667, true),
            )
            .await;
            wait_until_ready(&page, "#42 waits for your decision.", false).await;
            // `overflow-x: auto` keeps the wide block inside the bubble, so no ancestor overflows.
            let script = String::from(
                "(() => {
                    const msgs = document.querySelector('.msgs');
                    const bubble = document.querySelector('.msg.owner');
                    const message = bubble?.querySelector('.md');
                    const pre = message?.querySelector('pre');
                    const table = message?.querySelector('table');
                    const overflows = (element) => element.scrollWidth > element.clientWidth;
                    const scrolls = (element) => getComputedStyle(element).overflowX === 'auto';
                    return !!pre && !!table
                        && overflows(pre) && scrolls(pre)
                        && overflows(table) && scrolls(table)
                        && !overflows(message) && !overflows(bubble) && !overflows(msgs)
                        && document.documentElement.scrollWidth <= window.innerWidth;
                })()",
            );
            wait_for(async || check(&page, script.clone()).await.then_some(())).await;
            page.close().await.unwrap();
        }
    }
    browser.close().await.unwrap();
}

// The Triager chat stays open after `create_workstream` and `move_issue`, and the sidebar lists the new Workstream.
#[tokio::test]
#[ignore = "starts Chrome and serves the web bundle in DIOXUS_PUBLIC_PATH"]
async fn the_triager_chat_stays_open_after_its_actions() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", CLAUDE);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    let url = serve_ui(&engine).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 7, "Add plan prices");
    github.add_issue(REPOSITORY, 8, "Add plan names");
    wait_for(async || (workstreams::list(&engine).await.unwrap().len() == 1).then_some(())).await;
    let (mut browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .launch_timeout(Duration::from_secs(60))
            .no_sandbox()
            .arg("--hide-scrollbars")
            .user_data_dir(data_dir.path().join("chrome"))
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    tokio::spawn(async move { while handler.next().await.is_some() {} });
    log_in(&browser, &format!("{url}/github")).await;
    for (viewport, create, moved, number, issue) in [
        (
            DESKTOP,
            "Create the desktop Workstream.",
            "Move #7 to the Workstream.",
            13,
            7,
        ),
        (
            PHONE,
            "Create the phone Workstream.",
            "Move #8 to the Workstream.",
            14,
            8,
        ),
    ] {
        let page = open(&browser, &format!("{url}/workstreams/new"), viewport).await;
        wait_until_live(&page).await;
        chat::send(&engine, "owner", "", 0, create).await.unwrap();
        let link =
            format!("!!document.querySelector('a[href=\"/workstreams/owner/shop/{number}\"]')");
        wait_for(async || check(&page, link.clone()).await.then_some(())).await;
        chat::send(&engine, "owner", "", 0, moved).await.unwrap();
        wait_for(async || {
            github
                .sub_issue_numbers(REPOSITORY, 12)
                .contains(&issue)
                .then_some(())
        })
        .await;
        assert!(
            check(
                &page,
                "location.pathname === '/workstreams/new'".to_string()
            )
            .await
        );
        assert!(check(&page, link).await);
        page.close().await.unwrap();
    }
    browser.close().await.unwrap();
}

// The Upgrade button needs a release version, so the test also needs `MOBIUS_VERSION` at build time.
#[tokio::test]
#[ignore = "starts Chrome, serves the web bundle in DIOXUS_PUBLIC_PATH, and needs MOBIUS_VERSION at build time"]
async fn the_upgrade_modal_lists_the_release_changes() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.set_release(
        "v0.1.4",
        &[
            "Send all events and Owner messages to one lead session (#317)",
            "Check for a new release each hour (#316)\n\nThe check runs once each hour.",
            "Server: keep a Workstream in the list when its sub-issues cannot be read (#314)",
            "Show the release changes in a modal before the upgrade (#320)",
        ],
    );
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", CLAUDE);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    let url = serve_ui(&engine).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    wait_for(async || github::new_release(&engine).map(|_| ())).await;
    let (mut browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .launch_timeout(Duration::from_secs(60))
            .no_sandbox()
            .arg("--hide-scrollbars")
            .user_data_dir(data_dir.path().join("chrome"))
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    tokio::spawn(async move { while handler.next().await.is_some() {} });
    log_in(&browser, &format!("{url}/github")).await;
    let upgrade = Shot {
        name: "upgrade",
        path: "/workstreams/new",
        clicks: &["button.entry.upd"],
        expected: "Show the release changes in a modal before the upgrade (#320)",
        inbox_count: false,
    };
    screenshot(&browser, &url, upgrade, DESKTOP).await;
    let line = Shot {
        name: "upgrade-line",
        path: "/workstreams",
        clicks: &[],
        expected: "Upgrade v0.1.4",
        inbox_count: false,
    };
    screenshot(&browser, &url, line, PHONE).await;
    let modal = Shot {
        name: "upgrade",
        path: "/workstreams",
        clicks: &["button.upd.phone"],
        expected: "Show the release changes in a modal before the upgrade (#320)",
        inbox_count: false,
    };
    screenshot(&browser, &url, modal, PHONE).await;
    browser.close().await.unwrap();
}

#[tokio::test]
#[ignore = "starts Chrome and serves the web bundle in DIOXUS_PUBLIC_PATH"]
async fn the_note_closes_the_workstream_when_all_tasks_are_closed() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    let holding_lead = CLAUDE.replace(
        "when = \"dispatch of #41\"\n",
        "when = \"dispatch of #41\"\nhang = true\n",
    );
    assert_ne!(holding_lead, CLAUDE);
    install_fake_harness(
        data_dir.path(),
        FAKE_AGENT,
        "claude-agent-acp",
        &holding_lead,
    );
    install_fake_harness(data_dir.path(), FAKE_AGENT, "devin", COMMITTING_IMPLEMENTER);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    let url = serve_ui(&engine).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    github.add_repository(REPOSITORY);
    for (number, title, task) in [(12, "Desktop plans", 41), (13, "Phone plans", 42)] {
        github.add_issue(REPOSITORY, number, title);
        github.add_label(REPOSITORY, number, "mobius:workstream", "owner");
        github.add_issue(REPOSITORY, task, "Add plan model");
        github.add_sub_issue(REPOSITORY, number, task);
    }
    github.fail_close(REPOSITORY, 13);
    wait_for(async || (workstreams::list(&engine).await.unwrap().len() == 2).then_some(())).await;
    github.add_label(REPOSITORY, 41, "mobius:ready", "owner");
    let agents = async || {
        engine
            .store
            .sessions()
            .list("owner", REPOSITORY, 12)
            .await
            .unwrap()
    };
    wait_for(async || {
        let sessions = agents().await;
        (["lead_chat", "reviewer"].iter().all(|role| {
            sessions
                .iter()
                .any(|session| session.role == *role && session.acp_session_id.is_some())
        }))
        .then_some(())
    })
    .await;
    let (mut browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .launch_timeout(Duration::from_secs(60))
            .no_sandbox()
            .arg("--hide-scrollbars")
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    tokio::spawn(async move { while handler.next().await.is_some() {} });
    log_in(&browser, &format!("{url}/github")).await;
    for (viewport, number, title, task, closes) in [
        (DESKTOP, 12, "Desktop plans", 41, true),
        (PHONE, 13, "Phone plans", 42, false),
    ] {
        let page = open(
            &browser,
            &format!("{url}/workstreams/owner/shop/{number}"),
            viewport,
        )
        .await;
        wait_until_ready(&page, title, false).await;
        wait_until_live(&page).await;
        assert!(!check(&page, "!!document.querySelector('.closing')".to_string()).await);
        let done = format!(
            "document.querySelector('a[href=\"/workstreams/owner/shop/{number}\"] .chip')?.textContent === 'done'"
        );
        assert!(!check(&page, done.clone()).await);

        github.close_issue(REPOSITORY, task);

        wait_for(async || {
            check(&page, "!!document.querySelector('.closing')".to_string())
                .await
                .then_some(())
        })
        .await;
        wait_for(async || check(&page, done.clone()).await.then_some(())).await;
        assert!(
            check(
                &page,
                "(() => { const note = document.querySelector('.closing');\
                 const button = note.querySelector('button').getBoundingClientRect();\
                 return button.left >= 0 && button.right <= window.innerWidth \
                 && note.scrollWidth <= note.clientWidth; })()"
                    .to_string()
            )
            .await
        );
        if closes {
            // The pull request of the task is open, so only the closure of the Workstream ends the Reviewer.
            assert!(
                agents()
                    .await
                    .iter()
                    .any(|session| { session.role == "reviewer" && session.ended_at.is_none() })
            );
        }
        assert!(
            check(
                &page,
                "(() => { document.querySelector('.closing button').click(); return true; })()"
                    .to_string()
            )
            .await
        );
        let link =
            format!("!!document.querySelector('a[href=\"/workstreams/owner/shop/{number}\"]')");
        if closes {
            wait_for(async || (github.state(REPOSITORY, number).0 == "closed").then_some(())).await;
            wait_for(async || {
                agents()
                    .await
                    .iter()
                    .all(|session| session.ended_at.is_some())
                    .then_some(())
            })
            .await;
            for role in ["lead_chat", "reviewer"] {
                assert!(agents().await.iter().any(|session| {
                    session.role == role && session.end_reason.as_deref() == Some("stopped")
                }));
            }
            wait_for(async || {
                check(&page, "location.pathname === '/workstreams'".to_string())
                    .await
                    .then_some(())
            })
            .await;
            wait_for(async || (!check(&page, link.clone()).await).then_some(())).await;
            assert!(!check(&page, "!!document.querySelector('.closing')".to_string()).await);
        } else {
            wait_for(async || {
                check(
                    &page,
                    "!!document.querySelector('.closing .error')?.textContent".to_string(),
                )
                .await
                .then_some(())
            })
            .await;
            assert_eq!(github.state(REPOSITORY, number).0, "open");
            assert!(check(&page, link).await);
            assert!(check(&page, "!!document.querySelector('.closing')".to_string()).await);
        }
        page.close().await.unwrap();
    }
    browser.close().await.unwrap();
}

async fn set_offline(page: &Page, offline: bool) {
    page.execute(EmulateNetworkConditionsByRuleParams::new(
        offline,
        vec![NetworkConditions::new("", 0.0, -1.0, -1.0)],
    ))
    .await
    .unwrap();
}

// The server sends no live event for a message that the store gets while the connection is down.
#[tokio::test]
#[ignore = "starts Chrome and serves the web bundle in DIOXUS_PUBLIC_PATH"]
async fn the_open_chat_shows_the_messages_that_arrived_while_the_connection_was_down() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", CLAUDE);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    let url = serve_ui(&engine).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    wait_for(async || (workstreams::list(&engine).await.unwrap().len() == 1).then_some(())).await;
    let (mut browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .launch_timeout(Duration::from_secs(60))
            .no_sandbox()
            .arg("--hide-scrollbars")
            .user_data_dir(data_dir.path().join("chrome"))
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    tokio::spawn(async move { while handler.next().await.is_some() {} });
    log_in(&browser, &format!("{url}/github")).await;
    let page = open(&browser, &format!("{url}/workstreams/owner/shop/12"), PHONE).await;
    wait_until_live(&page).await;
    page.evaluate("window.__sameDocument = true").await.unwrap();
    set_offline(&page, true).await;
    engine
        .store
        .chat_messages()
        .add("owner", REPOSITORY, 12, Author::Lead, "Sent while offline.")
        .await
        .unwrap();
    set_offline(&page, false).await;
    let script = "document.querySelector('.msgs').textContent.includes('Sent while offline.') \
                  && window.__sameDocument === true"
        .to_string();
    // The rule does not change `navigator`, so the page gets the event that a browser sends when the network returns.
    // The page ignores the event until the first live connection exists, so each poll sends the event again.
    wait_for(async || {
        page.evaluate("window.dispatchEvent(new Event('online'))")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        check(&page, script.clone()).await.then_some(())
    })
    .await;
    page.close().await.unwrap();
    browser.close().await.unwrap();
}

#[tokio::test]
#[ignore = "starts Chrome and serves the web bundle in DIOXUS_PUBLIC_PATH"]
async fn resume_takes_the_issue_off_the_list_and_the_hint_goes_away() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    github.add_user_code("user-code", "owner");
    install_fake_harness(data_dir.path(), FAKE_AGENT, "claude-agent-acp", CLAUDE);
    install_fake_harness(data_dir.path(), FAKE_AGENT, "devin", IMPLEMENTER);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    let url = serve_ui(&engine).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    assert!(github::authorize_user(&engine, "user-code").await.unwrap());
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Desktop plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    wait_for(async || (workstreams::list(&engine).await.unwrap().len() == 1).then_some(())).await;
    let (mut browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .launch_timeout(Duration::from_secs(60))
            .no_sandbox()
            .arg("--hide-scrollbars")
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    tokio::spawn(async move { while handler.next().await.is_some() {} });
    log_in(&browser, &format!("{url}/github")).await;
    for (viewport, first, second) in [(DESKTOP, 41, 42), (PHONE, 43, 44)] {
        github.add_issue(
            REPOSITORY,
            first,
            "Add plan model with a long title that wraps on the phone",
        );
        github.add_issue(REPOSITORY, second, "Let customers change plans");
        for number in [first, second] {
            github.add_sub_issue(REPOSITORY, 12, number);
            github.add_label(REPOSITORY, number, "mobius:needs-human", "owner");
        }
        let page = open(
            &browser,
            &format!("{url}/workstreams/owner/shop/12"),
            viewport,
        )
        .await;
        wait_until_ready(&page, "Resume", false).await;
        wait_until_live(&page).await;
        // The list sits directly above the composer and fits the screen.
        assert!(
            check(
                &page,
                "(() => { const list = document.querySelector('.chat > .needs-human');\
                 const box = list.getBoundingClientRect();\
                 const buttons = [...list.querySelectorAll('button')].map(b => b.getBoundingClientRect());\
                 return list.nextElementSibling === document.querySelector('.chat > .composer')\
                 && list.querySelectorAll('.item').length === 2\
                 && box.left >= 0 && box.right <= window.innerWidth\
                 && buttons.every(b => b.left >= 0 && b.right <= window.innerWidth)\
                 && list.scrollWidth <= list.clientWidth; })()"
                    .to_string()
            )
            .await
        );
        let hint = "[...document.querySelectorAll('a.entry .chip.warn')].length";
        wait_for(async || check(&page, format!("{hint} === 1")).await.then_some(())).await;

        assert!(
            check(
                &page,
                "(() => { document.querySelector('.needs-human .item button').click(); return true; })()"
                    .to_string()
            )
            .await
        );
        wait_for(async || {
            check(
                &page,
                "document.querySelectorAll('.needs-human .item').length === 1".to_string(),
            )
            .await
            .then_some(())
        })
        .await;
        assert!(check(&page, format!("{hint} === 1")).await);
        wait_for(async || {
            engine
                .store
                .tasks()
                .live(REPOSITORY, first)
                .await
                .unwrap()
                .map(|_| ())
        })
        .await;

        assert!(
            check(
                &page,
                "(() => { document.querySelector('.needs-human .item button').click(); return true; })()"
                    .to_string()
            )
            .await
        );
        wait_for(async || {
            check(&page, "!document.querySelector('.needs-human')".to_string())
                .await
                .then_some(())
        })
        .await;
        wait_for(async || check(&page, format!("{hint} === 0")).await.then_some(())).await;
        wait_for(async || {
            engine
                .store
                .tasks()
                .live(REPOSITORY, second)
                .await
                .unwrap()
                .map(|_| ())
        })
        .await;
        page.close().await.unwrap();
    }
    browser.close().await.unwrap();
}
