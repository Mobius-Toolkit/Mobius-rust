use mobius_engine::github;
use mobius_engine::labels::{self, LabelStatus};
use mobius_testkit::fake_github::{self, FakeGitHub};
use mobius_testkit::{start, wait_for};
use tempfile::TempDir;

const REPOSITORY: &str = "owner/shop";

#[tokio::test]
async fn fix_creates_the_missing_labels_and_sets_the_fixed_colors() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_repository(REPOSITORY);
    // `mobius:ready` has the fixed color in lowercase and its own description.
    github.add_repository_label(
        REPOSITORY,
        "mobius:ready",
        "0e8a16",
        "Ready, says the Owner",
    );
    // `mobius:working` has a different color and its own description.
    github.add_repository_label(REPOSITORY, "mobius:working", "ededed", "Custom description");
    // `bug` is not a Mobius label.
    github.add_repository_label(REPOSITORY, "bug", "d73a4a", "Something is wrong");
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    let repository = engine
        .github
        .repositories(
            fake_github::APP_ID,
            fake_github::APP_SLUG,
            fake_github::APP_PRIVATE_KEY,
        )
        .await
        .unwrap()
        .remove(0);

    let status: Vec<(&str, LabelStatus)> = labels::status(&repository)
        .await
        .unwrap()
        .iter()
        .map(|entry| (entry.label.name, entry.status.clone()))
        .collect();
    assert_eq!(
        status,
        [
            ("mobius:workstream", LabelStatus::Missing),
            ("mobius:autopilot", LabelStatus::Missing),
            ("mobius:ready", LabelStatus::Present),
            (
                "mobius:working",
                LabelStatus::WrongColor("ededed".to_string())
            ),
            ("mobius:needs-human", LabelStatus::Missing),
            ("mobius:no-workstream", LabelStatus::Missing),
        ]
    );

    labels::fix(&repository).await.unwrap();

    let labels: Vec<(String, String, String)> = github
        .repository_labels(REPOSITORY)
        .into_iter()
        .map(|label| (label.name, label.color, label.description))
        .collect();
    assert_eq!(
        labels,
        [
            ("bug", "d73a4a", "Something is wrong"),
            (
                "mobius:autopilot",
                "1D76DB",
                "Mobius dispatches the ready tasks of this Workstream"
            ),
            (
                "mobius:needs-human",
                "D93F0B",
                "Mobius waits for an answer from a human"
            ),
            (
                "mobius:no-workstream",
                "BFD4F2",
                "The Triager found no Workstream for this issue"
            ),
            // The correct color and the description stayed.
            ("mobius:ready", "0e8a16", "Ready, says the Owner"),
            // The color became the fixed color, the description stayed.
            ("mobius:working", "FBCA04", "Custom description"),
            (
                "mobius:workstream",
                "5319E7",
                "Mobius Workstream: a parent issue for a group of tasks"
            ),
        ]
        .map(|(name, color, description)| {
            (name.to_string(), color.to_string(), description.to_string())
        })
    );
    // Only `mobius:working` got a PATCH request. The label with the correct color got none.
    assert_eq!(github.label_patches(REPOSITORY), ["mobius:working"]);
    assert!(
        labels::status(&repository)
            .await
            .unwrap()
            .iter()
            .all(|entry| entry.status == LabelStatus::Present)
    );
}

// The poll fixes the labels of a managed repository at its first sight, one time in a run of the engine.
#[tokio::test]
async fn a_poll_fixes_the_labels_of_a_managed_repository_one_time() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_manifest_code("manifest-code");
    github.add_repository(REPOSITORY);
    // `mobius:working` has a wrong color, and `bug` is not a Mobius label.
    github.add_repository_label(REPOSITORY, "mobius:working", "ededed", "Custom description");
    github.add_repository_label(REPOSITORY, "bug", "d73a4a", "Something is wrong");
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    github::convert_manifest(&engine, "manifest-code")
        .await
        .unwrap();

    let labels = wait_for(async || {
        let labels = github.repository_labels(REPOSITORY);
        (labels.len() == 7).then_some(labels)
    })
    .await;
    let labels: Vec<(String, String, String)> = labels
        .into_iter()
        .map(|label| (label.name, label.color, label.description))
        .collect();
    assert_eq!(
        labels,
        [
            ("bug", "d73a4a", "Something is wrong"),
            (
                "mobius:autopilot",
                "1D76DB",
                "Mobius dispatches the ready tasks of this Workstream"
            ),
            (
                "mobius:needs-human",
                "D93F0B",
                "Mobius waits for an answer from a human"
            ),
            (
                "mobius:no-workstream",
                "BFD4F2",
                "The Triager found no Workstream for this issue"
            ),
            ("mobius:ready", "0E8A16", "Mobius can dispatch this task"),
            // The color became the fixed color, the description stayed.
            ("mobius:working", "FBCA04", "Custom description"),
            (
                "mobius:workstream",
                "5319E7",
                "Mobius Workstream: a parent issue for a group of tasks"
            ),
        ]
        .map(|(name, color, description)| {
            (name.to_string(), color.to_string(), description.to_string())
        })
    );
    assert_eq!(github.label_patches(REPOSITORY), ["mobius:working"]);

    // The next polls see the repository again, but the done set keeps `fix` away from it.
    let polls = github.not_modified_count();
    github.delete_repository_label(REPOSITORY, "mobius:ready");
    wait_for(async || (github.not_modified_count() >= polls + 2).then_some(())).await;
    let labels = github.repository_labels(REPOSITORY);
    assert_eq!(labels.len(), 6);
    assert!(labels.iter().all(|label| label.name != "mobius:ready"));
    assert_eq!(github.label_patches(REPOSITORY), ["mobius:working"]);
}

// GitHub compares label names without regard to case, so a POST for a label in a
// different case fails. The engine does not see such a label on issues, because it
// compares names exactly, so the status is not `Present`. `fix` leaves the label for
// a human, because Mobius does not rename labels.
#[tokio::test]
async fn fix_skips_a_label_with_a_name_in_a_different_case() {
    let data_dir = TempDir::new().unwrap();
    let github = FakeGitHub::start().await;
    github.add_repository(REPOSITORY);
    // `Mobius:Ready` has the fixed color, `MOBIUS:WORKING` has a different color.
    github.add_repository_label(REPOSITORY, "Mobius:Ready", "0E8A16", "Ready");
    github.add_repository_label(REPOSITORY, "MOBIUS:WORKING", "ededed", "Working");
    let engine = start(data_dir.path(), "correct horse", &github.url).await;
    let repository = engine
        .github
        .repositories(
            fake_github::APP_ID,
            fake_github::APP_SLUG,
            fake_github::APP_PRIVATE_KEY,
        )
        .await
        .unwrap()
        .remove(0);

    let status: Vec<(&str, LabelStatus)> = labels::status(&repository)
        .await
        .unwrap()
        .iter()
        .map(|entry| (entry.label.name, entry.status.clone()))
        .collect();
    assert_eq!(
        status,
        [
            ("mobius:workstream", LabelStatus::Missing),
            ("mobius:autopilot", LabelStatus::Missing),
            (
                "mobius:ready",
                LabelStatus::WrongCase("Mobius:Ready".to_string())
            ),
            (
                "mobius:working",
                LabelStatus::WrongCase("MOBIUS:WORKING".to_string())
            ),
            ("mobius:needs-human", LabelStatus::Missing),
            ("mobius:no-workstream", LabelStatus::Missing),
        ]
    );

    labels::fix(&repository).await.unwrap();

    let labels: Vec<(String, String)> = github
        .repository_labels(REPOSITORY)
        .into_iter()
        .map(|label| (label.name, label.color))
        .collect();
    // The name, the color, and the description of each label in a different case stayed.
    assert_eq!(
        labels,
        [
            ("MOBIUS:WORKING", "ededed"),
            ("Mobius:Ready", "0E8A16"),
            ("mobius:autopilot", "1D76DB"),
            ("mobius:needs-human", "D93F0B"),
            ("mobius:no-workstream", "BFD4F2"),
            ("mobius:workstream", "5319E7"),
        ]
        .map(|(name, color)| (name.to_string(), color.to_string()))
    );
    // No label got a PATCH request, and the labels in a different case got no POST.
    assert!(github.label_patches(REPOSITORY).is_empty());
}
