use std::collections::BTreeMap;
use std::error::Error;

use mobius_domain::{ActiveAgent, ActiveAgents, AgentGroup, AgentNode, Session, organization};

use crate::workers::Role;
use crate::{Engine, chat, triager};

pub(crate) fn node(session: Session) -> AgentNode {
    let (role, title) = match session.role.as_str() {
        chat::ROLE => ("Lead".to_string(), "chat session".to_string()),
        triager::ROLE if session.repository.is_empty() => {
            ("Triager".to_string(), "chat session".to_string())
        }
        triager::ROLE => ("Triager".to_string(), session.repository.clone()),
        role => (role.to_string(), String::new()),
    };
    AgentNode {
        session,
        role,
        title,
    }
}

// All open sessions of all organizations in one group for each role, with the limits of the config.
// A queued session (`queue_reason` is set) shows in its group but holds no slot and does not count.
pub async fn groups(engine: &Engine) -> Result<ActiveAgents, Box<dyn Error + Send + Sync>> {
    let sessions = engine.store.sessions().open().await?;
    // The slot counts match the counts of `workers::reason`: a session holds a slot while it is open and not queued.
    let mut running: BTreeMap<Role, u32> = BTreeMap::new();
    let mut agents: BTreeMap<Role, Vec<ActiveAgent>> = BTreeMap::new();
    for open in sessions {
        let session = open.session;
        let Some(role) = Role::of_session(&session.role) else {
            continue;
        };
        if session.queue_reason.is_none() {
            *running.entry(role).or_default() += 1;
        }
        agents.entry(role).or_default().push(ActiveAgent {
            node: node(session),
            workstream_title: open.workstream_title,
            issue_title: open.issue_title,
            pull_request: open.pull_request,
        });
    }
    let config = &engine.config;
    let count = running
        .iter()
        .filter(|(role, _)| role.binding(config).counts_in_max_agents)
        .map(|(_, count)| *count)
        .sum();
    let groups = Role::ALL
        .into_iter()
        .map(|role| AgentGroup {
            name: role.title().to_string(),
            count: running.get(&role).copied().unwrap_or_default(),
            max: role.binding(config).max,
            agents: agents.remove(&role).unwrap_or_default(),
        })
        .collect();
    Ok(ActiveAgents {
        count,
        max: config.max_agents,
        groups,
    })
}

pub async fn tree(
    engine: &Engine,
    repository: &str,
    workstream: i64,
) -> Result<Vec<AgentNode>, Box<dyn Error + Send + Sync>> {
    let sessions = engine
        .store
        .sessions()
        .list(organization(repository), repository, workstream)
        .await?;
    Ok(sessions.into_iter().map(node).collect())
}
