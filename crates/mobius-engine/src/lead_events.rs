use std::error::Error;

use mobius_domain::{InboxKind, Live, organization};
use mobius_runner::Session;
use mobius_store::NewInboxItem;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::lead::Recorder;
use crate::{Engine, chat, limits};

pub(crate) async fn add(
    engine: &Engine,
    repository: &str,
    workstream: i64,
    issue: Option<i64>,
    kind: &str,
    payload: &str,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let _order = engine.chat_order.lock().await;
    let message = engine
        .store
        .lead_events()
        .add(
            organization(repository),
            repository,
            workstream,
            issue,
            kind,
            payload,
        )
        .await?;
    engine.broadcast(Live::Message(message));
    chat::send_events(engine, repository, workstream).await
}

pub(crate) async fn failed(
    engine: &Engine,
    repository: &str,
    workstream: i64,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let events = engine.store.lead_events();
    for event in events.ready(repository, workstream).await? {
        let item = engine
            .store
            .inbox_items()
            .add(NewInboxItem {
                kind: InboxKind::LeadFailed,
                organization: organization(repository),
                repository,
                workstream,
                issue: workstream,
                text: &event.payload,
                link: "",
            })
            .await?;
        engine.broadcast(Live::Inbox(item));
        events.deliver(event.id).await?;
    }
    Ok(())
}

pub(crate) async fn turn(
    session: &Session,
    prompt: &str,
    recorder: &mut Recorder,
    updates: &mut UnboundedReceiver<Value>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    loop {
        recorder.prompt(prompt).await?;
        let result = {
            let turn = session.prompt(prompt);
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    biased;
                    result = &mut turn => break result,
                    Some(update) = updates.recv() => recorder.update(update).await?,
                }
            }
        };
        // The connection reads each update of the turn before the answer to the prompt.
        while let Ok(update) = updates.try_recv() {
            recorder.update(update).await?;
        }
        if let Err(error) = &result
            && limits::wait_out(recorder, session.harness(), error).await?
        {
            continue;
        }
        return Ok(result?);
    }
}
