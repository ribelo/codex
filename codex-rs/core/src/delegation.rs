//! Delegation context for tracking nested subagent calls.

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

/// Registry for managing active delegation contexts.
/// This used to track state, but now acts as a factory/helper.
#[derive(Debug, Default, Clone)]
pub struct DelegationRegistry;

impl DelegationRegistry {
    pub fn new() -> Self {
        Self
    }

    /// Enter a new delegation context with an explicit parent.
    /// If parent is None, creates a root context.
    pub async fn enter_with_parent(
        &self,
        delegation_id: String,
        parent: Option<&DelegationContext>,
    ) -> DelegationContext {
        match parent {
            Some(p) => p.new_child(delegation_id),
            None => DelegationContext::new_root(delegation_id),
        }
    }
}
