use std::error::Error;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use mobius_domain::Live;
use mobius_github::NewReviewComment;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    JsonObject, ListToolsResult, MetaObject, PaginatedRequestParams, ServerCapabilities,
    ServerConfig, Tool, object,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{RoleServer, ServerHandler};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use crate::threads::Target;
use crate::{
    Engine, chat, dispatch, implementer, issues, judge, lead, plans, researcher, reviewer, tasks,
    threads, triager, trust,
};

#[derive(Clone)]
pub(crate) struct Caller {
    pub(crate) session: i64,
    pub(crate) role: &'static str,
    pub(crate) organization: String,
    // The Triager chat has the empty repository.
    pub(crate) repository: String,
    pub(crate) workstream: i64,
    // The Implementer session reads the reason of `cannot_do` from the receiver.
    pub(crate) cannot_do: Option<UnboundedSender<String>>,
    // The Implementer session of a fix round reads the held `reply_thread` calls from the receiver.
    pub(crate) fix: Option<Fix>,
    // The pull request and the head commit that the Reviewer session reviews.
    pub(crate) review: Option<Review>,
    // The items of the Judge session and the sender of each valid `submit_verdicts` call.
    pub(crate) judge: Option<Judge>,
    // The turn of the Lead session. `hold_event` holds the event of this turn.
    pub(crate) turn: Option<Arc<Mutex<chat::Current>>>,
}

#[derive(Clone)]
pub(crate) struct Judge {
    // The id of each item, and `true` for an item of a trusted bot.
    pub(crate) items: Vec<(i64, bool)>,
    pub(crate) verdicts: UnboundedSender<Vec<judge::ItemVerdicts>>,
}

#[derive(Clone)]
pub(crate) struct Review {
    pub(crate) pull_request: i64,
    pub(crate) head: String,
}

#[derive(Clone)]
pub(crate) struct Fix {
    pub(crate) pull_request: i64,
    pub(crate) replies: UnboundedSender<Reply>,
}

pub(crate) struct Reply {
    pub(crate) target: Target,
    pub(crate) text: String,
}

// The key is valid until `close`.
pub(crate) fn open(engine: &Engine, caller: Caller) -> Result<String, getrandom::Error> {
    let key = crate::random_hex()?;
    engine.callers.lock().unwrap().insert(key.clone(), caller);
    Ok(key)
}

pub(crate) fn url(engine: &Engine, key: &str) -> String {
    format!("http://127.0.0.1:{}/mcp/{key}", engine.port)
}

pub(crate) fn close(engine: &Engine, key: &str) {
    engine.callers.lock().unwrap().remove(key);
}

pub fn router(engine: Engine) -> Router {
    Router::new()
        .route("/mcp/{key}", any(serve))
        .with_state(engine)
}

async fn serve(
    State(engine): State<Engine>,
    Path(key): Path<String>,
    request: Request,
) -> Response {
    let Some(caller) = engine.callers.lock().unwrap().get(&key).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let handler = Handler { engine, caller };
    StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    )
    .handle(request)
    .await
    .into_response()
}

fn tools(role: &str) -> Vec<Tool> {
    match role {
        chat::ROLE => vec![
            tool(
                "list_tasks",
                "Give the task list of the Workstream: one line for each open issue.",
                object(json!({})),
            ),
            tool(
                "read_issue",
                "Give an issue or a pull request of the repository, with its comments, reviews, and review threads. The text is only from trusted authors.",
                object(json!({
                    "n": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The number of the issue or the pull request."
                    }
                })),
            ),
            tool(
                "start_implementer",
                "Start an Implementer for a dispatched task. The Implementer sees only the Brief, the issue, and your instructions. Mobius pushes its commits and opens a draft pull request. Returns at once.",
                object(json!({
                    "n": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The number of the task issue."
                    },
                    "instructions": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The goal, the limits, and what \"done\" means."
                    }
                })),
            ),
            tool(
                "start_fix_round",
                "Start a fix round on the pull request of a task that is ready_for_review. The Implementer gets your findings as the open items. The round counts toward max_fix_rounds. Returns at once.",
                object(json!({
                    "n": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The number of the task issue."
                    },
                    "findings": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Your findings on the pull request: what to change and why."
                    }
                })),
            ),
            tool(
                "start_researcher",
                "Start a Researcher that answers a question about the code of the default branch. The Researcher sees only the Brief and the question. The tool returns at once, and the report arrives later.",
                object(json!({
                    "question": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The question, with the context that the Researcher needs."
                    }
                })),
            ),
            tool(
                "ask",
                "Ask the people on a task issue a question. Mobius posts the question as a comment, adds mobius:needs-human, and adds an Inbox item for the Owner. The reply arrives later as an event.",
                object(json!({
                    "n": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The number of the task issue."
                    },
                    "text": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The question for the people on the issue."
                    }
                })),
            ),
            tool(
                "decline",
                "Decline a task. Mobius posts the reason as a comment on the issue, removes mobius:working, and ends the task.",
                object(json!({
                    "n": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The number of the task issue."
                    },
                    "reason": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The reason for the people on the issue."
                    }
                })),
            ),
            tool(
                "create_issue",
                "Create an issue below an issue of the Workstream. Mobius adds the blockers as native issue dependencies.",
                object(json!({
                    "title": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The title of the issue."
                    },
                    "body": {
                        "type": "string",
                        "description": "The body of the issue: the goal, the limits, and what \"done\" means."
                    },
                    "parent": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The Workstream issue or an issue below it."
                    },
                    "blocked_by": {
                        "type": "array",
                        "items": { "type": "integer", "minimum": 1 },
                        "description": "The issues that block this issue. They can be in another Workstream."
                    }
                })),
            ),
            tool(
                "mark_ready",
                "Add mobius:ready to an issue of the Workstream. Use it when the Owner tells you to start an issue.",
                object(json!({
                    "n": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The number of the issue."
                    }
                })),
            ),
            tool(
                "reply_thread",
                "Reply in a review thread or to a conversation comment of the pull request of a task, for example with the link to a follow-up issue.",
                object(json!({
                    "thread": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The number of the thread or the comment in the event."
                    },
                    "text": {
                        "type": "string",
                        "minLength": 1,
                        "description": "A follow-up link, an answer, or a reason to reject. Do not write an acknowledgement."
                    }
                })),
            ),
            tool(
                "comment_pull_request",
                "Post a comment on the pull request of a task, for example to propose that a human closes a stale pull request.",
                object(json!({
                    "n": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The number of the pull request."
                    },
                    "text": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The comment."
                    }
                })),
            ),
            tool(
                "create_workstream",
                "Create a Workstream in this repository: an issue with mobius:workstream. Call it only after the Owner approves the exact title and Brief in the chat.",
                object(json!({
                    "title": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The name of the Workstream."
                    },
                    "brief": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The Brief: the goal, the scope, and the limits of the Workstream."
                    }
                })),
            ),
            tool(
                "move_task",
                "Make a task of this Workstream a sub-issue of a different open Workstream in this repository. Call it only after the Owner approves the move in the chat. The task must not be in progress.",
                object(json!({
                    "n": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The number of the task issue."
                    },
                    "workstream": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The number of the target Workstream issue."
                    }
                })),
            ),
            tool(
                "hold_event",
                "Hold the event of this turn until the Owner decides. Mobius sends the event again after the end of your next reply to the Owner. A later event of the same task issue waits behind it. Call it only in a turn for an event.",
                object(json!({})),
            ),
            tool(
                "tell_owner",
                "Tell the Owner something. Mobius adds the text to the Lead chat and adds an Inbox item.",
                object(json!({
                    "text": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The text for the Owner."
                    }
                })),
            ),
        ],
        implementer::ROLE => vec![
            tool(
                "cannot_do",
                "Tell the Lead that you cannot do the task. Mobius ends your turn and pushes nothing.",
                object(json!({
                    "reason": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The reason for the Lead."
                    }
                })),
            ),
            tool(
                "reply_thread",
                "Reply in a review thread of the pull request in a fix round. Mobius posts the reply after it pushes your commits, so the SHA of a fix commit in the text links to a pushed commit. Mobius resolves the thread after the reply.",
                object(json!({
                    "thread": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The number of the thread or the comment in the prompt. Mobius cannot resolve a conversation comment."
                    },
                    "text": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The SHA of the fix commit, an answer, a follow-up link, or a reason to reject. Do not write an acknowledgement."
                    }
                })),
            ),
        ],
        reviewer::ROLE => vec![tool(
            "submit_review",
            "Post your review on the pull request as one GitHub review with inline comments. Call it one time. With no findings, do not call it.",
            object(json!({
                "body": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The summary of the review."
                },
                "comments": {
                    "type": "array",
                    "description": "One inline comment for each finding.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "minLength": 1,
                                "description": "The file path, relative to the repository root."
                            },
                            "line": {
                                "type": "integer",
                                "minimum": 1,
                                "description": "The line in the new version of the file. It must be in the diff."
                            },
                            "body": {
                                "type": "string",
                                "minLength": 1,
                                "description": "The finding."
                            }
                        },
                        "required": ["path", "line", "body"],
                        "additionalProperties": false
                    }
                }
            })),
        )],
        triager::ROLE => vec![
            tool(
                "create_workstream",
                "Create a Workstream: an issue with mobius:workstream. In the chat, call it only after the Owner approves the exact title and Brief.",
                object(json!({
                    "title": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The name of the Workstream."
                    },
                    "brief": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The Brief: the goal, the scope, and the limits of the Workstream."
                    }
                })),
            ),
            tool(
                "move_issue",
                "Make an issue a sub-issue of an open Workstream. For an issue with mobius:no-workstream, Mobius then adds mobius:ready again.",
                object(json!({
                    "n": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The number of the issue."
                    },
                    "workstream": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The number of the Workstream issue."
                    }
                })),
            ),
        ],
        judge::ROLE => vec![tool(
            "submit_verdicts",
            "Give the actions for each item of the batch, one entry for each item. Items of trusted users take fix, question, and follow-up. Items of trusted bots take fix and reject. A later valid call replaces an earlier one.",
            object(json!({
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "item": {
                                "type": "integer",
                                "description": "The number of the thread or the comment in the prompt."
                            },
                            "actions": {
                                "type": "array",
                                "minItems": 1,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "verdict": {
                                            "type": "string",
                                            "enum": ["fix", "question", "follow-up", "reject"]
                                        },
                                        "text": {
                                            "type": "string",
                                            "minLength": 1,
                                            "description": "For fix and question, the work for the Implementer. For follow-up, the goal of the new issue. For reject, the reason for the author."
                                        }
                                    },
                                    "required": ["verdict", "text"],
                                    "additionalProperties": false
                                }
                            }
                        },
                        "required": ["item", "actions"],
                        "additionalProperties": false
                    }
                }
            })),
        )],
        _ => Vec::new(),
    }
}

fn tool(name: &'static str, description: &'static str, properties: JsonObject) -> Tool {
    let required: Vec<&String> = properties.keys().collect();
    let schema = object(json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    }));
    Tool::new(name, description, schema).with_meta(MetaObject(object(json!({
        "anthropic/alwaysLoad": true
    }))))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListTasks {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HoldEvent {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadIssue {
    n: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartImplementer {
    n: i64,
    instructions: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartFixRound {
    n: i64,
    findings: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateWorkstream {
    title: String,
    brief: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MoveIssue {
    n: i64,
    workstream: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartResearcher {
    question: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CannotDo {
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyThread {
    thread: i64,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitVerdicts {
    items: Vec<judge::ItemVerdicts>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LeadReplyThread {
    thread: i64,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitReview {
    body: String,
    comments: Vec<NewReviewComment>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ask {
    n: i64,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decline {
    n: i64,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkReady {
    n: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommentPullRequest {
    n: i64,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TellOwner {
    text: String,
}

fn parse<T: DeserializeOwned>(tool: &str, arguments: &Value) -> Result<T, String> {
    T::deserialize(arguments).map_err(|error| format!("Invalid arguments for {tool}: {error}."))
}

#[derive(Clone)]
struct Handler {
    engine: Engine,
    caller: Caller,
}

impl Handler {
    async fn run(
        &self,
        tool: &str,
        arguments: &Value,
    ) -> Result<String, Box<dyn Error + Send + Sync>> {
        let unknown = || format!("Unknown tool: {tool}.");
        if !tools(self.caller.role)
            .iter()
            .any(|known| known.name == tool)
        {
            return Err(unknown().into());
        }
        let repository = triager::repository(
            &self.engine,
            &self.caller.organization,
            &self.caller.repository,
        )?;
        match tool {
            "list_tasks" => {
                let ListTasks {} = parse(tool, arguments)?;
                let lines = tasks::list(
                    &self.engine,
                    &self.caller.repository,
                    self.caller.workstream,
                )
                .await?;
                Ok(tasks::text(&lines))
            }
            "read_issue" => {
                let ReadIssue { n } = parse(tool, arguments)?;
                if n < 1 {
                    return Err("n must be 1 or more.".into());
                }
                let trusted = trust::trusted_authors(&self.engine, &repository);
                issues::read_issue(&repository, n, &trusted).await
            }
            "start_implementer" => {
                let StartImplementer { n, instructions } = parse(tool, arguments)?;
                if n < 1 {
                    return Err("n must be 1 or more.".into());
                }
                if instructions.trim().is_empty() {
                    return Err("instructions must not be empty.".into());
                }
                implementer::start(
                    &self.engine,
                    &repository,
                    self.caller.workstream,
                    n,
                    &instructions,
                    Some(self.caller.session),
                )
                .await
            }
            "start_fix_round" => {
                let StartFixRound { n, findings } = parse(tool, arguments)?;
                if n < 1 {
                    return Err("n must be 1 or more.".into());
                }
                if findings.trim().is_empty() {
                    return Err("findings must not be empty.".into());
                }
                dispatch::fix_round(
                    &self.engine,
                    &repository,
                    self.caller.workstream,
                    n,
                    &findings,
                    self.caller.session,
                )
                .await
            }
            "create_workstream" => {
                let CreateWorkstream { title, brief } = parse(tool, arguments)?;
                if self.caller.role != chat::ROLE && !self.caller.repository.is_empty() {
                    return Err(
                        "Only the Triager chat or the Lead chat creates a Workstream, after the Owner approves it."
                            .into(),
                    );
                }
                if title.trim().is_empty() || brief.trim().is_empty() {
                    return Err("title and brief must not be empty.".into());
                }
                let number = triager::create_workstream(&repository, &title, &brief).await?;
                self.engine.broadcast(if self.caller.role == chat::ROLE {
                    Live::Workstreams
                } else {
                    Live::WorkstreamCreated {
                        repository: repository.full_name.clone(),
                        number,
                    }
                });
                Ok(format!("Created the Workstream #{number}."))
            }
            "move_task" => {
                let MoveIssue { n, workstream } = parse(tool, arguments)?;
                if n < 1 || workstream < 1 {
                    return Err("n and workstream must be 1 or more.".into());
                }
                let result = lead::move_task(
                    &self.engine,
                    &repository,
                    self.caller.workstream,
                    n,
                    workstream,
                )
                .await?;
                self.engine.broadcast(Live::Workstreams);
                Ok(result)
            }
            "move_issue" => {
                let MoveIssue { n, workstream } = parse(tool, arguments)?;
                if n < 1 || workstream < 1 {
                    return Err("n and workstream must be 1 or more.".into());
                }
                triager::move_issue(&repository, n, workstream).await
            }
            "start_researcher" => {
                let StartResearcher { question } = parse(tool, arguments)?;
                if question.trim().is_empty() {
                    return Err("question must not be empty.".into());
                }
                if self.engine.drain.on() {
                    return Err("Mobius prepares an upgrade, so no Researcher starts now.".into());
                }
                // The subscription comes before the spawn, so the Researcher gets each stop of its Lead.
                tokio::spawn(researcher::run(
                    self.engine.clone(),
                    self.engine.lead_stops.subscribe(),
                    researcher::Job {
                        repository: self.caller.repository.clone(),
                        workstream: self.caller.workstream,
                        question,
                        parent: self.caller.session,
                    },
                ));
                Ok("Started a Researcher. The report arrives later.".to_string())
            }
            "cannot_do" => {
                let CannotDo { reason } = parse(tool, arguments)?;
                if reason.trim().is_empty() {
                    return Err("reason must not be empty.".into());
                }
                self.caller
                    .cannot_do
                    .as_ref()
                    .ok_or_else(unknown)?
                    .send(reason)?;
                Ok("Mobius ends this turn.".to_string())
            }
            "reply_thread" if self.caller.role != implementer::ROLE => {
                let LeadReplyThread { thread, text } = parse(tool, arguments)?;
                if text.trim().is_empty() {
                    return Err("text must not be empty.".into());
                }
                for task in self
                    .engine
                    .store
                    .tasks()
                    .live_in(&self.caller.repository)
                    .await?
                    .into_iter()
                    .filter(|task| task.workstream == self.caller.workstream)
                {
                    let Some(pull_request) = task.pull_request else {
                        continue;
                    };
                    if let Some(target) = threads::target(&repository, pull_request, thread).await?
                    {
                        threads::reply(&repository, pull_request, &target, &text).await?;
                        return Ok(format!("Replied to {thread}."));
                    }
                }
                Err(format!(
                    "{thread} is not a review thread or a comment of a pull request of a live task in this Workstream."
                )
                .into())
            }
            "reply_thread" => {
                let ReplyThread { thread, text } = parse(tool, arguments)?;
                if text.trim().is_empty() {
                    return Err("text must not be empty.".into());
                }
                let fix = self
                    .caller
                    .fix
                    .as_ref()
                    .ok_or("Only an Implementer of a fix round can reply in a thread.")?;
                let target = threads::target(&repository, fix.pull_request, thread)
                    .await?
                    .ok_or_else(|| {
                        format!(
                            "{thread} is not a review thread or a comment of pull request #{}.",
                            fix.pull_request
                        )
                    })?;
                fix.replies.send(Reply { target, text })?;
                Ok("Mobius posts the reply after it pushes your commits.".to_string())
            }
            "submit_verdicts" => {
                let SubmitVerdicts { items } = parse(tool, arguments)?;
                let judge = self.caller.judge.as_ref().ok_or_else(unknown)?;
                judge::validate(&judge.items, &items)?;
                judge.verdicts.send(items)?;
                Ok("Mobius routes the actions when your turn ends.".to_string())
            }
            "submit_review" => {
                let SubmitReview { body, comments } = parse(tool, arguments)?;
                if body.trim().is_empty() {
                    return Err("body must not be empty.".into());
                }
                if comments.iter().any(|comment| {
                    comment.path.trim().is_empty()
                        || comment.line < 1
                        || comment.body.trim().is_empty()
                }) {
                    return Err(
                        "Each comment needs a path, a line of 1 or more, and a body.".into(),
                    );
                }
                let review = self.caller.review.as_ref().ok_or_else(unknown)?;
                repository
                    .submit_review(review.pull_request, &review.head, &body, &comments)
                    .await?;
                Ok("Posted the review.".to_string())
            }
            "ask" => {
                let Ask { n, text } = parse(tool, arguments)?;
                if n < 1 {
                    return Err("n must be 1 or more.".into());
                }
                if text.trim().is_empty() {
                    return Err("text must not be empty.".into());
                }
                dispatch::ask(&self.engine, &repository, self.caller.workstream, n, &text).await
            }
            "decline" => {
                let Decline { n, reason } = parse(tool, arguments)?;
                if n < 1 {
                    return Err("n must be 1 or more.".into());
                }
                if reason.trim().is_empty() {
                    return Err("reason must not be empty.".into());
                }
                dispatch::decline(
                    &self.engine,
                    &repository.app_slug,
                    &repository,
                    self.caller.workstream,
                    n,
                    &reason,
                )
                .await
            }
            "create_issue" => {
                let new: plans::NewIssue = parse(tool, arguments)?;
                if new.title.trim().is_empty() {
                    return Err("title must not be empty.".into());
                }
                if new.parent < 1 || new.blocked_by.iter().any(|number| *number < 1) {
                    return Err("parent and each blocked_by must be 1 or more.".into());
                }
                plans::create_issue(&repository, self.caller.workstream, &new).await
            }
            "mark_ready" => {
                let MarkReady { n } = parse(tool, arguments)?;
                if n < 1 {
                    return Err("n must be 1 or more.".into());
                }
                plans::mark_ready(&self.engine, &repository, self.caller.workstream, n).await
            }
            "comment_pull_request" => {
                let CommentPullRequest { n, text } = parse(tool, arguments)?;
                if n < 1 {
                    return Err("n must be 1 or more.".into());
                }
                if text.trim().is_empty() {
                    return Err("text must not be empty.".into());
                }
                self.engine
                    .store
                    .tasks()
                    .live_by_pull_request(&self.caller.repository, n)
                    .await?
                    .filter(|task| task.workstream == self.caller.workstream)
                    .ok_or_else(|| {
                        format!("#{n} is not the pull request of a live task in this Workstream.")
                    })?;
                repository.add_comment(n, &text).await?;
                Ok(format!("Commented on #{n}."))
            }
            "hold_event" => {
                let HoldEvent {} = parse(tool, arguments)?;
                chat::hold_event(self.caller.turn.as_ref().ok_or_else(unknown)?)
            }
            "tell_owner" => {
                let TellOwner { text } = parse(tool, arguments)?;
                if text.trim().is_empty() {
                    return Err("text must not be empty.".into());
                }
                chat::tell_owner(&self.engine, &repository, self.caller.workstream, &text).await
            }
            _ => Err(unknown().into()),
        }
    }
}

impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        // Protocol 2026-07-28 requires `ttlMs` and `cacheScope`. Claude Code refuses a tool list without them.
        Ok(ListToolsResult::with_all_items(tools(self.caller.role))
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Private))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let outcome = self
            .run(&request.name, &arguments)
            .await
            .map_err(|error| error.to_string());
        let row = match &outcome {
            Ok(text) => json!({ "tool": request.name, "arguments": arguments, "result": text }),
            Err(error) => json!({ "tool": request.name, "arguments": arguments, "error": error }),
        };
        self.engine
            .store
            .transcript()
            .add(self.caller.session, "mcp_call", &row.to_string())
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        Ok(match outcome {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(error) => CallToolResult::error(vec![ContentBlock::text(error)]),
        }
        .into())
    }
}
