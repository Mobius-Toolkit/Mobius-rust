use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

// The queue reason of a session that waits for the end of a pause of its Harness starts with this text.
pub const PAUSED: &str = "paused until ";

// The release tag of this binary, set by the release workflow. A local build has no release tag.
pub const RELEASE_VERSION: Option<&'static str> = option_env!("MOBIUS_VERSION");

fn release_tag(tag: &str) -> Option<(u64, u64, u64)> {
    let mut parts = tag.strip_prefix('v')?.split('.');
    let version = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(version)
}

pub fn newer_release(current: &str, latest: &str) -> bool {
    match (release_tag(current), release_tag(latest)) {
        (Some(current), Some(latest)) => latest > current,
        _ => false,
    }
}

pub fn organization(repository: &str) -> &str {
    repository.split('/').next().unwrap_or_default()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Harness {
    ClaudeCode,
    Antigravity,
    Devin,
}

impl Harness {
    pub const ALL: [Harness; 3] = [Harness::ClaudeCode, Harness::Antigravity, Harness::Devin];

    pub fn name(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude-code",
            Harness::Antigravity => "antigravity",
            Harness::Devin => "devin",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceLogin {
    pub id: i64,
    pub user_agent: String,
    pub created_at: OffsetDateTime,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Devices {
    pub this_device: i64,
    pub logins: Vec<DeviceLogin>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ManifestForm {
    pub url: String,
    pub manifest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LabelStatus {
    Present,
    // The label exists with this different color.
    WrongColor(String),
    // The label exists with this name in a different case. The engine compares label
    // names exactly, so it does not see this label on issues.
    WrongCase(String),
    Missing,
}

impl LabelStatus {
    // Mobius does not rename labels, so a label in a different case stays for a human.
    pub fn fixable(&self) -> bool {
        matches!(self, LabelStatus::Missing | LabelStatus::WrongColor(_))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelCheck {
    pub name: String,
    // The fixed color of the label.
    pub color: String,
    pub status: LabelStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryCheckup {
    pub repository: String,
    pub labels: Vec<LabelCheck>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionStatus {
    Present,
    // The App has the permission and the installation does not. The Owner accepts it on this page.
    NotAccepted(String),
    // The App does not have the permission. The Owner adds it on this page.
    Missing(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionCheck {
    pub name: String,
    // The required level.
    pub level: String,
    pub status: PermissionStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckupView {
    pub repositories: Vec<RepositoryCheckup>,
    // The error text when the check of the App permissions fails. The label status does not depend on it.
    pub permissions: Result<Vec<PermissionCheck>, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Workstream {
    pub repository: String,
    pub number: i64,
    pub title: String,
    pub body: String,
    pub autopilot: bool,
    // It is true when the Workstream has direct sub-issues and all of them are closed.
    pub all_tasks_closed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskLine {
    pub number: i64,
    pub title: String,
    pub state: String,
    pub url: String,
    // A direct sub-issue of the Workstream has depth 0, and each deeper level adds one.
    pub depth: i64,
    // It holds only the open blockers.
    pub blocked_by: Vec<Blocker>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NeedsHuman {
    pub number: i64,
    pub title: String,
    pub url: String,
    // They are set when the live task of the issue has a pull request.
    pub pull_request: Option<i64>,
    pub pull_request_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Blocker {
    pub number: i64,
    // It holds a title only when the blocker is in another Workstream.
    pub workstream_title: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FeedRow {
    pub id: i64,
    pub time: OffsetDateTime,
    pub repository: String,
    pub workstream: i64,
    pub issue: i64,
    pub actor: String,
    pub text: String,
    pub link: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Author {
    Owner,
    Lead,
    TellOwner,
    Researcher,
    Triager,
    Mobius,
    Event,
}

impl Author {
    pub const ALL: [Author; 7] = [
        Author::Owner,
        Author::Lead,
        Author::TellOwner,
        Author::Researcher,
        Author::Triager,
        Author::Mobius,
        Author::Event,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Author::Owner => "Owner",
            Author::Lead => "Lead",
            Author::TellOwner => "tell_owner",
            Author::Researcher => "Researcher",
            Author::Triager => "Triager",
            Author::Mobius => "Mobius",
            Author::Event => "Event",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: i64,
    pub organization: String,
    pub repository: String,
    pub workstream: i64,
    pub author: Author,
    pub time: OffsetDateTime,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatView {
    pub messages: Vec<ChatMessage>,
    pub writing: bool,
    pub lead: Harness,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Unread {
    pub organization: String,
    pub repository: String,
    pub workstream: i64,
    pub count: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: i64,
    pub role: String,
    pub harness: Harness,
    pub model: String,
    pub organization: String,
    pub repository: String,
    pub workstream: i64,
    pub acp_session_id: Option<String>,
    pub started_at: OffsetDateTime,
    pub ended_at: Option<OffsetDateTime>,
    pub end_reason: Option<String>,
    // The reason why the session waits for a slot.
    pub queue_reason: Option<String>,
    // The one issue the session works on. `None` for the chats and the Researcher.
    pub issue: Option<i64>,
    // The session of the agent that started this session. `None` when no agent started it.
    pub parent: Option<i64>,
    // The step of the work that the session does now. The session holds its slot in each step.
    pub phase: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentNode {
    pub session: Session,
    pub role: String,
    pub title: String,
}

// The nodes in tree order with their depth: each node follows its parent, and the newest node comes first among siblings.
// A node whose parent is not in `nodes` has depth 0.
pub fn agent_rows(nodes: Vec<AgentNode>) -> Vec<(usize, AgentNode)> {
    let by_id: HashMap<i64, &AgentNode> =
        nodes.iter().map(|node| (node.session.id, node)).collect();
    let mut paths = Vec::new();
    for node in &nodes {
        let mut path = Vec::new();
        let mut next = Some(node);
        while let Some(node) = next {
            path.push(Reverse(node.session.id));
            next = node
                .session
                .parent
                .and_then(|parent| by_id.get(&parent).copied());
        }
        path.reverse();
        paths.push(path);
    }
    let mut rows: Vec<(Vec<Reverse<i64>>, AgentNode)> = paths.into_iter().zip(nodes).collect();
    rows.sort_by(|(a, _), (b, _)| a.cmp(b));
    rows.into_iter()
        .map(|(path, node)| (path.len() - 1, node))
        .collect()
}

// The nodes to show. Without `show_stopped`, a stopped node stays only when a node below it on any level is active,
// so that `agent_rows` keeps the active nodes below their parents.
pub fn shown_agents(nodes: Vec<AgentNode>, show_stopped: bool) -> Vec<AgentNode> {
    if show_stopped {
        return nodes;
    }
    let by_id: HashMap<i64, &AgentNode> =
        nodes.iter().map(|node| (node.session.id, node)).collect();
    let mut kept = HashSet::new();
    for node in nodes.iter().filter(|node| node.session.ended_at.is_none()) {
        let mut next = Some(node);
        while let Some(node) = next {
            if !kept.insert(node.session.id) {
                break;
            }
            next = node
                .session
                .parent
                .and_then(|parent| by_id.get(&parent).copied());
        }
    }
    nodes
        .into_iter()
        .filter(|node| kept.contains(&node.session.id))
        .collect()
}

// An open session on the "Agents" page. A title is `None` when the store has no copy of the Workstream or the issue.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActiveAgent {
    pub node: AgentNode,
    pub workstream_title: Option<String>,
    pub issue_title: Option<String>,
    pub pull_request: Option<i64>,
}

// The open sessions of one role on the "Agents" page, with the role limit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentGroup {
    pub name: String,
    // The sessions that hold a slot. A queued session shows in `agents` but does not count.
    pub count: u32,
    pub max: u32,
    pub agents: Vec<ActiveAgent>,
}

// The "Agents" page: the global count and one group for each role.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActiveAgents {
    // The sessions that hold a slot and whose role counts toward `max_agents`.
    pub count: u32,
    pub max: u32,
    pub groups: Vec<AgentGroup>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TranscriptRow {
    pub id: i64,
    pub session: i64,
    pub time: OffsetDateTime,
    pub kind: String,
    pub json: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TranscriptLine {
    pub id: i64,
    pub time: OffsetDateTime,
    pub kind: String,
    pub text: String,
    pub harness_tool_name: Option<String>,
    pub body: Option<String>,
    pub folded: bool,
    pub error: bool,
    pub raw: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InboxKind {
    Question,
    Lead,
    ReadyForReview,
    StalePullRequest,
    UsageLimit,
    LeadFailed,
    Stopped,
    DiskFull,
}

impl InboxKind {
    pub const ALL: [InboxKind; 8] = [
        InboxKind::Question,
        InboxKind::Lead,
        InboxKind::ReadyForReview,
        InboxKind::StalePullRequest,
        InboxKind::UsageLimit,
        InboxKind::LeadFailed,
        InboxKind::Stopped,
        InboxKind::DiskFull,
    ];

    pub fn name(self) -> &'static str {
        match self {
            InboxKind::Question => "question",
            InboxKind::Lead => "Lead",
            InboxKind::ReadyForReview => "ready for review",
            InboxKind::StalePullRequest => "stale pull request",
            InboxKind::UsageLimit => "usage limit",
            InboxKind::LeadFailed => "Lead failed",
            InboxKind::Stopped => "stopped",
            InboxKind::DiskFull => "full disk",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InboxItem {
    pub id: i64,
    pub kind: InboxKind,
    pub organization: String,
    pub repository: String,
    pub workstream: i64,
    pub issue: i64,
    pub text: String,
    pub link: String,
    pub time: OffsetDateTime,
    pub dismissed_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Live {
    Feed(FeedRow),
    Message(ChatMessage),
    Lead {
        organization: String,
        repository: String,
        workstream: i64,
        writing: bool,
        error: Option<String>,
    },
    Unread(Unread),
    Agent(AgentNode),
    Inbox(InboxItem),
    // The Workstream list changed, for example its Autopilot.
    Workstreams,
    // The Triager chat created this Workstream.
    WorkstreamCreated {
        repository: String,
        number: i64,
    },
    // The number of agents the upgrade drain waits for. `None` means no drain.
    Drain {
        waiting: Option<usize>,
    },
    // The error of the last upgrade. `None` means the last upgrade has no error.
    UpgradeError(Option<String>),
}

// What an upgrade drain gives back when it ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DrainEnd {
    // The drain completed: no agent of Mobius runs.
    Drained,
    // The Owner cancelled the drain.
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: i64, parent: Option<i64>) -> AgentNode {
        AgentNode {
            session: Session {
                id,
                role: "implementer".to_string(),
                harness: Harness::ALL[0],
                model: String::new(),
                organization: String::new(),
                repository: String::new(),
                workstream: 12,
                acp_session_id: None,
                started_at: OffsetDateTime::UNIX_EPOCH,
                ended_at: None,
                end_reason: None,
                queue_reason: None,
                issue: None,
                parent,
                phase: None,
            },
            role: String::new(),
            title: String::new(),
        }
    }

    #[test]
    fn agent_rows_put_each_node_below_its_parent_and_a_node_with_an_unknown_parent_at_the_top() {
        let nodes = vec![
            node(1, None),
            node(2, Some(1)),
            node(3, Some(2)),
            node(4, Some(1)),
            node(5, Some(99)),
        ];

        let rows: Vec<(usize, i64)> = agent_rows(nodes)
            .into_iter()
            .map(|(depth, node)| (depth, node.session.id))
            .collect();

        assert_eq!(rows, [(0, 5), (0, 1), (1, 4), (1, 2), (2, 3)]);
    }

    fn stopped(mut node: AgentNode) -> AgentNode {
        node.session.ended_at = Some(OffsetDateTime::UNIX_EPOCH);
        node
    }

    fn shown_ids(nodes: Vec<AgentNode>, show_stopped: bool) -> Vec<i64> {
        shown_agents(nodes, show_stopped)
            .into_iter()
            .map(|node| node.session.id)
            .collect()
    }

    #[test]
    fn shown_agents_keep_a_stopped_node_that_has_an_active_node_on_any_level_below_it() {
        let nodes = vec![
            stopped(node(1, None)),
            stopped(node(2, Some(1))),
            node(3, Some(2)),
            stopped(node(4, Some(1))),
            stopped(node(5, Some(4))),
            stopped(node(6, None)),
        ];

        assert_eq!(shown_ids(nodes.clone(), false), [1, 2, 3]);
        assert_eq!(shown_ids(nodes, true), [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn a_newer_release_tag_counts_each_number() {
        assert!(newer_release("v0.1.57", "v0.2.0"));
        assert!(newer_release("v1.9.9", "v2.0.0"));
        // Numbers, not text: "10" sorts before "9" as text.
        assert!(newer_release("v0.1.9", "v0.1.10"));
    }

    #[test]
    fn an_equal_or_older_release_tag_is_not_newer() {
        assert!(!newer_release("v0.1.57", "v0.1.57"));
        assert!(!newer_release("v0.1.10", "v0.1.9"));
        assert!(!newer_release("v2.0.0", "v1.9.9"));
    }

    #[test]
    fn a_tag_that_does_not_parse_is_not_newer() {
        assert!(!newer_release("v0.1.57", "latest"));
        assert!(!newer_release("v0.1.57", "v0.1"));
        assert!(!newer_release("local", "v0.2.0"));
    }
}
