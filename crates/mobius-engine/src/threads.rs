use std::error::Error;

use mobius_github::Repository;

// A review thread, named by the id of its first comment, or a conversation comment of the pull request.
#[derive(Clone)]
pub(crate) enum Target {
    Thread { comment: i64, node: String },
    Comment { body: String },
}

pub(crate) async fn target(
    repository: &Repository,
    pull_request: i64,
    id: i64,
) -> Result<Option<Target>, Box<dyn Error + Send + Sync>> {
    if let Some(thread) = repository
        .review_threads(pull_request)
        .await?
        .into_iter()
        .find(|thread| thread.comment == id)
    {
        return Ok(Some(Target::Thread {
            comment: id,
            node: thread.id,
        }));
    }
    Ok(repository
        .issue_comments(pull_request)
        .await?
        .into_iter()
        .find(|comment| comment.id == id)
        .map(|comment| Target::Comment { body: comment.body }))
}

// A reply to a review thread resolves the thread. A conversation comment has no thread, so the reply is a new comment that quotes it, and Mobius cannot resolve it.
pub(crate) async fn reply(
    repository: &Repository,
    pull_request: i64,
    target: &Target,
    text: &str,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    match target {
        Target::Thread { comment, node } => {
            repository
                .reply_to_review_comment(pull_request, *comment, text)
                .await?;
            repository.resolve_review_thread(node).await?;
        }
        Target::Comment { body } => {
            let quoted: Vec<String> = body.lines().map(|line| format!("> {line}")).collect();
            repository
                .add_comment(pull_request, &format!("{}\n\n{text}", quoted.join("\n")))
                .await?;
        }
    }
    Ok(())
}
