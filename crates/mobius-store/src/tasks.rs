use std::error::Error;

use sqlx::sqlite::SqlitePool;
use time::OffsetDateTime;

pub struct Tasks<'a> {
    pub(crate) pool: &'a SqlitePool,
}

#[derive(Debug, PartialEq)]
pub struct Task {
    pub id: i64,
    pub issue: i64,
    pub workstream: i64,
    pub state: String,
    pub branch: Option<String>,
    pub fix_rounds: i64,
    // The number of Reviewer runs that ended with a result.
    pub review_rounds: i64,
    pub pull_request: Option<i64>,
    // The time of the newest comment that the Judge got.
    pub judged_at: Option<OffsetDateTime>,
    // The kind of the Worker that the task waits for or runs: `implementer`, `conflict_round`, `reviewer`, or `judge`.
    pub worker: Option<String>,
    // The first prompt of an Implementer, or the state of the task before a Judge.
    pub worker_input: Option<String>,
}

impl Tasks<'_> {
    pub async fn add(
        &self,
        repository: &str,
        issue: i64,
        workstream: i64,
    ) -> Result<Task, Box<dyn Error + Send + Sync>> {
        let dispatched_at = OffsetDateTime::now_utc();
        let task = sqlx::query_as!(
            Task,
            r#"INSERT INTO tasks (repository, issue, workstream, state, dispatched_at)
               VALUES (?, ?, ?, 'dispatched', ?)
               RETURNING id AS "id!", issue, workstream, state, branch, fix_rounds, review_rounds, pull_request,
                         judged_at AS "judged_at: OffsetDateTime", worker, worker_input"#,
            repository,
            issue,
            workstream,
            dispatched_at
        )
        .fetch_one(self.pool)
        .await?;
        Ok(task)
    }

    pub async fn live(
        &self,
        repository: &str,
        issue: i64,
    ) -> Result<Option<Task>, Box<dyn Error + Send + Sync>> {
        let task = sqlx::query_as!(
            Task,
            r#"SELECT id, issue, workstream, state, branch, fix_rounds, review_rounds, pull_request,
                      judged_at AS "judged_at: OffsetDateTime", worker, worker_input FROM tasks
               WHERE repository = ? AND issue = ? AND state <> 'ended'"#,
            repository,
            issue
        )
        .fetch_optional(self.pool)
        .await?;
        Ok(task)
    }

    // Also gives `true` for an ended task.
    pub async fn exists(
        &self,
        repository: &str,
        issue: i64,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let exists = sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM tasks WHERE repository = ? AND issue = ?) AS "exists!: bool""#,
            repository,
            issue
        )
        .fetch_one(self.pool)
        .await?;
        Ok(exists)
    }

    // Gives `false` when the task is not in the state `from`.
    pub async fn queue(&self, id: i64, from: &str) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let queued_at = OffsetDateTime::now_utc();
        let result = sqlx::query!(
            "UPDATE tasks SET state = 'queued', queued_at = ? WHERE id = ? AND state = ?",
            queued_at,
            id,
            from
        )
        .execute(self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    // Gives the id and the `queued_at` time of each queued task, in queue order.
    pub async fn queued(&self) -> Result<Vec<(i64, OffsetDateTime)>, Box<dyn Error + Send + Sync>> {
        let rows = sqlx::query!(
            r#"SELECT id AS "id!", queued_at AS "queued_at!: OffsetDateTime" FROM tasks
               WHERE state = 'queued' ORDER BY queued_at, id"#
        )
        .fetch_all(self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| (row.id, row.queued_at))
            .collect())
    }

    pub async fn set_worker(
        &self,
        id: i64,
        worker: &str,
        input: Option<&str>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        sqlx::query!(
            "UPDATE tasks SET worker = ?, worker_input = ? WHERE id = ?",
            worker,
            input,
            id
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    // A restart keeps the place of the task in the queue.
    pub async fn requeue(&self, id: i64) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let result = sqlx::query!(
            "UPDATE tasks SET state = 'queued' WHERE id = ? AND state IN ('queued', 'working')",
            id
        )
        .execute(self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    // Gives `false` when the task is not in the state `from`.
    pub async fn set_state(
        &self,
        id: i64,
        from: &str,
        to: &str,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let result = sqlx::query!(
            "UPDATE tasks SET state = ? WHERE id = ? AND state = ?",
            to,
            id,
            from
        )
        .execute(self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn set_branch(
        &self,
        id: i64,
        branch: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        sqlx::query!("UPDATE tasks SET branch = ? WHERE id = ?", branch, id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_pull_request(
        &self,
        id: i64,
        pull_request: i64,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        sqlx::query!(
            "UPDATE tasks SET pull_request = ? WHERE id = ?",
            pull_request,
            id
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn live_in(
        &self,
        repository: &str,
    ) -> Result<Vec<Task>, Box<dyn Error + Send + Sync>> {
        let tasks = sqlx::query_as!(
            Task,
            r#"SELECT id, issue, workstream, state, branch, fix_rounds, review_rounds, pull_request,
                      judged_at AS "judged_at: OffsetDateTime", worker, worker_input FROM tasks
               WHERE repository = ? AND state <> 'ended'"#,
            repository
        )
        .fetch_all(self.pool)
        .await?;
        Ok(tasks)
    }

    pub async fn active_count(&self) -> Result<i64, Box<dyn Error + Send + Sync>> {
        let count = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM tasks WHERE state IN ('dispatched', 'queued', 'working')"
        )
        .fetch_one(self.pool)
        .await?;
        Ok(count)
    }

    // Gives the pull request of each task of the Workstream, also of an ended task.
    pub async fn pull_requests(
        &self,
        repository: &str,
        workstream: i64,
    ) -> Result<Vec<i64>, Box<dyn Error + Send + Sync>> {
        let pull_requests = sqlx::query_scalar!(
            r#"SELECT pull_request AS "pull_request!" FROM tasks
               WHERE repository = ? AND workstream = ? AND pull_request IS NOT NULL"#,
            repository,
            workstream
        )
        .fetch_all(self.pool)
        .await?;
        Ok(pull_requests)
    }

    pub async fn live_repositories(&self) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        let repositories =
            sqlx::query_scalar!("SELECT DISTINCT repository FROM tasks WHERE state <> 'ended'")
                .fetch_all(self.pool)
                .await?;
        Ok(repositories)
    }

    pub async fn live_by_pull_request(
        &self,
        repository: &str,
        pull_request: i64,
    ) -> Result<Option<Task>, Box<dyn Error + Send + Sync>> {
        let task = sqlx::query_as!(
            Task,
            r#"SELECT id, issue, workstream, state, branch, fix_rounds, review_rounds, pull_request,
                      judged_at AS "judged_at: OffsetDateTime", worker, worker_input FROM tasks
               WHERE repository = ? AND pull_request = ? AND state <> 'ended'"#,
            repository,
            pull_request
        )
        .fetch_optional(self.pool)
        .await?;
        Ok(task)
    }

    // Gives `false` when the task has `max` fix rounds.
    pub async fn add_fix_round(
        &self,
        id: i64,
        max: u32,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let result = sqlx::query!(
            "UPDATE tasks SET fix_rounds = fix_rounds + 1 WHERE id = ? AND fix_rounds < ?",
            id,
            max
        )
        .execute(self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn add_review_round(&self, id: i64) -> Result<(), Box<dyn Error + Send + Sync>> {
        sqlx::query!(
            "UPDATE tasks SET review_rounds = review_rounds + 1 WHERE id = ?",
            id
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    // Gives the id of the pull request comment of the Reviewer run in progress.
    pub async fn review_comment(
        &self,
        id: i64,
    ) -> Result<Option<i64>, Box<dyn Error + Send + Sync>> {
        let comment = sqlx::query_scalar!("SELECT review_comment FROM tasks WHERE id = ?", id)
            .fetch_one(self.pool)
            .await?;
        Ok(comment)
    }

    pub async fn set_review_comment(
        &self,
        id: i64,
        comment: Option<i64>,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        sqlx::query!(
            "UPDATE tasks SET review_comment = ? WHERE id = ?",
            comment,
            id
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_judged_at(
        &self,
        id: i64,
        judged_at: OffsetDateTime,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        sqlx::query!("UPDATE tasks SET judged_at = ? WHERE id = ?", judged_at, id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    // The head of the pull request for which the last fix round for a failed check started.
    pub async fn check_head(
        &self,
        id: i64,
    ) -> Result<Option<String>, Box<dyn Error + Send + Sync>> {
        let head = sqlx::query_scalar!("SELECT check_head FROM tasks WHERE id = ?", id)
            .fetch_one(self.pool)
            .await?;
        Ok(head)
    }

    pub async fn set_check_head(
        &self,
        id: i64,
        head: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        sqlx::query!("UPDATE tasks SET check_head = ? WHERE id = ?", head, id)
            .execute(self.pool)
            .await?;
        Ok(())
    }

    // Gives the new number of Worker restarts, or `None` when the task has `max` Worker restarts.
    pub async fn add_worker_restart(
        &self,
        id: i64,
        max: u32,
    ) -> Result<Option<i64>, Box<dyn Error + Send + Sync>> {
        let row = sqlx::query!(
            r#"UPDATE tasks SET worker_restarts = worker_restarts + 1 WHERE id = ? AND worker_restarts < ?
               RETURNING worker_restarts AS "worker_restarts!""#,
            id,
            max
        )
        .fetch_optional(self.pool)
        .await?;
        Ok(row.map(|row| row.worker_restarts))
    }

    pub async fn reset_counters(&self, id: i64) -> Result<(), Box<dyn Error + Send + Sync>> {
        sqlx::query!(
            "UPDATE tasks SET fix_rounds = 0, review_rounds = 0, worker_restarts = 0 WHERE id = ?",
            id
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    // Gives `false` when the task is already stopped or ended.
    pub async fn stop(&self, id: i64) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let result = sqlx::query!(
            "UPDATE tasks SET state = 'stopped' WHERE id = ? AND state NOT IN ('stopped', 'ended')",
            id
        )
        .execute(self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn end(&self, id: i64) -> Result<(), Box<dyn Error + Send + Sync>> {
        sqlx::query!("UPDATE tasks SET state = 'ended' WHERE id = ?", id)
            .execute(self.pool)
            .await?;
        Ok(())
    }
}
