use std::path::PathBuf;
use std::time::Duration;

use pyre_proto::PyreDaemonClient;
use tokio::sync::mpsc;

use crate::model::toast::{pane_event_to_toast, Toast};
use crate::rpc::client::try_connect_control;

// ─────────────────────────────────────────────────────────────────────────────
// Background push-event consumer
// ─────────────────────────────────────────────────────────────────────────────

/// Spawn the background task that long-polls `next_pane_event` and converts
/// daemon push events into [`Toast`] notifications.
///
/// Returns a channel receiver that the event loop can drain without awaiting
/// the RPC directly.  The task reconnects automatically on transport errors.
pub fn spawn_push_event_task(socket: PathBuf, ttl: Duration) -> mpsc::Receiver<Toast> {
    let (toast_tx, toast_rx) = mpsc::channel::<Toast>(64);
    tokio::spawn(async move {
        let mut seq: u64 = 0;
        let mut backoff = Duration::from_millis(200);
        loop {
            let client = match try_connect_control(&socket).await {
                Ok(c) => c,
                Err(_) => {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                    continue;
                }
            };
            backoff = Duration::from_millis(200);

            match client
                .next_pane_event(tarpc::context::current(), seq, 30_000)
                .await
            {
                Ok(Ok(events)) if !events.is_empty() => {
                    if let Some(last) = events.last() {
                        seq = last.seq;
                    }
                    for ev in &events {
                        if let Some(toast) = pane_event_to_toast(ev, ttl) {
                            // Silently drop if receiver is gone (TUI exiting).
                            let _ = toast_tx.try_send(toast);
                        }
                    }
                }
                Ok(Ok(_)) => {
                    // Normal long-poll timeout; loop immediately.
                }
                _ => {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                }
            }
        }
    });
    toast_rx
}

// ─────────────────────────────────────────────────────────────────────────────
// Background block-poll task
// ─────────────────────────────────────────────────────────────────────────────

/// Spawn the background task that polls `list_blocks` every 500 ms and
/// publishes snapshots via a [`tokio::sync::watch`] channel.
///
/// Returns the watch receiver.  The event loop does a non-blocking
/// `borrow_and_update` read without ever awaiting the RPC directly.
pub fn spawn_block_poll_task(
    client: PyreDaemonClient,
) -> tokio::sync::watch::Receiver<
    std::collections::HashMap<pyre_proto::PaneId, Vec<pyre_proto::Block>>,
> {
    use pyre_proto::{Block, PaneId};
    use std::collections::HashMap;

    let (blocks_tx, blocks_rx) = tokio::sync::watch::channel(HashMap::<PaneId, Vec<Block>>::new());
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let req = pyre_proto::blocks::ListBlocksReq {
                session: None,
                limit: 20,
            };
            // Apply a 2 s hard timeout so a stuck daemon cannot pin this task.
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                client.list_blocks(tarpc::context::current(), req),
            )
            .await;
            if let Ok(Ok(Ok(blocks))) = result {
                // Group by pane_id so the event loop can index directly.
                let mut map: HashMap<PaneId, Vec<Block>> = HashMap::new();
                for b in blocks {
                    map.entry(b.pane).or_default().push(b);
                }
                // send only fails when all receivers are dropped (TUI exited).
                if blocks_tx.send(map).is_err() {
                    break;
                }
            }
        }
    });
    blocks_rx
}
