use rmcp::RoleServer;
use rmcp::model::{ProgressNotificationParam, ProgressToken};
use rmcp::service::Peer;

/// Sends MCP progress notifications to the client during long-running tool operations.
#[derive(Clone)]
pub struct ProgressSender {
    tx: tokio::sync::mpsc::UnboundedSender<ProgressNotificationParam>,
    token: ProgressToken,
}

impl ProgressSender {
    pub fn new(peer: Peer<RoleServer>, token: ProgressToken) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ProgressNotificationParam>();
        tokio::spawn(async move {
            while let Some(params) = rx.recv().await {
                if let Err(e) = peer.notify_progress(params).await {
                    tracing::debug!("[progress] notify failed: {e}");
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
        if self.tx.send(params).is_err() {
            tracing::debug!("[progress] progress receiver already closed");
        }
    }
}

pub type SharedProgressSender = std::sync::Arc<std::sync::Mutex<Option<ProgressSender>>>;
