use std::error::Error;

use mobius_domain::{Harness, Session};
use sqlx::sqlite::SqlitePool;
use time::OffsetDateTime;

pub struct Sessions<'a> {
    pub(crate) pool: &'a SqlitePool,
}

struct Row {
    id: i64,
    role: String,
    harness: String,
    model: String,
    organization: String,
    repository: String,
    workstream: i64,
    acp_session_id: Option<String>,
    started_at: OffsetDateTime,
    ended_at: Option<OffsetDateTime>,
    end_reason: Option<String>,
    queue_reason: Option<String>,
    issue: Option<i64>,
    parent: Option<i64>,
}

impl Row {
    fn session(self) -> Result<Session, Box<dyn Error + Send + Sync>> {
        let harness = Harness::ALL
            .into_iter()
            .find(|harness| harness.name() == self.harness)
            .ok_or_else(|| format!("unknown harness `{}`", self.harness))?;
        Ok(Session {
            id: self.id,
            role: self.role,
            harness,
            model: self.model,
            organization: self.organization,
            repository: self.repository,
            workstream: self.workstream,
            acp_session_id: self.acp_session_id,
            started_at: self.started_at,
            ended_at: self.ended_at,
            end_reason: self.end_reason,
            queue_reason: self.queue_reason,
            issue: self.issue,
            parent: self.parent,
        })
    }
}

struct OpenRow {
    id: i64,
    role: String,
    harness: String,
    model: String,
    organization: String,
    repository: String,
    workstream: i64,
    acp_session_id: Option<String>,
    started_at: OffsetDateTime,
    ended_at: Option<OffsetDateTime>,
    end_reason: Option<String>,
    queue_reason: Option<String>,
    issue: Option<i64>,
    parent: Option<i64>,
    workstream_title: Option<String>,
    issue_title: Option<String>,
    pull_request: Option<i64>,
}

impl OpenRow {
    fn open_session(self) -> Result<OpenSession, Box<dyn Error + Send + Sync>> {
        let session = Row {
            id: self.id,
            role: self.role,
            harness: self.harness,
            model: self.model,
            organization: self.organization,
            repository: self.repository,
            workstream: self.workstream,
            acp_session_id: self.acp_session_id,
            started_at: self.started_at,
            ended_at: self.ended_at,
            end_reason: self.end_reason,
            queue_reason: self.queue_reason,
            issue: self.issue,
            parent: self.parent,
        }
        .session()?;
        Ok(OpenSession {
            session,
            workstream_title: self.workstream_title,
            issue_title: self.issue_title,
            pull_request: self.pull_request,
        })
    }
}

// An open session with the data that the store holds about its Workstream, issue, and task.
pub struct OpenSession {
    pub session: Session,
    pub workstream_title: Option<String>,
    pub issue_title: Option<String>,
    pub pull_request: Option<i64>,
}

pub struct NewSession<'a> {
    pub role: &'a str,
    pub harness: Harness,
    pub model: &'a str,
    pub organization: &'a str,
    pub repository: &'a str,
    pub workstream: i64,
    pub issue: Option<i64>,
    pub parent: Option<i64>,
}

impl Sessions<'_> {
    pub async fn add(
        &self,
        session: NewSession<'_>,
    ) -> Result<Session, Box<dyn Error + Send + Sync>> {
        let harness = session.harness.name();
        let started_at = OffsetDateTime::now_utc();
        sqlx::query_as!(
            Row,
            r#"INSERT INTO sessions (role, harness, model, organization, repository, workstream, issue, parent, started_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
               RETURNING id AS "id!", role, harness, model, organization, repository, workstream, acp_session_id,
                         started_at AS "started_at: OffsetDateTime",
                         ended_at AS "ended_at: OffsetDateTime", end_reason, queue_reason, issue, parent"#,
            session.role,
            harness,
            session.model,
            session.organization,
            session.repository,
            session.workstream,
            session.issue,
            session.parent,
            started_at
        )
        .fetch_one(self.pool)
        .await?
        .session()
    }

    pub async fn set_acp_session_id(
        &self,
        id: i64,
        acp_session_id: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        sqlx::query!(
            "UPDATE sessions SET acp_session_id = ? WHERE id = ?",
            acp_session_id,
            id
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_queue_reason(
        &self,
        id: i64,
        reason: &str,
    ) -> Result<Session, Box<dyn Error + Send + Sync>> {
        sqlx::query_as!(
            Row,
            r#"UPDATE sessions SET queue_reason = ? WHERE id = ?
               RETURNING id AS "id!", role, harness, model, organization, repository, workstream, acp_session_id,
                         started_at AS "started_at: OffsetDateTime",
                         ended_at AS "ended_at: OffsetDateTime", end_reason, queue_reason, issue, parent"#,
            reason,
            id
        )
        .fetch_one(self.pool)
        .await?
        .session()
    }

    pub async fn clear_queue_reason(
        &self,
        id: i64,
    ) -> Result<Session, Box<dyn Error + Send + Sync>> {
        sqlx::query_as!(
            Row,
            r#"UPDATE sessions SET queue_reason = NULL WHERE id = ?
               RETURNING id AS "id!", role, harness, model, organization, repository, workstream, acp_session_id,
                         started_at AS "started_at: OffsetDateTime",
                         ended_at AS "ended_at: OffsetDateTime", end_reason, queue_reason, issue, parent"#,
            id
        )
        .fetch_one(self.pool)
        .await?
        .session()
    }

    pub async fn start(&self, id: i64) -> Result<Session, Box<dyn Error + Send + Sync>> {
        let started_at = OffsetDateTime::now_utc();
        sqlx::query_as!(
            Row,
            r#"UPDATE sessions SET started_at = ?, queue_reason = NULL WHERE id = ?
               RETURNING id AS "id!", role, harness, model, organization, repository, workstream, acp_session_id,
                         started_at AS "started_at: OffsetDateTime",
                         ended_at AS "ended_at: OffsetDateTime", end_reason, queue_reason, issue, parent"#,
            started_at,
            id
        )
        .fetch_one(self.pool)
        .await?
        .session()
    }

    pub async fn end(
        &self,
        id: i64,
        reason: &str,
    ) -> Result<Session, Box<dyn Error + Send + Sync>> {
        let ended_at = OffsetDateTime::now_utc();
        sqlx::query_as!(
            Row,
            r#"UPDATE sessions SET ended_at = ?, end_reason = ?, queue_reason = NULL WHERE id = ?
               RETURNING id AS "id!", role, harness, model, organization, repository, workstream, acp_session_id,
                         started_at AS "started_at: OffsetDateTime",
                         ended_at AS "ended_at: OffsetDateTime", end_reason, queue_reason, issue, parent"#,
            ended_at,
            reason,
            id
        )
        .fetch_one(self.pool)
        .await?
        .session()
    }

    pub async fn open_ids(&self) -> Result<Vec<i64>, Box<dyn Error + Send + Sync>> {
        let ids = sqlx::query_scalar!("SELECT id FROM sessions WHERE ended_at IS NULL")
            .fetch_all(self.pool)
            .await?;
        Ok(ids)
    }

    // All open sessions of all organizations, for the "Agents" page.
    pub async fn open(&self) -> Result<Vec<OpenSession>, Box<dyn Error + Send + Sync>> {
        let rows = sqlx::query_as!(
            OpenRow,
            r#"SELECT s.id, s.role, s.harness, s.model, s.organization, s.repository, s.workstream, s.acp_session_id,
                      s.started_at AS "started_at: OffsetDateTime",
                      s.ended_at AS "ended_at: OffsetDateTime", s.end_reason, s.queue_reason, s.issue, s.parent,
                      w.title AS workstream_title, i.title AS issue_title,
                      (SELECT t.pull_request FROM tasks t
                       WHERE t.repository = s.repository AND t.issue = s.issue
                       ORDER BY t.id DESC LIMIT 1) AS pull_request
               FROM sessions s
               LEFT JOIN copied_workstreams w ON w.repository = s.repository AND w.number = s.workstream
               LEFT JOIN copied_issues i
                 ON i.repository = s.repository AND i.workstream = s.workstream AND i.number = s.issue
               WHERE s.ended_at IS NULL ORDER BY s.id"#,
        )
        .fetch_all(self.pool)
        .await?;
        rows.into_iter().map(OpenRow::open_session).collect()
    }

    pub async fn get(&self, id: i64) -> Result<Session, Box<dyn Error + Send + Sync>> {
        sqlx::query_as!(
            Row,
            r#"SELECT id, role, harness, model, organization, repository, workstream, acp_session_id,
                      started_at AS "started_at: OffsetDateTime",
                      ended_at AS "ended_at: OffsetDateTime", end_reason, queue_reason, issue, parent
               FROM sessions WHERE id = ?"#,
            id
        )
        .fetch_one(self.pool)
        .await?
        .session()
    }

    pub async fn with_role(
        &self,
        role: &str,
    ) -> Result<Vec<Session>, Box<dyn Error + Send + Sync>> {
        let rows = sqlx::query_as!(
            Row,
            r#"SELECT id, role, harness, model, organization, repository, workstream, acp_session_id,
                      started_at AS "started_at: OffsetDateTime",
                      ended_at AS "ended_at: OffsetDateTime", end_reason, queue_reason, issue, parent
               FROM sessions WHERE role = ? ORDER BY id"#,
            role
        )
        .fetch_all(self.pool)
        .await?;
        rows.into_iter().map(Row::session).collect()
    }

    pub async fn list(
        &self,
        organization: &str,
        repository: &str,
        workstream: i64,
    ) -> Result<Vec<Session>, Box<dyn Error + Send + Sync>> {
        let rows = sqlx::query_as!(
            Row,
            r#"SELECT id, role, harness, model, organization, repository, workstream, acp_session_id,
                      started_at AS "started_at: OffsetDateTime",
                      ended_at AS "ended_at: OffsetDateTime", end_reason, queue_reason, issue, parent
               FROM sessions WHERE organization = ? AND repository = ? AND workstream = ? ORDER BY id"#,
            organization,
            repository,
            workstream
        )
        .fetch_all(self.pool)
        .await?;
        rows.into_iter().map(Row::session).collect()
    }
}
