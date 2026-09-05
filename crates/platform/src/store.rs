use crate::{
    DeploymentCredential, MAX_STATE_BYTES, NamespaceRegistry, PolicyState, Result, SCHEMA_VERSION,
    StoredTransition, Versioned, invalid, valid_name,
};
use compiler_core::{canonical_bytes, parse_strict};
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
    let encoded = canonical_bytes(next)?;
    if encoded.len() > MAX_STATE_BYTES {
        return Err(invalid(
            "control state exceeds 512 KiB; split or reduce the domain",
        ));
    }
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    let mut object = api.get(name).await?;
    if object.metadata.uid.as_deref() != Some(&previous.uid)
        || object.metadata.resource_version.as_deref() != Some(&previous.resource_version)
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
    if updated.metadata.uid.as_deref() != Some(&previous.uid) {
        return Err(invalid("replacement changed control-object UID"));
    }
    decode_state(&updated)
}

#[derive(Clone)]
pub struct PlatformClient {
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
        if !valid_name(control_namespace)
            || !valid_name(workload_namespace)
            || control_namespace == workload_namespace
            || cluster_binding.is_empty()
            || cluster_binding.len() > 256
        {
            return Err(invalid(
                "platform scope must have distinct fixed control and workload namespaces",
            ));
        }
        Ok(Self {
            client,
            control_namespace: control_namespace.into(),
            workload_namespace: workload_namespace.into(),
            cluster_binding: cluster_binding.into(),
        })
    }

    /// # Errors
    /// Rejects unavailable, corrupt, oversized or differently scoped registry.
    pub async fn load_registry(&self) -> Result<Versioned<NamespaceRegistry>> {
        let registry: Versioned<NamespaceRegistry> = read_state(
            &self.client,
            &self.control_namespace,
            &format!("reakt-registry-{}", self.workload_namespace),
        )
        .await?;
        if registry.value.schema_version != SCHEMA_VERSION
            || registry.value.cluster_binding != self.cluster_binding
            || registry.value.workload_namespace != self.workload_namespace
        {
            return Err(invalid("protected registry scope mismatch"));
        }
        Ok(registry)
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

    /// Atomically reserve claims, permanent generation history and transition
    /// ownership in one platform-provisioned namespace registry `ConfigMap`.
    /// # Errors
    /// Rejects stale plans, concurrent deployers, claim overlap and identity reuse.
    pub async fn reserve_transition(
        &self,
        request: &DeploymentRequest,
        executor_id: &str,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let current = self.load_registry().await?;
        let domain = &request.next.compiled.ir.domain;
        let mut history = current
            .value
            .histories
            .get(&domain.id)
            .cloned()
            .ok_or_else(|| {
                invalid("platform must provision the complete domain generation history")
            })?;
        history.revision.clone_from(&current.resource_version);
        let expected = RegistrySnapshot {
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
        let policy = self.load_policy(&domain.id).await?;
        if policy.value.runtime_ceiling != request.runtime_ceiling
            || policy.value.cleanup_ceiling != request.cleanup_ceiling
        {
            return Err(invalid(
                "request ceiling differs from independent platform policy",
            ));
        }
        let plan = plan_transition(request)?;
        self.verify_ownership_intents(request).await?;
        let record = begin(&plan, executor_id)?;
        let mut next = current.value.clone();
        if next.transitions.values().any(|entry| {
            entry.plan.request.next.compiled.ir.domain.id == domain.id
                && entry.record.phase != deploy::Phase::Committed
        }) {
            return Err(invalid(
                "domain has an unfinished transition; recover it without takeover",
            ));
        }
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
                domain_id: domain.id.clone(),
                generation_id: request.next.generation_id.clone(),
                transition_id: request.transition_id.clone(),
                state: ClaimState::Reserved,
                uid,
            });
        }
        next.claims.sort_by(|a, b| a.identity.cmp(&b.identity));
        next.histories
            .get_mut(&domain.id)
            .ok_or_else(|| invalid("missing domain history"))?
            .allocations
            .push(plan.generation_reservation.clone());
        next.transitions.insert(
            request.transition_id.clone(),
            StoredTransition::reserved(plan, record),
        );
        replace_state(
            &self.client,
            &self.control_namespace,
            &format!("reakt-registry-{}", self.workload_namespace),
            &current,
            &next,
        )
        .await
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
