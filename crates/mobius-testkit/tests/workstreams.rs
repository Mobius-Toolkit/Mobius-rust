use std::time::Duration;

use mobius_domain::{FeedRow, Live, Workstream};
use mobius_engine::activity::{self, Feed};
use mobius_engine::{Engine, github, workstreams};
use mobius_testkit::fake_github::FakeGitHub;
use mobius_testkit::{start, wait_for};
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

async fn next_row(feed: &mut Feed) -> FeedRow {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Live::Feed(row) = feed.next().await.unwrap() {
                return row;
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn a_workstream_label_shows_the_issue_in_the_workstream_list() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_issue(REPOSITORY, 13, "Fix the footer");

    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");

    let list = wait_for(async || {
        let list = workstreams::list(&engine).await.unwrap();
        (!list.is_empty()).then_some(list)
    })
    .await;
    assert_eq!(
        list,
        [Workstream {
            repository: REPOSITORY.to_string(),
            number: 12,
            title: "Integrate loyalty plans".to_string(),
            body: String::new(),
            autopilot: false,
            all_tasks_closed: false,
        }]
    );
}

#[tokio::test]
async fn a_new_workstream_adds_one_feed_row_that_goes_live() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    let mut feed = activity::feed(&engine, None).await.unwrap();
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_issue(REPOSITORY, 13, "Price rounding");

    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    let first = next_row(&mut feed).await;
    github.add_label(REPOSITORY, 13, "mobius:workstream", "Owner");
    let second = next_row(&mut feed).await;

    assert_eq!(first.repository, REPOSITORY);
    assert_eq!((first.workstream, first.issue), (12, 12));
    assert_eq!(first.actor, "owner");
    assert_eq!(first.text, "New Workstream \"Integrate loyalty plans\"");
    assert_eq!(first.link, "https://github.com/owner/shop/issues/12");
    assert_eq!(second.issue, 13);
    assert_eq!(
        engine.store.events().latest(100).await.unwrap(),
        [first, second]
    );
}

#[tokio::test]
async fn a_workstream_label_from_an_untrusted_author_adds_no_feed_row() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    let mut feed = activity::feed(&engine, None).await.unwrap();
    github.add_issue(REPOSITORY, 12, "Mine the servers");
    github.add_issue(REPOSITORY, 13, "Integrate loyalty plans");

    github.add_label(REPOSITORY, 12, "mobius:workstream", "mallory");
    github.add_label(REPOSITORY, 13, "mobius:workstream", "owner");

    assert_eq!(next_row(&mut feed).await.issue, 13);
}

#[tokio::test]
async fn a_feed_after_a_row_gives_the_rows_that_follow_it() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    let mut feed = activity::feed(&engine, None).await.unwrap();
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_issue(REPOSITORY, 13, "Price rounding");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    let first = next_row(&mut feed).await;
    github.add_label(REPOSITORY, 13, "mobius:workstream", "owner");
    let second = next_row(&mut feed).await;

    let mut resumed = activity::feed(&engine, Some(first.id)).await.unwrap();

    assert_eq!(next_row(&mut resumed).await, second);
}

#[tokio::test]
async fn a_workstream_label_from_before_the_first_poll_adds_a_feed_row() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");

    let engine = connect(&data_dir, &github).await;
    let mut feed = activity::feed(&engine, None).await.unwrap();

    assert_eq!(next_row(&mut feed).await.issue, 12);
}

#[tokio::test]
async fn a_body_edit_sends_workstreams_and_the_list_has_the_new_body() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    let mut feed = activity::feed(&engine, None).await.unwrap();
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(feed.next().await, Some(Live::Workstreams)) {}
    })
    .await
    .unwrap();

    github.set_body(REPOSITORY, 12, "Ship loyalty plans to all shops.");

    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(feed.next().await, Some(Live::Workstreams)) {}
    })
    .await
    .unwrap();
    let list = workstreams::list(&engine).await.unwrap();
    assert_eq!(list[0].body, "Ship loyalty plans to all shops.");
}

#[tokio::test]
async fn an_unchanged_repository_gets_not_modified_answers() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    let _engine = connect(&data_dir, &github).await;

    wait_for(async || (github.not_modified_count() > 0).then_some(())).await;
}

async fn all_tasks_closed(engine: &Engine) -> bool {
    workstreams::list(engine).await.unwrap()[0].all_tasks_closed
}

// Ends after the polls of the setup sent their last broadcast, so a later broadcast has a new cause.
async fn wait_until_quiet(feed: &mut Feed) {
    while tokio::time::timeout(Duration::from_millis(500), feed.next())
        .await
        .is_ok()
    {}
}

async fn workstream_with_task(engine: &Engine, github: &FakeGitHub) {
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    github.add_issue(REPOSITORY, 41, "Add plan model");
    github.add_sub_issue(REPOSITORY, 12, 41);
    wait_for(async || (!workstreams::list(engine).await.unwrap().is_empty()).then_some(())).await;
}

#[tokio::test]
async fn a_workstream_with_no_sub_issue_has_not_all_tasks_closed() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    github.add_issue(REPOSITORY, 12, "Integrate loyalty plans");
    github.add_label(REPOSITORY, 12, "mobius:workstream", "owner");
    wait_for(async || (!workstreams::list(&engine).await.unwrap().is_empty()).then_some(())).await;

    assert!(!all_tasks_closed(&engine).await);
}

#[tokio::test]
async fn a_workstream_with_an_open_sub_issue_has_not_all_tasks_closed() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    workstream_with_task(&engine, &github).await;
    github.add_issue(REPOSITORY, 42, "Add plan price");
    github.add_sub_issue(REPOSITORY, 12, 42);
    github.close_issue(REPOSITORY, 41);

    assert!(!all_tasks_closed(&engine).await);
}

#[tokio::test]
async fn a_workstream_with_only_closed_sub_issues_has_all_tasks_closed() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    workstream_with_task(&engine, &github).await;
    github.close_issue(REPOSITORY, 41);

    assert!(all_tasks_closed(&engine).await);
}

#[tokio::test]
async fn a_workstream_whose_sub_issues_cannot_be_read_stays_in_the_list() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    workstream_with_task(&engine, &github).await;
    github.close_issue(REPOSITORY, 41);
    github.fail_sub_issues(REPOSITORY, 12);

    let list = workstreams::list(&engine).await.unwrap();

    assert_eq!(list.len(), 1);
    assert_eq!(list[0].number, 12);
    assert!(!list[0].all_tasks_closed);
}

#[tokio::test]
async fn an_open_nested_sub_issue_does_not_count() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    workstream_with_task(&engine, &github).await;
    github.add_issue(REPOSITORY, 43, "Price table");
    github.add_sub_issue(REPOSITORY, 41, 43);
    github.close_issue(REPOSITORY, 41);

    assert!(all_tasks_closed(&engine).await);
}

#[tokio::test]
async fn the_close_of_a_sub_issue_goes_live_as_a_workstream_change() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    workstream_with_task(&engine, &github).await;
    let mut feed = activity::feed(&engine, None).await.unwrap();
    wait_until_quiet(&mut feed).await;
    github.close_issue(REPOSITORY, 41);

    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(feed.next().await, Some(Live::Workstreams)) {}
    })
    .await
    .unwrap();

    assert!(all_tasks_closed(&engine).await);
}

#[tokio::test]
async fn the_needs_human_label_on_a_nested_task_goes_live_as_a_workstream_change() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    workstream_with_task(&engine, &github).await;
    github.add_issue(REPOSITORY, 43, "Price table");
    github.add_sub_issue(REPOSITORY, 41, 43);
    let mut feed = activity::feed(&engine, None).await.unwrap();
    wait_until_quiet(&mut feed).await;
    github.add_label(REPOSITORY, 43, "mobius:needs-human", "mobius[bot]");

    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(feed.next().await, Some(Live::Workstreams)) {}
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_completion_closes_the_workstream_issue_and_leaves_the_list() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    workstream_with_task(&engine, &github).await;
    github.close_issue(REPOSITORY, 41);
    let mut feed = activity::feed(&engine, None).await.unwrap();

    workstreams::complete(&engine, REPOSITORY, 12)
        .await
        .unwrap();

    assert_eq!(
        github.state(REPOSITORY, 12),
        ("closed".to_string(), Some("completed".to_string()))
    );
    assert!(workstreams::list(&engine).await.unwrap().is_empty());
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(feed.next().await, Some(Live::Workstreams)) {}
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_failed_completion_returns_the_error_and_keeps_the_workstream_open() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    workstream_with_task(&engine, &github).await;
    github.close_issue(REPOSITORY, 41);
    github.fail_close(REPOSITORY, 12);

    let result = workstreams::complete(&engine, REPOSITORY, 12).await;

    assert!(result.is_err());
    assert_eq!(github.state(REPOSITORY, 12), ("open".to_string(), None));
    assert_eq!(workstreams::list(&engine).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_completion_with_an_open_sub_issue_closes_nothing() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    workstream_with_task(&engine, &github).await;

    let result = workstreams::complete(&engine, REPOSITORY, 12).await;

    assert!(result.is_err());
    assert_eq!(github.state(REPOSITORY, 12), ("open".to_string(), None));
    assert_eq!(github.state(REPOSITORY, 41), ("open".to_string(), None));
}

#[tokio::test]
async fn a_completion_of_an_issue_without_the_workstream_label_closes_nothing() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    let engine = connect(&data_dir, &github).await;
    github.add_issue(REPOSITORY, 13, "Fix the footer");
    github.add_issue(REPOSITORY, 42, "Round the price");
    github.add_sub_issue(REPOSITORY, 13, 42);
    github.close_issue(REPOSITORY, 42);

    let result = workstreams::complete(&engine, REPOSITORY, 13).await;

    assert!(result.is_err());
    assert_eq!(github.state(REPOSITORY, 13), ("open".to_string(), None));
}
