use rmcp::RoleServer;
use rmcp::model::{ProgressNotificationParam, ProgressToken};
use rmcp::service::Peer;

enum ProgressCommand {
    Notify(ProgressNotificationParam),
    Flush(std::sync::mpsc::SyncSender<()>),
}

/// Sends MCP progress notifications to the client during long-running tool operations.
#[derive(Clone)]
pub struct ProgressSender {
    tx: tokio::sync::mpsc::UnboundedSender<ProgressCommand>,
    token: ProgressToken,
}

impl ProgressSender {
    pub fn new(peer: Peer<RoleServer>, token: ProgressToken) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ProgressCommand>();
        tokio::spawn(async move {
            while let Some(command) = rx.recv().await {
                match command {
                    ProgressCommand::Notify(params) => {
                        if let Err(e) = peer.notify_progress(params).await {
                            tracing::debug!("[progress] notify failed: {e}");
                        }
                    }
                    ProgressCommand::Flush(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        Self { tx, token }
    }

    pub fn send(&self, progress: f64, total: Option<f64>, message: Option<String>) {
        // ProgressNotificationParam is #[non_exhaustive] since rmcp 2.0 — build via ctor.
        let mut params = ProgressNotificationParam::new(self.token.clone(), progress);
        if let Some(total) = total {
            params = params.with_total(total);
        }
        if let Some(message) = message {
            params = params.with_message(message);
        }
        if self.tx.send(ProgressCommand::Notify(params)).is_err() {
            tracing::debug!("[progress] progress receiver already closed");
        }
    }

    /// Block until every previously queued progress notification has been
    /// handed to the MCP peer. Tool handlers run inside spawn_blocking, so this
    /// does not block the async server runtime that drains the queue.
    pub fn flush_blocking(&self, timeout: std::time::Duration) -> bool {
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(0);
        if self.tx.send(ProgressCommand::Flush(done_tx)).is_err() {
            return false;
        }
        done_rx.recv_timeout(timeout).is_ok()
    }
}

pub type SharedProgressSender = std::sync::Arc<std::sync::Mutex<Option<ProgressSender>>>;
