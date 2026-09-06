//! Registry references are authenticated in Kubernetes; claim blobs are verified separately.
use crate::store::{decode_state, read_state, replace_state_at};
use crate::{
    NamespaceRegistry, NamespaceRegistryState, PlatformClient, RegistryAuthority, Result,
    Versioned, invalid,
};
use coordination::{CLAIM_SNAPSHOT_SCHEMA, ClaimRef, ClaimScope, ClaimSnapshotV1, ClaimStore};
use k8s_openapi::api::core::v1::ConfigMap;
use kube::Api;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

impl PlatformClient {
    fn claims_backend(&self) -> Result<&dyn ClaimStore> {
        self.claim_store
            .as_deref()
            .ok_or_else(|| invalid("registry operations require a configured claim store"))
    }

    fn claim_scope(&self, registry_uid: &str) -> ClaimScope {
        ClaimScope {
            registry_uid: registry_uid.into(),
            cluster_binding: self.cluster_binding.clone(),
            control_namespace: self.control_namespace.clone(),
            workload_namespace: self.workload_namespace.clone(),
        }
    }

    async fn verify_registry_anchor(&self, uid: &str) -> Result<()> {
        let authority: Versioned<RegistryAuthority> = read_state(
            &self.client,
            crate::REGISTRY_AUTHORITY_NAMESPACE,
            &crate::registry_authority_name(&self.control_namespace, &self.workload_namespace)?,
        )
        .await?;
        if authority.value.schema_version != crate::REGISTRY_AUTHORITY_SCHEMA_VERSION
            || authority.value.registry_uid != uid
            || authority.value.control_namespace != self.control_namespace
            || authority.value.cluster_binding != self.cluster_binding
            || authority.value.workload_namespace != self.workload_namespace
        {
            return Err(invalid(
                "registry incarnation authority mismatch; explicit platform recovery required",
            ));
        }
        Ok(())
    }

    /// Authenticate the protected registry reference before resolving its immutable claims.
    /// # Errors
    /// Rejects legacy schemas, mismatched authority, missing snapshots and corrupt backend results.
    pub async fn load_registry(&self) -> Result<Versioned<NamespaceRegistry>> {
        self.claims_backend()?;
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let object = api
            .get(&format!("reakt-registry-{}", self.workload_namespace))
            .await?;
        let raw: Versioned<Value> = decode_state(&object)?;
        if raw.value.get("schemaVersion").and_then(Value::as_str)
            != Some(crate::REGISTRY_SCHEMA_VERSION)
        {
            return Err(invalid(
                "registry claim snapshot schema requires explicit recovery; automatic legacy migration is forbidden",
            ));
        }
        let state: NamespaceRegistryState = serde_json::from_value(raw.value)
            .map_err(|_| invalid("registry snapshot state shape invalid"))?;
        if state.registry_uid != raw.uid
            || state.next_sequence == 0
            || state.cluster_binding != self.cluster_binding
            || state.workload_namespace != self.workload_namespace
        {
            return Err(invalid("protected registry scope mismatch"));
        }
        deploy::allocation_names(&raw.uid, 1)?;
        self.verify_registry_anchor(&raw.uid).await?;
        if state.claim_snapshot.scope != self.claim_scope(&raw.uid) {
            return Err(invalid(
                "claim reference scope differs from protected registry",
            ));
        }
        state.claim_snapshot.validate()?;
        let snapshot = self
            .claims_backend()?
            .read_claims(&state.claim_snapshot)
            .await?
            .ok_or_else(|| {
                invalid("protected registry claim snapshot missing; explicit recovery required")
            })?;
        let mut registry = Versioned {
            value: state.hydrate(&snapshot)?,
            uid: raw.uid,
            resource_version: raw.resource_version,
        };
        self.validate_registry_allocations(&registry.value)?;
        remove_obsolete_tls_digests(&mut registry.value);
        Ok(registry)
    }

    async fn publish_claims(&self, snapshot: &ClaimSnapshotV1) -> Result<ClaimRef> {
        snapshot.validate()?;
        let expected = snapshot.reference()?;
        let stored = self.claims_backend()?.put_claims(snapshot).await?;
        if stored != expected {
            return Err(invalid(
                "claim store returned a different publication reference",
            ));
        }
        let actual = self
            .claims_backend()?
            .read_claims(&stored)
            .await?
            .ok_or_else(|| invalid("claim snapshot not readable after publication"))?;
        actual.validate()?;
        if actual != *snapshot || actual.reference()? != expected {
            return Err(invalid("claim store publication verification failed"));
        }
        Ok(stored)
    }

    pub(crate) async fn persist_registry(
        &self,
        previous: &Versioned<NamespaceRegistry>,
        next: &NamespaceRegistry,
    ) -> Result<Versioned<NamespaceRegistry>> {
        self.claims_backend()?;
        let scope = self.claim_scope(&previous.uid);
        if previous.value.captured_claim_scope != scope
            || next.captured_claim_scope != scope
            || previous.value.registry_uid != previous.uid
            || next.registry_uid != previous.uid
        {
            return Err(invalid(
                "registry mutation changed its captured scope or incarnation",
            ));
        }
        self.validate_registry_allocations(next)?;
        let snapshot = next.claim_snapshot()?;
        let expected = snapshot.reference()?;
        let state = next.state(&expected)?;
        validate_state_size(&state)?;
        self.publish_claims(&snapshot).await?;
        let written = replace_state_at(
            &self.client,
            &self.control_namespace,
            &format!("reakt-registry-{}", self.workload_namespace),
            &previous.uid,
            &previous.resource_version,
            &state,
        )
        .await?;
        if written.value != state {
            return Err(invalid(
                "registry admission changed the protected publication candidate",
            ));
        }
        let hydrated = written.value.hydrate(&snapshot)?;
        self.validate_registry_allocations(&hydrated)?;
        Ok(Versioned {
            value: hydrated,
            uid: written.uid,
            resource_version: written.resource_version,
        })
    }

    /// Initialize a truly empty registry after its Kubernetes UID and authority anchor exist.
    /// No legacy state, claims, allocation history or transitions are adopted.
    /// # Errors
    /// Rejects existing data, changed UID/RV, absent authority and unavailable claim storage.
    pub async fn bootstrap_registry(
        &self,
        expected_uid: &str,
        expected_resource_version: &str,
        domain_ids: &[String],
    ) -> Result<Versioned<NamespaceRegistry>> {
        self.claims_backend()?;
        let scope = self.claim_scope(expected_uid);
        scope.validate()?;
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let object = api
            .get(&format!("reakt-registry-{}", self.workload_namespace))
            .await?;
        if object.metadata.uid.as_deref() != Some(expected_uid)
            || object.metadata.resource_version.as_deref() != Some(expected_resource_version)
            || object.metadata.deletion_timestamp.is_some()
            || object.data.as_ref().is_some_and(|data| !data.is_empty())
            || object
                .binary_data
                .as_ref()
                .is_some_and(|data| !data.is_empty())
            || expected_resource_version.is_empty()
            || expected_resource_version == "0"
        {
            return Err(invalid(
                "registry bootstrap requires exact UID/RV and empty uninitialized data",
            ));
        }
        self.verify_registry_anchor(expected_uid).await?;
        let mut histories = BTreeMap::new();
        if domain_ids.len() > 4096 {
            return Err(invalid("too many bootstrap domains"));
        }
        for domain in domain_ids {
            if !crate::valid_namespace(domain) || histories.contains_key(domain) {
                return Err(invalid("invalid or duplicate bootstrap domain"));
            }
            histories.insert(
                domain.clone(),
                deploy::GenerationHistory {
                    domain_id: domain.clone(),
                    cluster_binding: self.cluster_binding.clone(),
                    revision: expected_resource_version.into(),
                    active_allocation: None,
                },
            );
        }
        let snapshot = ClaimSnapshotV1 {
            schema_version: CLAIM_SNAPSHOT_SCHEMA.into(),
            scope,
            claims: vec![],
        };
        let state = NamespaceRegistryState {
            registry_uid: expected_uid.into(),
            next_sequence: 1,
            schema_version: crate::REGISTRY_SCHEMA_VERSION.into(),
            cluster_binding: self.cluster_binding.clone(),
            workload_namespace: self.workload_namespace.clone(),
            claim_snapshot: snapshot.reference()?,
            histories,
            active_writer_checkpoints: BTreeMap::new(),
            transitions: BTreeMap::new(),
        };
        validate_state_size(&state)?;
        self.publish_claims(&snapshot).await?;
        let written = replace_state_at(
            &self.client,
            &self.control_namespace,
            &format!("reakt-registry-{}", self.workload_namespace),
            expected_uid,
            expected_resource_version,
            &state,
        )
        .await?;
        if written.value != state {
            return Err(invalid(
                "registry admission changed the protected publication candidate",
            ));
        }
        let hydrated = written.value.hydrate(&snapshot)?;
        self.validate_registry_allocations(&hydrated)?;
        Ok(Versioned {
            value: hydrated,
            uid: written.uid,
            resource_version: written.resource_version,
        })
    }

    fn validate_registry_allocations(&self, registry: &NamespaceRegistry) -> Result<()> {
        for (domain_id, history) in &registry.histories {
            if &history.domain_id != domain_id
                || history.cluster_binding != registry.cluster_binding
            {
                return Err(invalid("active allocation history scope mismatch"));
            }
            if let Some(allocation) = &history.active_allocation {
                deploy::validate_allocation(allocation)?;
                if allocation.registry_uid != registry.registry_uid
                    || allocation.sequence >= registry.next_sequence
                {
                    return Err(invalid(
                        "active allocation is outside protected registry sequence",
                    ));
                }
            }
        }
        for domain in registry.active_writer_checkpoints.keys() {
            crate::writer_checkpoint::active(registry, domain, &self.control_namespace)?;
        }
        let mut sequences = BTreeSet::new();
        for (transition_id, stored) in &registry.transitions {
            let allocation = &stored.plan.generation_reservation;
            deploy::validate_allocation(allocation)?;
            if allocation.registry_uid != registry.registry_uid
                || allocation.sequence >= registry.next_sequence
                || !sequences.insert(allocation.sequence)
                || transition_id != &allocation.transition_id
                || stored.plan.request.registry.registry_uid != registry.registry_uid
                || stored.plan.request.registry.next_sequence != allocation.sequence
            {
                return Err(invalid(
                    "stored reservation is outside protected registry allocation authority",
                ));
            }
        }
        Ok(())
    }
}

fn validate_state_size(state: &NamespaceRegistryState) -> Result<()> {
    if compiler_core::canonical_bytes(state)?.len() > crate::MAX_STATE_BYTES {
        return Err(invalid("registry state exceeds control object budget"));
    }
    Ok(())
}

fn remove_obsolete_tls_digests(registry: &mut NamespaceRegistry) {
    for stored in registry.transitions.values_mut() {
        let key = format!(
            "v1/Secret/{}/{}-tls",
            stored
                .plan
                .request
                .next
                .compiled
                .ir
                .domain
                .control_namespace,
            stored.plan.generation_reservation.service_account_name
        );
        stored.staged_content_digests.remove(&key);
    }
}
