mod chat_messages;
mod device_logins;
mod events;
mod github_apps;
mod harness_pauses;
mod inbox_items;
mod lead_events;
mod sessions;
mod sync_cursors;
mod tasks;
mod transcript;
mod workstream_copy;

use std::error::Error;
use std::fs;
use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool};

pub use chat_messages::ChatMessages;
pub use device_logins::DeviceLogins;
pub use events::Events;
pub use github_apps::{GitHubAppRow, GitHubApps};
pub use harness_pauses::{HarnessPauses, Pause};
pub use inbox_items::{InboxItems, NewInboxItem};
pub use lead_events::{LeadEvent, LeadEvents};
pub use sessions::{NewSession, OpenSession, Sessions};
pub use sync_cursors::{SyncCursor, SyncCursors};
pub use tasks::{Task, Tasks};
pub use transcript::Transcript;
pub use workstream_copy::{
    ChangedIssue, CopiedBlocker, CopiedIssue, CopiedWorkstream, WorkstreamCopy,
};

#[derive(Clone)]
pub struct Store {
    pub pool: SqlitePool,
}

impl Store {
    pub async fn open(data_dir: &Path) -> Result<Store, Box<dyn Error + Send + Sync>> {
        fs::create_dir_all(data_dir)?;
        let options = SqliteConnectOptions::new()
            .filename(data_dir.join("mobius.db"))
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePool::connect_with(options).await?;
        sqlx::migrate!().run(&pool).await?;
        Ok(Store { pool })
    }

    pub fn chat_messages(&self) -> ChatMessages<'_> {
        ChatMessages { pool: &self.pool }
    }

    pub fn device_logins(&self) -> DeviceLogins<'_> {
        DeviceLogins { pool: &self.pool }
    }

    pub fn events(&self) -> Events<'_> {
        Events { pool: &self.pool }
    }

    pub fn github_apps(&self) -> GitHubApps<'_> {
        GitHubApps { pool: &self.pool }
    }

    pub fn harness_pauses(&self) -> HarnessPauses<'_> {
        HarnessPauses { pool: &self.pool }
    }

    pub fn inbox_items(&self) -> InboxItems<'_> {
        InboxItems { pool: &self.pool }
    }

    pub fn lead_events(&self) -> LeadEvents<'_> {
        LeadEvents { pool: &self.pool }
    }

    pub fn sessions(&self) -> Sessions<'_> {
        Sessions { pool: &self.pool }
    }

    pub fn sync_cursors(&self) -> SyncCursors<'_> {
        SyncCursors { pool: &self.pool }
    }

    pub fn tasks(&self) -> Tasks<'_> {
        Tasks { pool: &self.pool }
    }

    pub fn transcript(&self) -> Transcript<'_> {
        Transcript { pool: &self.pool }
    }

    pub fn workstream_copy(&self) -> WorkstreamCopy<'_> {
        WorkstreamCopy { pool: &self.pool }
    }
}
