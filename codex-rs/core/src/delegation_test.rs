#[cfg(test)]
mod tests {
    use crate::delegation::DelegationContext;
    use crate::delegation::DelegationRegistry;

    #[tokio::test]
    async fn test_delegation_context_root() {
        let ctx = DelegationContext::new_root("root-1".to_string());
        assert_eq!(ctx.delegation_id, "root-1");
        assert_eq!(ctx.parent_delegation_id, None);
        assert_eq!(ctx.depth, 0);
    }

    #[tokio::test]
    async fn test_delegation_context_child() {
        let root = DelegationContext::new_root("root-1".to_string());
        let child = root.new_child("child-1".to_string());

        assert_eq!(child.delegation_id, "child-1");
        assert_eq!(child.parent_delegation_id, Some("root-1".to_string()));
        assert_eq!(child.depth, 1);
    }

    #[tokio::test]
    async fn test_delegation_context_nested() {
        let root = DelegationContext::new_root("root-1".to_string());
        let child = root.new_child("child-1".to_string());
        let grandchild = child.new_child("grandchild-1".to_string());

        assert_eq!(grandchild.delegation_id, "grandchild-1");
        assert_eq!(grandchild.parent_delegation_id, Some("child-1".to_string()));
        assert_eq!(grandchild.depth, 2);
    }

    #[tokio::test]
    async fn test_delegation_registry_enter_root() {
        let registry = DelegationRegistry::new();

        let ctx = registry
            .enter_with_parent("delegation-1".to_string(), None)
            .await;

        assert_eq!(ctx.delegation_id, "delegation-1");
        assert_eq!(ctx.parent_delegation_id, None);
        assert_eq!(ctx.depth, 0);
    }

    #[tokio::test]
    async fn test_delegation_registry_enter_nested() {
        let registry = DelegationRegistry::new();

        // Enter first delegation
        let ctx1 = registry
            .enter_with_parent("delegation-1".to_string(), None)
            .await;
        assert_eq!(ctx1.depth, 0);

        // Enter nested delegation
        let ctx2 = registry
            .enter_with_parent("delegation-2".to_string(), Some(&ctx1))
            .await;
        assert_eq!(ctx2.delegation_id, "delegation-2");
        assert_eq!(ctx2.parent_delegation_id, Some("delegation-1".to_string()));
        assert_eq!(ctx2.depth, 1);
    }

    #[tokio::test]
    async fn parallel_delegations_are_siblings() {
        let registry = DelegationRegistry::new();

        // Simulate parallel calls - both should see no parent
        let ctx1 = registry
            .enter_with_parent("delegation-1".to_string(), None)
            .await;
        let ctx2 = registry
            .enter_with_parent("delegation-2".to_string(), None)
            .await;

        assert_eq!(ctx1.depth, 0);
        assert_eq!(ctx2.depth, 0);
        assert!(ctx1.parent_delegation_id.is_none());
        assert!(ctx2.parent_delegation_id.is_none());
    }
}
