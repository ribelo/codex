//! Delegation context for tracking nested subagent calls.

use std::sync::Arc;
use tokio::sync::RwLock;

#[cfg(test)]
#[path = "delegation_test.rs"]
mod delegation_test;

/// Context for tracking delegation hierarchy.
#[derive(Debug, Clone)]
pub struct DelegationContext {
    /// Unique identifier for this delegation.
    pub delegation_id: String,
    /// Parent delegation ID if this is a nested delegation.
    pub parent_delegation_id: Option<String>,
    /// Nesting depth (0 = top-level, 1 = first nested, etc.)
    pub depth: i32,
}

impl DelegationContext {
    /// Create a new top-level delegation context.
    pub fn new_root(delegation_id: String) -> Self {
        Self {
            delegation_id,
            parent_delegation_id: None,
            depth: 0,
        }
    }

    /// Create a nested delegation context from a parent.
    pub fn new_child(&self, delegation_id: String) -> Self {
        Self {
            delegation_id,
            parent_delegation_id: Some(self.delegation_id.clone()),
            depth: self.depth + 1,
        }
    }
}

/// Registry for managing active delegation contexts across a session.
#[derive(Debug, Default, Clone)]
pub struct DelegationRegistry {
    /// Current delegation context (if any).
    current: Arc<RwLock<Option<DelegationContext>>>,
}

impl DelegationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the current delegation context.
    pub async fn current(&self) -> Option<DelegationContext> {
        self.current.read().await.clone()
    }

    /// Set the current delegation context.
    pub async fn set_current(&self, ctx: Option<DelegationContext>) {
        *self.current.write().await = ctx;
    }

    /// Enter a new delegation context (either root or child).
    pub async fn enter(&self, delegation_id: String) -> DelegationContext {
        let current = self.current().await;
        let new_ctx = match current {
            Some(parent) => parent.new_child(delegation_id),
            None => DelegationContext::new_root(delegation_id),
        };
        self.set_current(Some(new_ctx.clone())).await;
        new_ctx
    }

    /// Exit the current delegation context, returning to the parent.
    pub async fn exit(&self) {
        // For now, just clear the current context
        // In a full implementation, we might want to track the stack
        self.set_current(None).await;
    }
}
