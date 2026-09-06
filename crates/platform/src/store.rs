#[cfg(feature = "deployer")]
use crate::StoredTransition;
use crate::{
    DeploymentCredential, MAX_STATE_BYTES, NamespaceRegistry, PolicyState, Result, SCHEMA_VERSION,
    Versioned, invalid, valid_name,
};
use compiler_core::{canonical_bytes, parse_strict};
#[cfg(feature = "deployer")]
use deploy::{
    ClaimState, DeploymentRequest, ProtectedClaim, RegistrySnapshot, begin, plan_transition,
};
use k8s_openapi::api::{
    authentication::v1::{BoundObjectReference, TokenRequest, TokenRequestSpec},
    core::v1::{ConfigMap, Secret, ServiceAccount},
};
use kube::{Api, Client, api::PostParams};
use serde::{Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;

pub(crate) fn decode_state<T: DeserializeOwned>(object: &ConfigMap) -> Result<Versioned<T>> {
    if object.metadata.deletion_timestamp.is_some() {
        return Err(invalid("control object is being deleted"));
    }
    let uid = object
        .metadata
        .uid
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("control object lacks UID"))?;
    let resource_version = object
        .metadata
        .resource_version
        .clone()
        .filter(|s| !s.is_empty() && s != "0")
        .ok_or_else(|| invalid("control object lacks resourceVersion"))?;
    let data = object
        .data
        .as_ref()
        .ok_or_else(|| invalid("control object lacks data"))?;
    let encoded = data
        .get("state.json")
        .ok_or_else(|| invalid("control object lacks state.json"))?;
    if data.len() != 1
        || object
            .binary_data
            .as_ref()
            .is_some_and(|data| !data.is_empty())
        || encoded.len() > MAX_STATE_BYTES
    {
        return Err(invalid("control object exceeds its schema or size budget"));
    }
    let value = serde_json::from_value(parse_strict(encoded)?)?;
    Ok(Versioned {
        value,
        uid,
        resource_version,
    })
}

pub(crate) async fn read_state<T: DeserializeOwned>(
    client: &Client,
    namespace: &str,
    name: &str,
) -> Result<Versioned<T>> {
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    decode_state(&api.get(name).await?)
}

pub(crate) async fn replace_state<T: Serialize + DeserializeOwned>(
    client: &Client,
    namespace: &str,
    name: &str,
    previous: &Versioned<T>,
    next: &T,
) -> Result<Versioned<T>> {
    replace_state_at(
        client,
        namespace,
        name,
        &previous.uid,
        &previous.resource_version,
        next,
    )
    .await
}

pub(crate) async fn replace_state_at<T: Serialize + DeserializeOwned>(
    client: &Client,
    namespace: &str,
    name: &str,
    expected_uid: &str,
    expected_revision: &str,
    next: &T,
) -> Result<Versioned<T>> {
    let encoded = canonical_bytes(next)?;
    if encoded.len() > MAX_STATE_BYTES {
        return Err(invalid(
            "control state exceeds 512 KiB; split or reduce the domain",
        ));
    }
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    let mut object = api.get(name).await?;
    if object.metadata.uid.as_deref() != Some(expected_uid)
        || object.metadata.resource_version.as_deref() != Some(expected_revision)
        || object.metadata.deletion_timestamp.is_some()
    {
        return Err(invalid(
            "control object UID/resourceVersion changed; reload owning transition",
        ));
    }
    object.data = Some(BTreeMap::from([(
        "state.json".into(),
        String::from_utf8(encoded).map_err(|_| invalid("invalid encoded state"))?,
    )]));
    // Preserve all platform metadata; UID and resourceVersion remain attached to
    // the PUT. Concurrent writes are rejected by the API server's atomic CAS.
    let updated = api.replace(name, &PostParams::default(), &object).await?;
    if updated.metadata.uid.as_deref() != Some(expected_uid) {
        return Err(invalid("replacement changed control-object UID"));
    }
    decode_state(&updated)
}

#[derive(Clone)]
pub struct PlatformClient {
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) phase_pause: Option<std::sync::Arc<crate::phase_interruption::PhasePause>>,
    #[cfg(feature = "deployer")]
    pub(crate) selection: Option<std::sync::Arc<crate::transition_session::Selection>>,
    #[cfg(feature = "deployer")]
    pub(crate) issued_executor: Option<std::sync::Arc<crate::executor_binding::IssuedExecutor>>,
    pub(crate) claim_store: Option<std::sync::Arc<dyn coordination::ClaimStore>>,
    pub(crate) audit_store: Option<std::sync::Arc<dyn coordination::AuditStore>>,
    pub(crate) review_store: Option<std::sync::Arc<dyn coordination::ReviewStore>>,
    pub(crate) client: Client,
    pub(crate) control_namespace: String,
    pub(crate) workload_namespace: String,
    pub(crate) cluster_binding: String,
}

impl PlatformClient {
    /// Client credentials are supplied by the trusted deployment environment.
    /// This constructor never loads project kubeconfigs or executes auth hooks.
    /// # Errors
    /// Rejects overlapping control/workload scopes and invalid names.
    pub fn new(
        client: Client,
        control_namespace: &str,
        workload_namespace: &str,
        cluster_binding: &str,
    ) -> Result<Self> {
        if !crate::valid_namespace(control_namespace)
            || !crate::valid_namespace(workload_namespace)
            || control_namespace == workload_namespace
            || control_namespace == crate::REGISTRY_AUTHORITY_NAMESPACE
            || workload_namespace == crate::REGISTRY_AUTHORITY_NAMESPACE
            || cluster_binding.is_empty()
            || cluster_binding.len() > 256
        {
            return Err(invalid(
                "platform scope must have distinct fixed control and workload namespaces",
            ));
        }
        Ok(Self {
            #[cfg(all(test, feature = "test-support"))]
            phase_pause: None,
            #[cfg(feature = "deployer")]
            selection: None,
            #[cfg(feature = "deployer")]
            issued_executor: None,
            claim_store: None,
            audit_store: None,
            review_store: None,
            client,
            control_namespace: control_namespace.into(),
            workload_namespace: workload_namespace.into(),
            cluster_binding: cluster_binding.into(),
        })
    }

    /// Attach the immutable claim store used only by registry operations.
    #[must_use]
    pub fn with_claim_store(mut self, store: std::sync::Arc<dyn coordination::ClaimStore>) -> Self {
        self.claim_store = Some(store);
        self
    }

    /// Attach immutable transition audit storage used by registry operations.
    #[must_use]
    pub fn with_audit_store(mut self, store: std::sync::Arc<dyn coordination::AuditStore>) -> Self {
        self.audit_store = Some(store);
        self
    }

    /// Attach immutable declaration metadata storage for deployment sessions and reviews.
    /// Runtime registry reads do not require this backend.
    #[must_use]
    pub fn with_review_store(
        mut self,
        store: std::sync::Arc<dyn coordination::ReviewStore>,
    ) -> Self {
        self.review_store = Some(store);
        self
    }

    /// Validate the committed writer checkpoint. Missing pre-cutover evidence
    /// requires explicit recovery; this schema cannot recover it from old manifests.
    /// # Errors
    /// Rejects altered history, unfinished transitions, missing qualified evidence or CAS races.
    pub async fn ensure_active_writer_checkpoint(
        &self,
        domain: &str,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let current = self.load_registry().await?;
        let history = current
            .value
            .histories
            .get(domain)
            .ok_or_else(|| invalid("active writer history missing"))?;
        let Some(_) = &history.active_allocation else {
            return Ok(current);
        };
        if current.value.active_writer_checkpoints.contains_key(domain) {
            crate::writer_checkpoint::active(&current.value, domain, &self.control_namespace)?;
            return Ok(current);
        }
        Err(invalid(
            "active writer checkpoint missing; manifest-free schema requires explicit recovery",
        ))
    }

    #[cfg(feature = "deployer")]
    pub(crate) async fn verify_new_writer_qualification(
        &self,
        transition: &str,
        executor: &str,
    ) -> Result<()> {
        let registry = self.load_registry().await?;
        let stored = self.selected(&registry, transition)?;
        deploy::verify_record(&stored.plan, &stored.record)?;
        if stored.record.executor_id != executor {
            return Err(invalid("writer qualification requires owning executor"));
        }
        crate::writer_checkpoint::require_qualified(&stored)
    }

    /// Validate the immutable writer authority captured with an upgrade reservation.
    /// Missing evidence requires explicit recovery; it is never inferred from history.
    /// # Errors
    /// Rejects forged records, missing checkpoints and changed authority.
    #[cfg(feature = "deployer")]
    pub async fn ensure_previous_writer_checkpoint(
        &self,
        transition: &str,
        executor: &str,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let current = self.load_registry().await?;
        let stored = self.selected(&current, transition)?;
        deploy::verify_record(&stored.plan, &stored.record)?;
        if stored.record.executor_id != executor {
            return Err(invalid("writer checkpoint requires the owning executor"));
        }
        let metadata = stored
            .plan
            .request
            .predecessor()
            .ok_or_else(|| invalid("initial installation has no previous writer"))?;
        if let Some(checkpoint) = &stored.previous_writer_checkpoint {
            checkpoint.validate(
                metadata,
                &self.control_namespace,
                &self.workload_namespace,
                &self.cluster_binding,
            )?;
            return Ok(current);
        }
        Err(invalid(
            "previous writer checkpoint missing; explicit recovery required",
        ))
    }

    /// # Errors
    /// Rejects malformed or differently scoped independent platform policy.
    pub async fn load_policy(&self, domain_id: &str) -> Result<Versioned<PolicyState>> {
        if !valid_name(domain_id) {
            return Err(invalid("invalid domain identifier"));
        }
        let policy: Versioned<PolicyState> = read_state(
            &self.client,
            &self.control_namespace,
            &format!("reakt-policy-{domain_id}"),
        )
        .await?;
        if policy.value.schema_version != SCHEMA_VERSION {
            return Err(invalid("unsupported policy schema"));
        }
        for ceiling in [&policy.value.runtime_ceiling, &policy.value.cleanup_ceiling] {
            if ceiling.domain_id != domain_id
                || ceiling.cluster_binding != self.cluster_binding
                || ceiling.workload_namespace != self.workload_namespace
                || ceiling.control_namespace != self.control_namespace
            {
                return Err(invalid("independent policy scope mismatch"));
            }
        }
        Ok(policy)
    }

    /// Fetch platform-owned artifact trust configuration for the trusted release
    /// verifier. No policy is accepted from the artifact or project checkout.
    /// # Errors
    /// Rejects invalid names, absent records and invalid/oversized JSON.
    pub async fn load_trust_policy(&self, domain_id: &str) -> Result<Versioned<serde_json::Value>> {
        if !valid_name(domain_id) {
            return Err(invalid("invalid domain identifier"));
        }
        read_state(
            &self.client,
            &self.control_namespace,
            &format!("reakt-trust-{domain_id}"),
        )
        .await
    }

    #[cfg(feature = "deployer")]
    pub(crate) fn validate_reservation_snapshot(
        &self,
        current: &Versioned<NamespaceRegistry>,
        request: &DeploymentRequest,
    ) -> Result<Option<crate::WriterCheckpoint>> {
        let domain = &request.next.compiled.ir.domain;
        let mut history = current
            .value
            .histories
            .get(&domain.id)
            .cloned()
            .ok_or_else(|| invalid("platform must provision domain allocation authority"))?;
        history.revision.clone_from(&current.resource_version);
        let expected = RegistrySnapshot {
            registry_uid: current.uid.clone(),
            next_sequence: current.value.next_sequence,
            revision: current.resource_version.clone(),
            claims: current.value.claims.clone(),
            generation_history: history,
        };
        if request.registry != expected
            || domain.namespace != self.workload_namespace
            || domain.control_namespace != self.control_namespace
            || domain.cluster_binding != self.cluster_binding
        {
            return Err(invalid(
                "request was not planned against this protected registry snapshot",
            ));
        }
        request
            .predecessor()
            .map(|previous| {
                let checkpoint = crate::writer_checkpoint::active(
                    &current.value,
                    &domain.id,
                    &self.control_namespace,
                )?;
                checkpoint.validate(
                    previous,
                    &self.control_namespace,
                    &self.workload_namespace,
                    &self.cluster_binding,
                )?;
                Ok::<_, crate::Error>(checkpoint.clone())
            })
            .transpose()
    }

    /// Atomically consume one registry sequence and reserve claims and transition
    /// ownership in one platform-provisioned namespace registry `ConfigMap`.
    /// # Errors
    /// Rejects stale plans, concurrent deployers, claim overlap and identity reuse.
    #[cfg(feature = "deployer")]
    pub async fn reserve_transition(
        &self,
        request: &DeploymentRequest,
        executor_id: &str,
        verified: &release::VerifiedRelease,
        inventory: &dyn inventory::InventoryStore,
    ) -> Result<Versioned<NamespaceRegistry>> {
        if request.next.compiled != *verified.compiled()
            || request.next.artifact_digest != verified.release_digest()
            || verified.manifest().image.reference.len() > 4096
        {
            return Err(invalid(
                "reservation requires the exact freshly verified release",
            ));
        }
        self.verify_release_trust(
            &request.next.compiled.ir.domain.id,
            verified.trust_policy_digest(),
        )
        .await?;
        Box::pin(self.reserve_authenticated(
            request,
            executor_id,
            inventory,
            Some(verified.trust_policy_digest()),
        ))
        .await
    }

    #[cfg(feature = "deployer")]
    pub(crate) async fn reserve_authenticated(
        &self,
        request: &DeploymentRequest,
        executor_id: &str,
        inventory: &dyn inventory::InventoryStore,
        trust_policy_digest: Option<&str>,
    ) -> Result<Versioned<NamespaceRegistry>> {
        self.verify_issued_identity(&request.next.compiled.ir.domain.id)
            .await?;
        if !request.observations.is_empty() {
            return Err(invalid("compact transitions require empty observations"));
        }
        if let Some(previous) = request.predecessor() {
            let id = inventory::snapshot_id(&previous.scope, &previous.inventory)?;
            let snapshot = inventory
                .read_snapshot(&previous.scope, &id)
                .await?
                .ok_or_else(|| {
                    invalid("predecessor inventory snapshot missing before reservation")
                })?;
            if snapshot != previous.inventory
                || inventory::snapshot_id(&previous.scope, &snapshot)? != id
            {
                return Err(invalid(
                    "predecessor inventory snapshot differs before reservation",
                ));
            }
        }
        let current = self.load_registry().await?;
        let next_sequence = current
            .value
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| invalid("registry allocation counter exhausted"))?;
        let domain = &request.next.compiled.ir.domain;
        let previous_writer_checkpoint = self.validate_reservation_snapshot(&current, request)?;
        let policy = self.load_policy(&domain.id).await?;
        if policy.value.runtime_ceiling != request.runtime_ceiling
            || policy.value.cleanup_ceiling != request.cleanup_ceiling
        {
            return Err(invalid(
                "request ceiling differs from independent platform policy",
            ));
        }
        let plan = plan_transition(request)?;
        let executor_binding = self.reservation_executor_binding(
            &current.uid,
            &plan.generation_reservation,
            &domain.id,
            executor_id,
        )?;
        let (review_metadata, previous_review_metadata) =
            self.prepare_review_metadata(&current.value, &plan).await?;
        self.verify_ownership_intents(request).await?;
        let record = begin(&plan, executor_id)?;
        deploy::require_event_capacity(&plan, &record)?;
        let profile = self
            .select_evidence_profile(previous_writer_checkpoint.as_ref())
            .await?;
        let mut next = current.value.clone();
        if next.transitions.values().any(|entry| {
            entry.domain_id() == domain.id && entry.record.phase != deploy::Phase::Committed
        }) {
            return Err(invalid(
                "domain has an unfinished transition; recover it without takeover",
            ));
        }
        reserve_claims(&mut next, &plan);
        next.next_sequence = next_sequence;
        let inputs = crate::TransitionInputs::freeze(
            &plan,
            current.value.claim_snapshot()?.reference()?,
            &policy.value,
        )?;
        let mut stored = StoredTransition::reserved(
            inputs,
            record,
            crate::CapacityAdmission::projection_placeholder(profile.clone()),
            review_metadata.reference()?,
            previous_review_metadata,
            executor_binding,
        );
        stored.previous_writer_checkpoint = previous_writer_checkpoint;
        next.transitions
            .insert(request.transition_id.clone(), stored);
        let capacity = crate::completion_envelope::calculate(&next, &plan, &profile)?;
        next.transitions
            .get_mut(&request.transition_id)
            .ok_or_else(|| invalid("reserved transition disappeared during capacity admission"))?
            .capacity = capacity;
        self.registry_candidate(&next)?;
        if let Some(trust) = trust_policy_digest {
            self.verify_release_trust(&domain.id, trust).await?;
        }
        self.verify_issued_identity(&domain.id).await?;
        self.publish_review_metadata(&review_metadata).await?;
        if let Some(trust) = trust_policy_digest {
            self.verify_release_trust(&domain.id, trust).await?;
        }
        self.verify_issued_identity(&domain.id).await?;
        self.persist_registry(&current, &next).await
    }

    /// Mint a bounded token for a preprovisioned deployment `ServiceAccount`,
    /// bound to a platform-owned Secret UID. Authentication of the requesting CI
    /// identity is performed by Kubernetes and the selected platform integration.
    /// # Errors
    /// Rejects identity mismatches and tokens with excessive returned validity.
    pub async fn request_deployment_credential(
        &self,
        service_account: &str,
        expected_sa_uid: &str,
        binding_secret: &str,
        expected_secret_uid: &str,
        audience: &str,
        validity_seconds: i64,
    ) -> Result<DeploymentCredential> {
        if !valid_name(service_account)
            || !valid_name(binding_secret)
            || expected_sa_uid.is_empty()
            || expected_secret_uid.is_empty()
            || audience.is_empty()
            || audience.len() > 256
            || !(600..=900).contains(&validity_seconds)
        {
            return Err(invalid("invalid short-lived deployment token request"));
        }
        let accounts: Api<ServiceAccount> =
            Api::namespaced(self.client.clone(), &self.control_namespace);
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let account = accounts.get(service_account).await?;
        let secret = secrets
            .get(binding_secret)
            .await
            .map_err(|_| invalid("deployment token binding lookup failed"))?;
        if account.metadata.uid.as_deref() != Some(expected_sa_uid)
            || account.metadata.deletion_timestamp.is_some()
            || secret.metadata.uid.as_deref() != Some(expected_secret_uid)
            || secret.metadata.deletion_timestamp.is_some()
        {
            return Err(invalid(
                "deployment credential binding UID differs from bootstrap policy",
            ));
        }
        let before = k8s_openapi::jiff::Timestamp::now().as_second();
        let request = TokenRequest {
            spec: Some(TokenRequestSpec {
                audiences: Some(vec![audience.into()]),
                expiration_seconds: Some(validity_seconds),
                bound_object_ref: Some(BoundObjectReference {
                    api_version: Some("v1".into()),
                    kind: Some("Secret".into()),
                    name: Some(binding_secret.into()),
                    uid: Some(expected_secret_uid.into()),
                }),
            }),
            ..TokenRequest::default()
        };
        let response = accounts
            .create_token_request(service_account, &PostParams::default(), &request)
            .await
            .map_err(|_| invalid("Kubernetes denied the deployment TokenRequest"))?;
        let status = response
            .status
            .ok_or_else(|| invalid("TokenRequest omitted status"))?;
        let expiration = status
            .expiration_timestamp
            .ok_or_else(|| invalid("TokenRequest omitted expiration"))?;
        let token = status
            .token
            .ok_or_else(|| invalid("TokenRequest omitted credential"))?;
        let expires = expiration.0.as_second();
        if token.is_empty() || expires <= before || expires > before + validity_seconds + 5 {
            return Err(invalid(
                "issued deployment token has invalid or excessive validity",
            ));
        }
        Ok(DeploymentCredential {
            token: token.into(),
            expires_at: expiration.0.to_string(),
            service_account_uid: expected_sa_uid.into(),
        })
    }
}

#[cfg(feature = "deployer")]
fn reserve_claims(next: &mut NamespaceRegistry, plan: &deploy::TransitionPlan) {
    let request = &plan.request;
    for reservation in &plan.reservations {
        let uid = next
            .claims
            .iter()
            .find(|claim| claim.identity == reservation.identity)
            .and_then(|claim| claim.uid.clone())
            .or_else(|| {
                request
                    .ownership_intents
                    .iter()
                    .find(|intent| intent.approval.identity == reservation.identity)
                    .map(|intent| intent.approval.observed_uid.clone())
            });
        next.claims
            .retain(|claim| claim.identity != reservation.identity);
        next.claims.push(ProtectedClaim {
            identity: reservation.identity.clone(),
            domain_id: request.next.compiled.ir.domain.id.clone(),
            generation_id: request.next.generation_id.clone(),
            transition_id: request.transition_id.clone(),
            state: ClaimState::Reserved,
            uid,
        });
    }
    next.claims.sort_by(|a, b| a.identity.cmp(&b.identity));
}
