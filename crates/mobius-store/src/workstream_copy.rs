use std::error::Error;

use sqlx::Sqlite;
use sqlx::Transaction;
use sqlx::sqlite::SqlitePool;

pub struct WorkstreamCopy<'a> {
    pub(crate) pool: &'a SqlitePool,
}

pub struct CopiedWorkstream {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub autopilot: bool,
    // The issues of the tree in depth-first order.
    pub issues: Vec<CopiedIssue>,
}

pub struct CopiedIssue {
    pub number: i64,
    pub parent: i64,
    pub title: String,
    pub body: String,
    pub state: String,
    pub labels: Vec<String>,
    pub author: String,
    pub html_url: String,
    pub repository_url: String,
    pub blockers: Vec<CopiedBlocker>,
}

pub struct ChangedIssue {
    pub number: i64,
    pub repository_url: String,
    pub title: String,
    pub body: String,
    pub state: String,
    pub labels: Vec<String>,
    pub author: String,
}

pub struct CopiedBlocker {
    pub number: i64,
    pub workstream: Option<i64>,
    pub workstream_title: Option<String>,
}

impl WorkstreamCopy<'_> {
    pub async fn forget_except(
        &self,
        repositories: &[String],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let stored = sqlx::query_scalar!("SELECT DISTINCT repository FROM copied_workstreams")
            .fetch_all(self.pool)
            .await?;
        for repository in stored {
            if !repositories.contains(&repository) {
                self.replace(&repository, &[]).await?;
            }
        }
        Ok(())
    }

    pub async fn replace(
        &self,
        repository: &str,
        workstreams: &[CopiedWorkstream],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query!(
            "DELETE FROM copied_workstreams WHERE repository = ?",
            repository
        )
        .execute(&mut *transaction)
        .await?;
        sqlx::query!("DELETE FROM copied_issues WHERE repository = ?", repository)
            .execute(&mut *transaction)
            .await?;
        sqlx::query!(
            "DELETE FROM copied_issue_labels WHERE repository = ?",
            repository
        )
        .execute(&mut *transaction)
        .await?;
        sqlx::query!(
            "DELETE FROM copied_blockers WHERE repository = ?",
            repository
        )
        .execute(&mut *transaction)
        .await?;
        for workstream in workstreams {
            insert_workstream(&mut transaction, repository, workstream).await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn has_workstream(
        &self,
        repository: &str,
        number: i64,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let stored = sqlx::query_scalar!(
            "SELECT number FROM copied_workstreams WHERE repository = ? AND number = ?",
            repository,
            number
        )
        .fetch_optional(self.pool)
        .await?;
        Ok(stored.is_some())
    }

    // Gives the Workstream of the issue. A stored Workstream is its own Workstream.
    // A row of another repository has the same number as an issue of this repository, so the URL must match.
    pub async fn workstream_of(
        &self,
        repository: &str,
        number: i64,
        repository_url: &str,
    ) -> Result<Option<i64>, Box<dyn Error + Send + Sync>> {
        Ok(sqlx::query_scalar!(
            r#"SELECT workstream AS "workstream!: i64" FROM (
                   SELECT number AS workstream, 0 AS rank FROM copied_workstreams
                   WHERE repository = ?1 AND number = ?2
                   UNION ALL
                   SELECT workstream, 1 FROM copied_issues
                   WHERE repository = ?1 AND number = ?2 AND repository_url = ?3
               ) ORDER BY rank LIMIT 1"#,
            repository,
            number,
            repository_url
        )
        .fetch_optional(self.pool)
        .await?)
    }

    // Gives the Workstreams of the repository that hold the issue as a task and
    // store its label `name` differently from `present`.
    pub async fn workstreams_with_label_change(
        &self,
        repository: &str,
        number: i64,
        repository_url: &str,
        name: &str,
        present: bool,
    ) -> Result<Vec<i64>, Box<dyn Error + Send + Sync>> {
        Ok(sqlx::query_scalar!(
            r#"SELECT i.workstream AS "workstream!: i64" FROM copied_issues i
               WHERE i.repository = ?1 AND i.number = ?2 AND i.repository_url = ?3
                 AND EXISTS (
                     SELECT 1 FROM copied_issue_labels l
                     WHERE l.repository = i.repository AND l.workstream = i.workstream
                       AND l.position = i.position AND l.name = ?4
                 ) != ?5"#,
            repository,
            number,
            repository_url,
            name,
            present
        )
        .fetch_all(self.pool)
        .await?)
    }

    // Gives the Workstreams of the repository whose trees have a blocker row in the Workstream `blocker_workstream`.
    pub async fn workstreams_with_blocker_in(
        &self,
        repository: &str,
        blocker_workstream: i64,
    ) -> Result<Vec<i64>, Box<dyn Error + Send + Sync>> {
        Ok(sqlx::query_scalar!(
            r#"SELECT DISTINCT workstream AS "workstream!: i64" FROM copied_blockers
               WHERE repository = ? AND blocker_workstream = ?"#,
            repository,
            blocker_workstream
        )
        .fetch_all(self.pool)
        .await?)
    }

    // Gives the Workstreams of the repository whose trees have a blocker row for the issue.
    pub async fn workstreams_with_blocker(
        &self,
        repository: &str,
        number: i64,
    ) -> Result<Vec<i64>, Box<dyn Error + Send + Sync>> {
        Ok(sqlx::query_scalar!(
            r#"SELECT DISTINCT workstream AS "workstream!: i64" FROM copied_blockers
               WHERE repository = ? AND number = ?"#,
            repository,
            number
        )
        .fetch_all(self.pool)
        .await?)
    }

    pub async fn add_workstream(
        &self,
        repository: &str,
        workstream: &CopiedWorkstream,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut transaction = self.pool.begin().await?;
        insert_workstream(&mut transaction, repository, workstream).await?;
        transaction.commit().await?;
        Ok(())
    }

    // Gives `false` when the stored values are equal.
    // The blockers of the repository that are in this Workstream get the new title too.
    pub async fn update_workstream(
        &self,
        repository: &str,
        number: i64,
        title: &str,
        body: &str,
        autopilot: bool,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let mut transaction = self.pool.begin().await?;
        let updated = sqlx::query!(
            "UPDATE copied_workstreams SET title = ?1, body = ?2, autopilot = ?3
             WHERE repository = ?4 AND number = ?5
               AND (title != ?1 OR body != ?2 OR autopilot != ?3)",
            title,
            body,
            autopilot,
            repository,
            number
        )
        .execute(&mut *transaction)
        .await?;
        let titled = sqlx::query!(
            "UPDATE copied_blockers SET blocker_workstream_title = ?1
             WHERE repository = ?2 AND blocker_workstream = ?3
               AND blocker_workstream_title IS NOT ?1",
            title,
            repository,
            number
        )
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(updated.rows_affected() > 0 || titled.rows_affected() > 0)
    }

    // Gives `false` when no blocker row has the issue.
    pub async fn remove_blocker(
        &self,
        repository: &str,
        number: i64,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let result = sqlx::query!(
            "DELETE FROM copied_blockers WHERE repository = ? AND number = ?",
            repository,
            number
        )
        .execute(self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn remove_workstream(
        &self,
        repository: &str,
        number: i64,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query!(
            "DELETE FROM copied_workstreams WHERE repository = ? AND number = ?",
            repository,
            number
        )
        .execute(&mut *transaction)
        .await?;
        delete_issues(&mut transaction, repository, number).await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn replace_issues(
        &self,
        repository: &str,
        workstream: i64,
        issues: &[CopiedIssue],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut transaction = self.pool.begin().await?;
        delete_issues(&mut transaction, repository, workstream).await?;
        insert_issues(&mut transaction, repository, workstream, issues).await?;
        transaction.commit().await?;
        Ok(())
    }

    // Changes each row of the issue in all repositories, and gives `false` when the stored values are equal.
    // A tree can hold an issue of another repository as a leaf, so the repository of the row can differ.
    pub async fn update_issue(
        &self,
        issue: &ChangedIssue,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let mut transaction = self.pool.begin().await?;
        let updated = sqlx::query!(
            "UPDATE copied_issues SET title = ?1, body = ?2, state = ?3, author = ?4
             WHERE number = ?5 AND repository_url = ?6
               AND (title != ?1 OR body != ?2 OR state != ?3 OR author != ?4)",
            issue.title,
            issue.body,
            issue.state,
            issue.author,
            issue.number,
            issue.repository_url
        )
        .execute(&mut *transaction)
        .await?;
        let mut changed = updated.rows_affected() > 0;
        let mut labels = issue.labels.clone();
        labels.sort();
        let rows = sqlx::query!(
            "SELECT repository, workstream, position FROM copied_issues
             WHERE number = ? AND repository_url = ?",
            issue.number,
            issue.repository_url
        )
        .fetch_all(&mut *transaction)
        .await?;
        for row in rows {
            let stored = sqlx::query_scalar!(
                "SELECT name FROM copied_issue_labels
                 WHERE repository = ? AND workstream = ? AND position = ? ORDER BY name",
                row.repository,
                row.workstream,
                row.position
            )
            .fetch_all(&mut *transaction)
            .await?;
            if stored == labels {
                continue;
            }
            changed = true;
            sqlx::query!(
                "DELETE FROM copied_issue_labels
                 WHERE repository = ? AND workstream = ? AND position = ?",
                row.repository,
                row.workstream,
                row.position
            )
            .execute(&mut *transaction)
            .await?;
            insert_labels(
                &mut transaction,
                &row.repository,
                row.workstream,
                row.position,
                &labels,
            )
            .await?;
        }
        transaction.commit().await?;
        Ok(changed)
    }
}

async fn insert_workstream(
    transaction: &mut Transaction<'_, Sqlite>,
    repository: &str,
    workstream: &CopiedWorkstream,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    sqlx::query!(
        "INSERT INTO copied_workstreams (repository, number, title, body, autopilot)
         VALUES (?, ?, ?, ?, ?)",
        repository,
        workstream.number,
        workstream.title,
        workstream.body,
        workstream.autopilot
    )
    .execute(&mut **transaction)
    .await?;
    insert_issues(
        transaction,
        repository,
        workstream.number,
        &workstream.issues,
    )
    .await
}

async fn delete_issues(
    transaction: &mut Transaction<'_, Sqlite>,
    repository: &str,
    workstream: i64,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    sqlx::query!(
        "DELETE FROM copied_issues WHERE repository = ? AND workstream = ?",
        repository,
        workstream
    )
    .execute(&mut **transaction)
    .await?;
    sqlx::query!(
        "DELETE FROM copied_issue_labels WHERE repository = ? AND workstream = ?",
        repository,
        workstream
    )
    .execute(&mut **transaction)
    .await?;
    sqlx::query!(
        "DELETE FROM copied_blockers WHERE repository = ? AND workstream = ?",
        repository,
        workstream
    )
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn insert_issues(
    transaction: &mut Transaction<'_, Sqlite>,
    repository: &str,
    workstream: i64,
    issues: &[CopiedIssue],
) -> Result<(), Box<dyn Error + Send + Sync>> {
    for (position, issue) in (0_i64..).zip(issues) {
        sqlx::query!(
            "INSERT INTO copied_issues (repository, workstream, position, number, parent, title, body,
                                       state, author, html_url, repository_url)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            repository,
            workstream,
            position,
            issue.number,
            issue.parent,
            issue.title,
            issue.body,
            issue.state,
            issue.author,
            issue.html_url,
            issue.repository_url
        )
        .execute(&mut **transaction)
        .await?;
        insert_labels(transaction, repository, workstream, position, &issue.labels).await?;
        for blocker in &issue.blockers {
            sqlx::query!(
                "INSERT INTO copied_blockers (repository, workstream, position, number,
                                              blocker_workstream, blocker_workstream_title)
                 VALUES (?, ?, ?, ?, ?, ?)",
                repository,
                workstream,
                position,
                blocker.number,
                blocker.workstream,
                blocker.workstream_title
            )
            .execute(&mut **transaction)
            .await?;
        }
    }
    Ok(())
}

async fn insert_labels(
    transaction: &mut Transaction<'_, Sqlite>,
    repository: &str,
    workstream: i64,
    position: i64,
    labels: &[String],
) -> Result<(), Box<dyn Error + Send + Sync>> {
    for label in labels {
        sqlx::query!(
            "INSERT INTO copied_issue_labels (repository, workstream, position, name)
             VALUES (?, ?, ?, ?)",
            repository,
            workstream,
            position,
            label
        )
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}
