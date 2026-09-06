use crate::{NamespaceRegistry, NamespaceRegistryState, Result, invalid};
use coordination::{CLAIM_SNAPSHOT_SCHEMA, ClaimRef, ClaimScope, ClaimSnapshotV1};

impl NamespaceRegistryState {
    /// Resolve a wire registry from an independently read and verified claim snapshot.
    /// This validates data binding; callers still authenticate the Kubernetes object.
    /// # Errors
    /// Rejects schema, scope, digest or claim metadata mismatches.
    pub fn hydrate(&self, snapshot: &ClaimSnapshotV1) -> Result<NamespaceRegistry> {
        self.claim_snapshot.validate()?;
        snapshot.validate()?;
        validate_scope(self, &snapshot.scope)?;
        if snapshot.reference()? != self.claim_snapshot {
            return Err(invalid(
                "claim snapshot differs from protected registry reference",
            ));
        }
        Ok(NamespaceRegistry {
            registry_uid: self.registry_uid.clone(),
            next_sequence: self.next_sequence,
            schema_version: self.schema_version.clone(),
            cluster_binding: self.cluster_binding.clone(),
            workload_namespace: self.workload_namespace.clone(),
            claims: snapshot.claims.clone(),
            histories: self.histories.clone(),
            active_writer_checkpoints: self.active_writer_checkpoints.clone(),
            audit_heads: self.audit_heads.clone(),
            transitions: self.transitions.clone(),
            captured_claim_scope: snapshot.scope.clone(),
        })
    }
}

impl NamespaceRegistry {
    /// Project only this registry's captured scope and current claim tracking fields.
    /// # Errors
    /// Rejects changed scope and malformed or oversized claim metadata.
    pub fn claim_snapshot(&self) -> Result<ClaimSnapshotV1> {
        let scope = &self.captured_claim_scope;
        if self.registry_uid != scope.registry_uid
            || self.cluster_binding != scope.cluster_binding
            || self.workload_namespace != scope.workload_namespace
        {
            return Err(invalid("hydrated registry scope changed"));
        }
        let snapshot = ClaimSnapshotV1 {
            schema_version: CLAIM_SNAPSHOT_SCHEMA.into(),
            scope: scope.clone(),
            claims: self.claims.clone(),
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    /// Construct the wire DTO using a reference to exactly the current claims.
    /// # Errors
    /// Rejects a mismatched reference or an invalid hydrated registry.
    pub fn state(&self, reference: &ClaimRef) -> Result<NamespaceRegistryState> {
        if self.claim_snapshot()?.reference()? != *reference {
            return Err(invalid(
                "registry publication reference differs from current claims",
            ));
        }
        let state = NamespaceRegistryState {
            registry_uid: self.registry_uid.clone(),
            next_sequence: self.next_sequence,
            schema_version: self.schema_version.clone(),
            cluster_binding: self.cluster_binding.clone(),
            workload_namespace: self.workload_namespace.clone(),
            claim_snapshot: reference.clone(),
            histories: self.histories.clone(),
            active_writer_checkpoints: self.active_writer_checkpoints.clone(),
            audit_heads: self.audit_heads.clone(),
            transitions: self.transitions.clone(),
        };
        validate_scope(&state, &self.captured_claim_scope)?;
        Ok(state)
    }
}

fn validate_scope(state: &NamespaceRegistryState, scope: &ClaimScope) -> Result<()> {
    scope.validate()?;
    if state.schema_version != crate::REGISTRY_SCHEMA_VERSION
        || state.next_sequence == 0
        || state.registry_uid != scope.registry_uid
        || state.cluster_binding != scope.cluster_binding
        || state.workload_namespace != scope.workload_namespace
    {
        return Err(invalid(
            "registry schema or claim scope mismatch; explicit recovery required",
        ));
    }
    Ok(())
}
