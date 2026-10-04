use std::time::Duration;

use mobius_domain::Live;
use mobius_engine::activity::{self, Feed};
use mobius_engine::{Engine, github};
use mobius_store::CopiedWorkstream;
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::{start, wait_for, wait_for_first_poll};
use tempfile::TempDir;

const REPOSITORY: &str = "owner/shop";

async fn connect(data_dir: &TempDir, github: &FakeGitHub) -> Engine {
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();
    engine
}

// (number, title, body, autopilot) of each copied Workstream.
async fn workstreams(engine: &Engine) -> Vec<(i64, String, String, bool)> {
    wait_for(async || {
        let rows: Vec<(i64, String, String, bool)> = sqlx::query_as(
            "SELECT number, title, body, autopilot FROM copied_workstreams
             WHERE repository = ? ORDER BY number",
        )
        .bind(REPOSITORY)
        .fetch_all(&engine.store.pool)
        .await
        .unwrap();
        (!rows.is_empty()).then_some(rows)
    })
    .await
}

// (workstream, number, parent, state, author) of each copied issue in the walk order.
async fn issues(engine: &Engine) -> Vec<(i64, i64, i64, String, String)> {
    sqlx::query_as(
        "SELECT workstream, number, parent, state, author FROM copied_issues
         WHERE repository = ? ORDER BY workstream, position",
    )
    .bind(REPOSITORY)
    .fetch_all(&engine.store.pool)
    .await
    .unwrap()
}

async fn labels(engine: &Engine, number: i64) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT l.name FROM copied_issue_labels l
         JOIN copied_issues i USING (repository, workstream, position)
         WHERE i.repository = ? AND i.number = ? ORDER BY l.name",
    )
    .bind(REPOSITORY)
    .bind(number)
    .fetch_all(&engine.store.pool)
    .await
    .unwrap()
}

// (issue, blocker, workstream of the blocker, title of that Workstream)
async fn blockers(engine: &Engine) -> Vec<(i64, i64, Option<i64>, Option<String>)> {
    sqlx::query_as(
        "SELECT i.number, b.number, b.blocker_workstream, b.blocker_workstream_title
         FROM copied_blockers b
         JOIN copied_issues i USING (repository, workstream, position)
         WHERE b.repository = ? ORDER BY i.number, b.number",
    )
    .bind(REPOSITORY)
    .fetch_all(&engine.store.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn the_full_sync_copies_the_open_workstreams() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.set_body(REPOSITORY, 12, "Brief of the plans");
    github.add_issue(REPOSITORY, 13, "Fix the footer");
    github.add_issue(REPOSITORY, 14, "Price rounding");
    github.add_label(REPOSITORY, 14, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 15, "Old Workstream");
    github.add_label(REPOSITORY, 15, "mobius:workstream", "owner");
    github.close_issue(REPOSITORY, 15);

    let engine = connect(&data_dir, &github).await;

    assert_eq!(
        workstreams(&engine).await,
        [
            (
                12,
                "Integrate loyalty plans".to_string(),
                "Brief of the plans".to_string(),
                false
            ),
            (14, "Price rounding".to_string(), String::new(), false),
        ]
    );
}

#[tokio::test]
async fn the_full_sync_copies_autopilot_only_from_a_trusted_user() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    for (number, actor) in [(12, "owner"), (13, "mallory")] {
        github.add_issue(REPOSITORY, number, "Workstream");
        github.add_label(REPOSITORY, number, "mobius:workstream", "owner");
        github.add_label(REPOSITORY, number, "mobius:autopilot", actor);
    }
    github.add_issue(REPOSITORY, 14, "Workstream");
    github.add_label(REPOSITORY, 14, "mobius:workstream", "owner");

    let engine = connect(&data_dir, &github).await;

    let autopilot: Vec<(i64, bool)> = workstreams(&engine)
        .await
        .into_iter()
        .map(|(number, _, _, autopilot)| (number, autopilot))
        .collect();
    assert_eq!(autopilot, [(12, true), (13, false), (14, false)]);
}

#[tokio::test]
async fn the_full_sync_copies_the_nested_sub_issues_with_state_labels_and_author() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_label(REPOSITORY, 41, "enhancement", "owner");
    github.add_label(REPOSITORY, 41, "bug", "owner");
    github.add_sub_issue(REPOSITORY, 12, 41);
    github.add_issue(REPOSITORY, 50, "Store the price in cents");
    github.add_sub_issue(REPOSITORY, 41, 50);
    github.add_issue(REPOSITORY, 42, "Let customers change plans");
    github.close_issue(REPOSITORY, 42);
    github.add_sub_issue(REPOSITORY, 12, 42);
    github.add_issue(REPOSITORY, 43, "Mine the servers");
    github.set_author(REPOSITORY, 43, "mallory");
    github.add_sub_issue(REPOSITORY, 12, 43);
    github.add_issue(REPOSITORY, 51, "Task below the untrusted issue");
    github.add_sub_issue(REPOSITORY, 43, 51);

    let engine = connect(&data_dir, &github).await;

    workstreams(&engine).await;
    let open = || "open".to_string();
    let owner = || "owner".to_string();
    assert_eq!(
        issues(&engine).await,
        [
            (12, 41, 12, open(), owner()),
            (12, 50, 41, open(), owner()),
            (12, 42, 12, "closed".to_string(), owner()),
            (12, 43, 12, open(), "mallory".to_string()),
            (12, 51, 43, open(), owner()),
        ]
    );
    assert_eq!(labels(&engine, 41).await, ["bug", "enhancement"]);
    assert_eq!(labels(&engine, 50).await, Vec::<String>::new());
}

#[tokio::test]
async fn the_full_sync_copies_the_open_blockers_with_the_workstream_of_each_blocker() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 13, "Billing");
    github.add_label(REPOSITORY, 13, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);
    github.add_issue(REPOSITORY, 42, "Let customers change plans");
    github.add_sub_issue(REPOSITORY, 12, 42);
    github.add_issue(REPOSITORY, 43, "A closed blocker");
    github.close_issue(REPOSITORY, 43);
    github.add_sub_issue(REPOSITORY, 12, 43);
    github.add_issue(REPOSITORY, 60, "Invoice model");
    github.add_sub_issue(REPOSITORY, 13, 60);
    github.add_issue(REPOSITORY, 61, "An issue of no Workstream");
    github.add_issue("other/repo", 70, "A blocker in another repository");
    github.add_blocker(REPOSITORY, 41, 42);
    github.add_blocker(REPOSITORY, 41, 60);
    github.add_blocker(REPOSITORY, 41, 61);
    github.add_blocker(REPOSITORY, 41, 43);
    github.add_blocker(REPOSITORY, 42, 60);

    let engine = connect(&data_dir, &github).await;

    workstreams(&engine).await;
    let billing = || Some("Billing".to_string());
    assert_eq!(
        blockers(&engine).await,
        [
            (
                41,
                42,
                Some(12),
                Some("Integrate loyalty plans".to_string())
            ),
            (41, 60, Some(13), billing()),
            (41, 61, None, None),
            (42, 60, Some(13), billing()),
        ]
    );
}

#[tokio::test]
async fn the_full_sync_copies_a_sub_issue_of_another_repository_as_a_leaf() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue("other/repo", 70, "A blocker in another repository");
    github.add_issue("other/repo", 77, "A task in another repository");
    github.add_blocker("other/repo", 77, 70);
    github.add_issue("other/repo", 78, "A child in another repository");
    github.add_foreign_sub_issue(REPOSITORY, 12, "other/repo", 77);
    github.add_foreign_sub_issue("other/repo", 77, "other/repo", 78);
    // The different issue 77 of this repository has a child and a blocker.
    github.add_issue(REPOSITORY, 77, "A different issue");
    github.add_issue(REPOSITORY, 90, "Child of the different issue");
    github.add_sub_issue(REPOSITORY, 77, 90);
    github.add_blocker(REPOSITORY, 77, 90);

    let engine = connect(&data_dir, &github).await;

    workstreams(&engine).await;
    assert_eq!(
        issues(&engine).await,
        [(12, 77, 12, "open".to_string(), "owner".to_string())]
    );
    let repository_url: String = sqlx::query_scalar(
        "SELECT repository_url FROM copied_issues WHERE repository = ? AND number = 77",
    )
    .bind(REPOSITORY)
    .fetch_one(&engine.store.pool)
    .await
    .unwrap();
    assert!(
        repository_url.ends_with("/repos/other/repo"),
        "{repository_url}"
    );
    assert_eq!(blockers(&engine).await, []);
}

#[tokio::test]
async fn the_full_sync_forgets_the_rows_of_a_repository_that_is_not_polled() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    engine
        .store
        .workstream_copy()
        .replace(
            "owner/gone",
            &[CopiedWorkstream {
                number: 5,
                title: "An old Workstream".to_string(),
                body: String::new(),
                autopilot: false,
                issues: Vec::new(),
            }],
        )
        .await
        .unwrap();

    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();

    workstreams(&engine).await;
    let stale: Vec<i64> =
        sqlx::query_scalar("SELECT number FROM copied_workstreams WHERE repository = 'owner/gone'")
            .fetch_all(&engine.store.pool)
            .await
            .unwrap();
    assert!(stale.is_empty());
}

async fn workstream_numbers(engine: &Engine) -> Vec<i64> {
    sqlx::query_scalar("SELECT number FROM copied_workstreams WHERE repository = ? ORDER BY number")
        .bind(REPOSITORY)
        .fetch_all(&engine.store.pool)
        .await
        .unwrap()
}

async fn title(engine: &Engine, number: i64) -> String {
    sqlx::query_scalar("SELECT title FROM copied_issues WHERE repository = ? AND number = ?")
        .bind(REPOSITORY)
        .bind(number)
        .fetch_one(&engine.store.pool)
        .await
        .unwrap()
}

async fn issue_numbers(engine: &Engine) -> Vec<i64> {
    issues(engine)
        .await
        .into_iter()
        .map(|(_, number, ..)| number)
        .collect()
}

// Workstream 12 has the task 41, and 41 has the task 50.
async fn copied_workstream(data_dir: &TempDir, github: &FakeGitHub) -> Engine {
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);
    github.add_issue(REPOSITORY, 50, "Store the price in cents");
    github.add_sub_issue(REPOSITORY, 41, 50);
    let engine = connect(data_dir, github).await;
    workstreams(&engine).await;
    wait_for_first_poll(&engine, REPOSITORY).await;
    engine
}

async fn issues_cursor(engine: &Engine) -> Option<time::OffsetDateTime> {
    engine
        .store
        .sync_cursors()
        .get(REPOSITORY, "issues")
        .await
        .unwrap()
        .since
}

// Ends after the poll processed the changes that the test made before.
async fn wait_for_poll_after(engine: &Engine, since: Option<time::OffsetDateTime>) {
    wait_for(async || (issues_cursor(engine).await > since).then_some(())).await;
}

async fn next_workstreams(feed: &mut Feed) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while feed.next().await.unwrap() != Live::Workstreams {}
    })
    .await
    .unwrap();
}

async fn wait_until_quiet(feed: &mut Feed) {
    while tokio::time::timeout(Duration::from_millis(500), feed.next())
        .await
        .is_ok()
    {}
}

#[tokio::test]
async fn a_new_issue_under_a_workstream_is_copied_in_the_walk_order() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = copied_workstream(&data_dir, &github).await;

    github.add_sub_issue_of(REPOSITORY, 12, 42, "Let customers change plans");

    wait_for(async || (issue_numbers(&engine).await == [41, 50, 42]).then_some(())).await;
}

#[tokio::test]
async fn a_new_issue_under_a_nested_task_is_copied_below_its_parent() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = copied_workstream(&data_dir, &github).await;
    github.add_sub_issue_of(REPOSITORY, 12, 42, "Let customers change plans");
    wait_for(async || (issue_numbers(&engine).await == [41, 50, 42]).then_some(())).await;

    github.add_sub_issue_of(REPOSITORY, 41, 51, "Round the price");

    wait_for(async || (issue_numbers(&engine).await == [41, 50, 51, 42]).then_some(())).await;
    let parents: Vec<i64> = issues(&engine)
        .await
        .into_iter()
        .map(|(_, _, parent, ..)| parent)
        .collect();
    assert_eq!(parents, [12, 41, 41, 12]);
}

#[tokio::test]
async fn a_new_label_and_a_removed_label_change_the_labels_of_the_copied_issue() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = copied_workstream(&data_dir, &github).await;

    github.add_label(REPOSITORY, 41, "bug", "owner");
    github.add_label(REPOSITORY, 41, "mobius:working", "owner");

    wait_for(async || (labels(&engine, 41).await == ["bug", "mobius:working"]).then_some(())).await;

    github.remove_label(REPOSITORY, 41, "bug", "owner");

    wait_for(async || (labels(&engine, 41).await == ["mobius:working"]).then_some(())).await;
}

#[tokio::test]
async fn a_closed_issue_has_the_state_closed_in_the_copy() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = copied_workstream(&data_dir, &github).await;

    github.close_issue(REPOSITORY, 41);

    wait_for(async || {
        let states: Vec<String> = issues(&engine)
            .await
            .into_iter()
            .map(|(_, _, _, state, _)| state)
            .collect();
        (states == ["closed", "open"]).then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_new_title_and_a_new_body_change_the_copied_issue_and_the_copied_workstream() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = copied_workstream(&data_dir, &github).await;

    github.set_title(REPOSITORY, 41, "Add the plan table");
    github.set_title(REPOSITORY, 12, "Integrate loyalty tiers");
    github.set_body(REPOSITORY, 12, "Brief of the tiers");

    wait_for(async || (title(&engine, 41).await == "Add the plan table").then_some(())).await;
    wait_for(async || {
        let rows = workstreams(&engine).await;
        (rows[0].1 == "Integrate loyalty tiers" && rows[0].2 == "Brief of the tiers").then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_new_workstream_is_copied_with_its_tree() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = copied_workstream(&data_dir, &github).await;
    github.add_issue(REPOSITORY, 13, "Billing");
    github.add_sub_issue_of(REPOSITORY, 13, 60, "Invoice model");

    github.add_label(REPOSITORY, 13, "mobius:workstream", "owner");

    wait_for(async || (workstream_numbers(&engine).await == [12, 13]).then_some(())).await;
    assert_eq!(
        issues(&engine).await[2],
        (13, 60, 13, "open".to_string(), "owner".to_string())
    );
}

#[tokio::test]
async fn a_closed_workstream_is_removed_with_its_tree() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = copied_workstream(&data_dir, &github).await;

    github.close_issue(REPOSITORY, 12);

    wait_for(async || workstream_numbers(&engine).await.is_empty().then_some(())).await;
    assert_eq!(issue_numbers(&engine).await, Vec::<i64>::new());
    assert_eq!(labels(&engine, 41).await, Vec::<String>::new());
}

#[tokio::test]
async fn a_workstream_that_loses_the_label_is_removed_with_its_tree() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = copied_workstream(&data_dir, &github).await;
    github.add_label(REPOSITORY, 41, "bug", "owner");
    wait_for(async || (labels(&engine, 41).await == ["bug"]).then_some(())).await;

    github.remove_label(REPOSITORY, 12, "mobius:workstream", "owner");

    wait_for(async || workstream_numbers(&engine).await.is_empty().then_some(())).await;
    assert_eq!(issue_numbers(&engine).await, Vec::<i64>::new());
    assert_eq!(labels(&engine, 41).await, Vec::<String>::new());
}

#[tokio::test]
async fn autopilot_is_on_after_a_trusted_user_adds_the_label_and_off_after_the_removal() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = copied_workstream(&data_dir, &github).await;
    let autopilot = || async {
        let rows = workstreams(&engine).await;
        rows[0].3
    };

    github.add_label(REPOSITORY, 12, "mobius:autopilot", "owner");

    wait_for(async || autopilot().await.then_some(())).await;

    github.remove_label(REPOSITORY, 12, "mobius:autopilot", "owner");

    wait_for(async || (!autopilot().await).then_some(())).await;
}

#[tokio::test]
async fn autopilot_stays_off_after_an_untrusted_user_adds_the_label() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = copied_workstream(&data_dir, &github).await;
    let before = issues_cursor(&engine).await;

    github.add_label(REPOSITORY, 12, "mobius:autopilot", "mallory");

    wait_for_poll_after(&engine, before).await;
    assert!(!workstreams(&engine).await[0].3);
}

#[tokio::test]
async fn a_change_of_a_local_issue_does_not_change_the_foreign_row_with_the_same_number() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue("other/repo", 77, "A task in another repository");
    github.add_foreign_sub_issue(REPOSITORY, 12, "other/repo", 77);
    github.add_issue(REPOSITORY, 77, "A different issue");
    let engine = connect(&data_dir, &github).await;
    workstreams(&engine).await;
    wait_for_first_poll(&engine, REPOSITORY).await;
    let before = issues_cursor(&engine).await;

    github.set_title(REPOSITORY, 77, "A renamed issue");

    wait_for_poll_after(&engine, before).await;
    assert_eq!(title(&engine, 77).await, "A task in another repository");
}

#[tokio::test]
async fn a_task_that_gets_the_workstream_label_becomes_a_leaf_of_its_workstream() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = copied_workstream(&data_dir, &github).await;

    github.add_label(REPOSITORY, 41, "mobius:workstream", "owner");

    wait_for(async || (workstream_numbers(&engine).await == [12, 41]).then_some(())).await;
    wait_for(async || (issue_numbers(&engine).await == [41, 50]).then_some(())).await;
    let owners: Vec<i64> = issues(&engine)
        .await
        .into_iter()
        .map(|(workstream, ..)| workstream)
        .collect();
    assert_eq!(owners, [12, 41]);
}

#[tokio::test]
async fn a_task_that_loses_the_workstream_label_gets_its_tree_back_in_its_workstream() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_label(REPOSITORY, 41, "mobius:workstream", "owner");
    github.add_sub_issue(REPOSITORY, 12, 41);
    github.add_issue(REPOSITORY, 50, "Store the price in cents");
    github.add_sub_issue(REPOSITORY, 41, 50);
    let engine = connect(&data_dir, &github).await;
    wait_for(async || (workstream_numbers(&engine).await == [12, 41]).then_some(())).await;
    wait_for_first_poll(&engine, REPOSITORY).await;
    assert_eq!(issue_numbers(&engine).await, [41, 50]);

    github.remove_label(REPOSITORY, 41, "mobius:workstream", "owner");

    wait_for(async || (workstream_numbers(&engine).await == [12]).then_some(())).await;
    wait_for(async || (issue_numbers(&engine).await == [41, 50]).then_some(())).await;
    let parents: Vec<(i64, i64)> = issues(&engine)
        .await
        .into_iter()
        .map(|(workstream, _, parent, ..)| (workstream, parent))
        .collect();
    assert_eq!(parents, [(12, 12), (12, 41)]);
}

#[tokio::test]
async fn a_closed_issue_of_another_repository_has_the_state_closed_in_the_copy() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_repository("other/repo");
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue("other/repo", 77, "A task in another repository");
    github.add_foreign_sub_issue(REPOSITORY, 12, "other/repo", 77);
    let engine = connect(&data_dir, &github).await;
    workstreams(&engine).await;
    wait_for_first_poll(&engine, REPOSITORY).await;
    wait_for_first_poll(&engine, "other/repo").await;

    github.close_issue("other/repo", 77);

    wait_for(async || {
        let states: Vec<String> = issues(&engine)
            .await
            .into_iter()
            .map(|(_, _, _, state, _)| state)
            .collect();
        (states == ["closed"]).then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_poll_with_no_change_in_the_copy_sends_no_workstreams_event() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = copied_workstream(&data_dir, &github).await;
    let mut feed = activity::feed(&engine, None).await.unwrap();
    wait_until_quiet(&mut feed).await;
    let before = issues_cursor(&engine).await;

    github.add_issue(REPOSITORY, 13, "Fix the footer");
    github.add_comment(
        REPOSITORY,
        50,
        "owner",
        "A comment changes no copied value.",
    );

    wait_for_poll_after(&engine, before).await;
    while let Ok(live) = tokio::time::timeout(Duration::from_millis(300), feed.next()).await {
        assert_ne!(live.unwrap(), Live::Workstreams);
    }
}

#[tokio::test]
async fn a_change_in_the_copy_sends_a_workstreams_event() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = copied_workstream(&data_dir, &github).await;
    let mut feed = activity::feed(&engine, None).await.unwrap();
    wait_until_quiet(&mut feed).await;

    github.set_title(REPOSITORY, 41, "Add the plan table");

    next_workstreams(&mut feed).await;
    assert_eq!(title(&engine, 41).await, "Add the plan table");
}

#[tokio::test]
async fn a_failed_update_of_the_copy_does_not_repeat_the_events_of_the_poll() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = copied_workstream(&data_dir, &github).await;
    github.add_issue(REPOSITORY, 13, "Billing");
    let before = issues_cursor(&engine).await;
    github.fail_sub_issues(REPOSITORY, 12);

    github.add_label(REPOSITORY, 13, "mobius:workstream", "owner");
    github.add_sub_issue_of(REPOSITORY, 12, 42, "Let customers change plans");

    wait_for_poll_after(&engine, before).await;
    let rows = engine.store.events().latest(100).await.unwrap();
    let new_workstreams = rows
        .iter()
        .filter(|row| row.text == "New Workstream \"Billing\"")
        .count();
    assert_eq!(new_workstreams, 1);
}

// Task 42 of Workstream 12 is blocked by task 60 of Workstream 13.
async fn copied_blocker(data_dir: &TempDir, github: &FakeGitHub) -> Engine {
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 13, "Billing");
    github.add_label(REPOSITORY, 13, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 42, "Let customers change plans");
    github.add_sub_issue(REPOSITORY, 12, 42);
    github.add_issue(REPOSITORY, 60, "Invoice model");
    github.add_sub_issue(REPOSITORY, 13, 60);
    github.add_blocker(REPOSITORY, 42, 60);
    let engine = connect(data_dir, github).await;
    workstreams(&engine).await;
    wait_for_first_poll(&engine, REPOSITORY).await;
    engine
}

#[tokio::test]
async fn a_closed_blocker_is_removed_from_the_copy() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = copied_blocker(&data_dir, &github).await;
    assert_eq!(blockers(&engine).await.len(), 1);

    github.close_issue(REPOSITORY, 60);

    wait_for(async || blockers(&engine).await.is_empty().then_some(())).await;
}

#[tokio::test]
async fn a_new_title_of_a_workstream_changes_the_title_in_the_blockers_of_the_copy() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = copied_blocker(&data_dir, &github).await;

    github.set_title(REPOSITORY, 13, "Invoices");

    wait_for(async || {
        (blockers(&engine).await == [(42, 60, Some(13), Some("Invoices".to_string()))])
            .then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_repository_without_a_copy_sends_a_workstreams_event_for_a_new_workstream_label() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = copied_workstream(&data_dir, &github).await;
    let mut feed = activity::feed(&engine, None).await.unwrap();
    github.add_issue(REPOSITORY, 13, "Billing");
    github.fail_sub_issues(REPOSITORY, 12);
    github.add_sub_issue_of(REPOSITORY, 12, 42, "Let customers change plans");
    next_workstreams(&mut feed).await;
    wait_until_quiet(&mut feed).await;

    github.add_label(REPOSITORY, 13, "mobius:workstream", "owner");

    next_workstreams(&mut feed).await;
}

// Task 60 of Workstream 20 is blocked by task 50, and 50 is in the tree of 41 in Workstream 12.
async fn copied_blocker_below_a_task(data_dir: &TempDir, github: &FakeGitHub) -> Engine {
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);
    github.add_issue(REPOSITORY, 50, "Store the price in cents");
    github.add_sub_issue(REPOSITORY, 41, 50);
    github.add_issue(REPOSITORY, 20, "Billing");
    github.add_label(REPOSITORY, 20, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 60, "Invoice model");
    github.add_sub_issue(REPOSITORY, 20, 60);
    github.add_blocker(REPOSITORY, 60, 50);
    let engine = connect(data_dir, github).await;
    workstreams(&engine).await;
    wait_for_first_poll(&engine, REPOSITORY).await;
    engine
}

#[tokio::test]
async fn a_task_that_gets_the_workstream_label_changes_the_workstream_of_a_blocker_in_another_tree()
{
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = copied_blocker_below_a_task(&data_dir, &github).await;
    assert_eq!(
        blockers(&engine).await,
        [(
            60,
            50,
            Some(12),
            Some("Integrate loyalty plans".to_string())
        )]
    );

    github.add_label(REPOSITORY, 41, "mobius:workstream", "owner");

    wait_for(async || {
        (blockers(&engine).await == [(60, 50, Some(41), Some("Add plan model".to_string()))])
            .then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_task_that_loses_the_workstream_label_changes_the_workstream_of_a_blocker_in_another_tree()
 {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = copied_blocker_below_a_task(&data_dir, &github).await;
    github.add_label(REPOSITORY, 41, "mobius:workstream", "owner");
    wait_for(async || {
        (blockers(&engine).await == [(60, 50, Some(41), Some("Add plan model".to_string()))])
            .then_some(())
    })
    .await;

    github.remove_label(REPOSITORY, 41, "mobius:workstream", "owner");

    wait_for(async || {
        (blockers(&engine).await
            == [(
                60,
                50,
                Some(12),
                Some("Integrate loyalty plans".to_string()),
            )])
        .then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_new_workstream_changes_the_workstream_of_a_blocker_in_another_tree() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_issue(REPOSITORY, 50, "Store the price in cents");
    github.add_sub_issue(REPOSITORY, 41, 50);
    github.add_issue(REPOSITORY, 20, "Billing");
    github.add_label(REPOSITORY, 20, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 60, "Invoice model");
    github.add_sub_issue(REPOSITORY, 20, 60);
    github.add_blocker(REPOSITORY, 60, 50);
    let engine = connect(&data_dir, &github).await;
    workstreams(&engine).await;
    wait_for_first_poll(&engine, REPOSITORY).await;
    assert_eq!(blockers(&engine).await, [(60, 50, None, None)]);

    github.add_label(REPOSITORY, 41, "mobius:workstream", "owner");

    wait_for(async || {
        (blockers(&engine).await == [(60, 50, Some(41), Some("Add plan model".to_string()))])
            .then_some(())
    })
    .await;
}
