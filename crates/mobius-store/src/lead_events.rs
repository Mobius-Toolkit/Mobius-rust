use std::error::Error;

use mobius_domain::{Author, ChatMessage};
use sqlx::sqlite::SqlitePool;
use time::OffsetDateTime;

use crate::chat_messages;

pub struct LeadEvents<'a> {
    pub(crate) pool: &'a SqlitePool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LeadEvent {
    pub id: i64,
    pub payload: String,
    pub issue: Option<i64>,
    // The chat entry of the event. An event of an older store has none.
    pub chat_message: Option<i64>,
}

impl LeadEvents<'_> {
    // The chat entry and the event go in together, so a reader sees both or none.
    pub async fn add(
        &self,
        organization: &str,
        repository: &str,
        workstream: i64,
        issue: Option<i64>,
        kind: &str,
        payload: &str,
    ) -> Result<ChatMessage, Box<dyn Error + Send + Sync>> {
        let time = OffsetDateTime::now_utc();
        let mut transaction = self.pool.begin().await?;
        let message = chat_messages::insert(
            &mut *transaction,
            organization,
            repository,
            workstream,
            Author::Event,
            payload,
        )
        .await?;
        sqlx::query!(
            "INSERT INTO lead_events (repository, workstream, issue, kind, payload, time, chat_message) VALUES (?, ?, ?, ?, ?, ?, ?)",
            repository,
            workstream,
            issue,
            kind,
            payload,
            time,
            message.id
        )
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(message)
    }

    pub async fn waiting_workstreams(
        &self,
        repository: &str,
    ) -> Result<Vec<i64>, Box<dyn Error + Send + Sync>> {
        let workstreams = sqlx::query_scalar!(
            "SELECT DISTINCT workstream FROM lead_events
             WHERE repository = ? AND delivered_at IS NULL",
            repository
        )
        .fetch_all(self.pool)
        .await?;
        Ok(workstreams)
    }

    // Each Workstream with an undelivered event, with its repository.
    pub async fn waiting(&self) -> Result<Vec<(String, i64)>, Box<dyn Error + Send + Sync>> {
        let rows = sqlx::query!(
            "SELECT DISTINCT repository, workstream FROM lead_events
             WHERE delivered_at IS NULL"
        )
        .fetch_all(self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| (row.repository, row.workstream))
            .collect())
    }

    pub async fn undelivered(
        &self,
        repository: &str,
        workstream: i64,
    ) -> Result<Vec<LeadEvent>, Box<dyn Error + Send + Sync>> {
        let events = sqlx::query_as!(
            LeadEvent,
            r#"SELECT id, payload, issue, chat_message FROM lead_events
               WHERE repository = ? AND workstream = ? AND delivered_at IS NULL
               ORDER BY id"#,
            repository,
            workstream
        )
        .fetch_all(self.pool)
        .await?;
        Ok(events)
    }

    // The undelivered events that are not held, in the order that they occurred. A held event holds each later event of its task issue.
    pub async fn ready(
        &self,
        repository: &str,
        workstream: i64,
    ) -> Result<Vec<LeadEvent>, Box<dyn Error + Send + Sync>> {
        let events = sqlx::query_as!(
            LeadEvent,
            r#"SELECT id, payload, issue, chat_message FROM lead_events AS event
               WHERE repository = ? AND workstream = ? AND delivered_at IS NULL AND held = 0
               AND NOT EXISTS (
                   SELECT 1 FROM lead_events AS earlier
                   WHERE earlier.repository = event.repository
                   AND earlier.workstream = event.workstream
                   AND earlier.issue = event.issue
                   AND earlier.id < event.id
                   AND earlier.delivered_at IS NULL AND earlier.held = 1
               )
               ORDER BY id"#,
            repository,
            workstream
        )
        .fetch_all(self.pool)
        .await?;
        Ok(events)
    }

    pub async fn hold(&self, id: i64) -> Result<(), Box<dyn Error + Send + Sync>> {
        sqlx::query!("UPDATE lead_events SET held = 1 WHERE id = ?", id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    // Returns the events that were held.
    pub async fn free(
        &self,
        repository: &str,
        workstream: i64,
    ) -> Result<Vec<LeadEvent>, Box<dyn Error + Send + Sync>> {
        let freed = sqlx::query_as!(
            LeadEvent,
            r#"UPDATE lead_events SET held = 0
               WHERE repository = ? AND workstream = ? AND held = 1
               RETURNING id, payload, issue, chat_message"#,
            repository,
            workstream
        )
        .fetch_all(self.pool)
        .await?;
        Ok(freed)
    }

    pub async fn deliver_all(
        &self,
        repository: &str,
        workstream: i64,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let delivered_at = OffsetDateTime::now_utc();
        sqlx::query!(
            "UPDATE lead_events SET delivered_at = ?
             WHERE repository = ? AND workstream = ? AND delivered_at IS NULL",
            delivered_at,
            repository,
            workstream
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn deliver(&self, id: i64) -> Result<(), Box<dyn Error + Send + Sync>> {
        let delivered_at = OffsetDateTime::now_utc();
        sqlx::query!(
            "UPDATE lead_events SET delivered_at = ? WHERE id = ?",
            delivered_at,
            id
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }
}
