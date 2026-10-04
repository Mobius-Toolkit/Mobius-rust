use std::error::Error;

use mobius_domain::{Author, ChatMessage, Unread};
use sqlx::Executor;
use sqlx::sqlite::{Sqlite, SqlitePool};
use time::OffsetDateTime;

pub struct ChatMessages<'a> {
    pub(crate) pool: &'a SqlitePool,
}

struct Row {
    id: i64,
    organization: String,
    repository: String,
    workstream: i64,
    author: String,
    time: OffsetDateTime,
    text: String,
}

impl Row {
    fn message(self) -> Result<ChatMessage, Box<dyn Error + Send + Sync>> {
        let author = Author::ALL
            .into_iter()
            .find(|author| author.name() == self.author)
            .ok_or_else(|| format!("unknown chat author `{}`", self.author))?;
        Ok(ChatMessage {
            id: self.id,
            organization: self.organization,
            repository: self.repository,
            workstream: self.workstream,
            author,
            time: self.time,
            text: self.text,
        })
    }
}

pub(crate) async fn insert<'a>(
    executor: impl Executor<'a, Database = Sqlite>,
    organization: &str,
    repository: &str,
    workstream: i64,
    author: Author,
    text: &str,
) -> Result<ChatMessage, Box<dyn Error + Send + Sync>> {
    let author = author.name();
    let time = OffsetDateTime::now_utc();
    let row = sqlx::query_as!(
        Row,
        r#"INSERT INTO chat_messages (organization, repository, workstream, author, time, text)
               VALUES (?, ?, ?, ?, ?, ?)
               RETURNING id, organization, repository, workstream, author, time AS "time: OffsetDateTime", text"#,
        organization,
        repository,
        workstream,
        author,
        time,
        text
    )
    .fetch_one(executor)
    .await?;
    row.message()
}

impl ChatMessages<'_> {
    pub async fn add(
        &self,
        organization: &str,
        repository: &str,
        workstream: i64,
        author: Author,
        text: &str,
    ) -> Result<ChatMessage, Box<dyn Error + Send + Sync>> {
        insert(
            self.pool,
            organization,
            repository,
            workstream,
            author,
            text,
        )
        .await
    }

    pub async fn append(
        &self,
        id: i64,
        text: &str,
    ) -> Result<ChatMessage, Box<dyn Error + Send + Sync>> {
        let row = sqlx::query_as!(
            Row,
            r#"UPDATE chat_messages SET text = text || ? WHERE id = ?
               RETURNING id, organization, repository, workstream, author, time AS "time: OffsetDateTime", text"#,
            text,
            id
        )
        .fetch_one(self.pool)
        .await?;
        row.message()
    }

    pub async fn list(
        &self,
        organization: &str,
        repository: &str,
        workstream: i64,
    ) -> Result<Vec<ChatMessage>, Box<dyn Error + Send + Sync>> {
        let rows = sqlx::query_as!(
            Row,
            r#"SELECT id, organization, repository, workstream, author, time AS "time: OffsetDateTime", text
               FROM chat_messages WHERE organization = ? AND repository = ? AND workstream = ?
                 AND author <> 'Researcher'
               ORDER BY id"#,
            organization,
            repository,
            workstream
        )
        .fetch_all(self.pool)
        .await?;
        rows.into_iter().map(Row::message).collect()
    }

    pub async fn before(
        &self,
        organization: &str,
        repository: &str,
        workstream: i64,
        id: i64,
        limit: i64,
    ) -> Result<Vec<ChatMessage>, Box<dyn Error + Send + Sync>> {
        let rows = sqlx::query_as!(
            Row,
            r#"SELECT id, organization, repository, workstream, author, time AS "time: OffsetDateTime", text
               FROM chat_messages WHERE organization = ? AND repository = ? AND workstream = ? AND id < ?
               ORDER BY id DESC LIMIT ?"#,
            organization,
            repository,
            workstream,
            id,
            limit
        )
        .fetch_all(self.pool)
        .await?;
        rows.into_iter().rev().map(Row::message).collect()
    }

    pub async fn set_seen(
        &self,
        organization: &str,
        repository: &str,
        workstream: i64,
        message: i64,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        sqlx::query!(
            "INSERT INTO chat_seen (organization, repository, workstream, message) VALUES (?, ?, ?, ?)
             ON CONFLICT (organization, repository, workstream)
             DO UPDATE SET message = max(message, excluded.message)",
            organization,
            repository,
            workstream,
            message
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn unread(&self) -> Result<Vec<Unread>, Box<dyn Error + Send + Sync>> {
        let unread = sqlx::query_as!(
            Unread,
            r#"SELECT chat_messages.organization, chat_messages.repository, chat_messages.workstream,
                      count(*) AS "count!: i64"
               FROM chat_messages
               LEFT JOIN chat_seen ON chat_seen.organization = chat_messages.organization
                                  AND chat_seen.repository = chat_messages.repository
                                  AND chat_seen.workstream = chat_messages.workstream
               WHERE chat_messages.author NOT IN ('Owner', 'Researcher', 'Event')
                 AND chat_messages.id > coalesce(chat_seen.message, 0)
               GROUP BY chat_messages.organization, chat_messages.repository, chat_messages.workstream"#
        )
        .fetch_all(self.pool)
        .await?;
        Ok(unread)
    }

    pub async fn unread_of(
        &self,
        organization: &str,
        repository: &str,
        workstream: i64,
    ) -> Result<Unread, Box<dyn Error + Send + Sync>> {
        let count = sqlx::query_scalar!(
            r#"SELECT count(*) AS "count!: i64" FROM chat_messages
               WHERE organization = ? AND repository = ? AND workstream = ? AND author NOT IN ('Owner', 'Researcher', 'Event')
                 AND id > coalesce((SELECT message FROM chat_seen
                                    WHERE organization = ? AND repository = ? AND workstream = ?), 0)"#,
            organization,
            repository,
            workstream,
            organization,
            repository,
            workstream
        )
        .fetch_one(self.pool)
        .await?;
        Ok(Unread {
            organization: organization.to_string(),
            repository: repository.to_string(),
            workstream,
            count,
        })
    }
}
