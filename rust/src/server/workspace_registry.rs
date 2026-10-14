// SPDX-License-Identifier: Apache-2.0
//! Connection-owned, read-only workspace state. The opaque identity is not an
//! authorization credential and must not be accepted as a selector from clients.
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceRegistry {
    pub id: String,
    pub revision: u64,
    pub primary: Option<String>,
    pub additional: Vec<String>,
}

pub(crate) type SharedWorkspaceRegistry = Arc<RwLock<WorkspaceRegistry>>;

pub(crate) fn new_registry() -> SharedWorkspaceRegistry {
    Arc::new(RwLock::new(WorkspaceRegistry {
        id: uuid::Uuid::new_v4().to_string(),
        revision: 0,
        primary: None,
        additional: Vec::new(),
    }))
}

impl WorkspaceRegistry {
    /// Returns true only when the effective authorization snapshot changes.
    pub fn update(&mut self, primary: String, mut additional: Vec<String>) -> bool {
        additional.sort();
        additional.dedup();
        additional.retain(|root| root != &primary);
        if self.primary.as_deref() == Some(&primary) && self.additional == additional {
            return false;
        }
        self.revision += 1;
        self.primary = Some(primary);
        self.additional = additional;
        true
    }
}
