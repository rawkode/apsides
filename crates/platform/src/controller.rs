use crate::store::decode_state;
use crate::tls_integrity::{TlsSecretExpectation, validate_tls_secret};
use crate::{
    ActivationState, ActivationStatus, ApprovedClaim, NamespaceRegistry, PlatformClient, Result,
    RuntimeBinding, RuntimeInventory, SCHEMA_VERSION, Versioned, invalid,
};
use compiler_core::{canonical_bytes, digest};
use deploy::{ClaimState, Event, advance};
use k8s_openapi::api::core::v1::Secret;
use kube::{
    Api,
    api::{ApiResource, DynamicObject, PostParams},
    core::GroupVersionKind,
};
use release::IntegrityVerifiedCandidate;
use serde_json::{Value, json};

pub(crate) struct StagingArtifact<'a> {
    pub compiled: &'a compiler_core::CompiledPlan,
    pub image: &'a str,
    pub release_digest: &'a str,
    pub provenance_digest: &'a str,
    pub trust_policy_digest: &'a str,
}

impl<'a> From<&'a IntegrityVerifiedCandidate> for StagingArtifact<'a> {
    fn from(release: &'a IntegrityVerifiedCandidate) -> Self {
        Self {
            compiled: release.compiled(),
            image: &release.manifest().image.reference,
            release_digest: release.release_digest(),
            provenance_digest: release.provenance_digest(),
            trust_policy_digest: release.trust_policy_digest(),
        }
    }
}

impl PlatformClient {
    pub(crate) async fn verify_controller_support(
        &self,
        stored: &crate::transition_session::ResolvedStoredTransition,
    ) -> Result<()> {
        let binding = stored
            .runtime_binding
            .as_ref()
            .ok_or_else(|| invalid("controller binding missing"))?;
        self.verify_recorded_support(
            stored,
            "ConfigMap",
            &format!("{}-config", binding.service_account_name),
        )
        .await?;
        if stored.proxy_identity.is_some() {
            self.verify_recorded_support(
                stored,
                "ServiceAccount",
                &format!("{}-process", binding.service_account_name),
            )
            .await?;
            self.verify_recorded_support(
                stored,
                "Service",
                &format!("apsides-proxy-{}", binding.generation_id),
            )
            .await?;
            let name = format!("{}-tls", binding.service_account_name);
            let api: Api<Secret> = Api::namespaced(self.client.clone(), &self.control_namespace);
            let secret = api
                .get(&name)
                .await
                .map_err(|_| invalid("generation TLS integrity lookup failed"))?;
            let key = format!("v1/Secret/{}/{name}", self.control_namespace);
            let uid = stored
                .staged_identity_uids
                .get(&key)
                .ok_or_else(|| invalid("generation TLS identity missing"))?;
            validate_tls_secret(
                &secret,
                &name,
                &self.control_namespace,
                TlsSecretExpectation::RecordedUid(uid),
            )?;
        }
        Ok(())
    }

    async fn verify_recorded_support(
        &self,
        stored: &crate::transition_session::ResolvedStoredTransition,
        kind: &str,
        name: &str,
    ) -> Result<()> {
        let ar = ApiResource::from_gvk(&GroupVersionKind::gvk("", "v1", kind));
        let api: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), &self.control_namespace, &ar);
        let object = api.get(name).await?;
        let key = format!("v1/{kind}/{}/{name}", self.control_namespace);
        if object.metadata.uid.is_none()
            || stored.staged_identity_uids.get(&key) != object.metadata.uid.as_ref()
            || object.metadata.deletion_timestamp.is_some()
            || stored.staged_content_digests.get(&key) != Some(&digest(&stable_data(&object))?)
        {
            return Err(invalid("staged controller support UID/content changed"));
        }
        Ok(())
    }
    pub(crate) async fn stage_initial_controller_resources(
        &self,
        registry: &mut Versioned<NamespaceRegistry>,
        stored: &crate::transition_session::ResolvedStoredTransition,
        binding: &RuntimeBinding,
        proxy_secret: &str,
    ) -> Result<()> {
        let transition_id = &stored.plan.request.transition_id;
        if stored.plan.request.previous.is_none() {
            let claims = approved_claims(&registry.value, stored)?;
            let key_id = self.preprovisioned_key_id(stored).await?;
            let activation = ActivationState {
                schema_version: SCHEMA_VERSION.into(),
                binding: binding.clone(),
                status: ActivationStatus::Inactive,
                approved_claims: claims,
                rollout_key_id: key_id,
            };
            crate::control_capacity::validate_activation(stored, &activation)?;
            let domain = &binding.domain_id;
            let activation_name = format!("apsides-activation-{domain}");
            let activation_object = self
                .stage_object(
                    registry,
                    transition_id,
                    "v1",
                    "ConfigMap",
                    &activation_name,
                    state_object(&activation_name, &self.control_namespace, &activation)?,
                )
                .await?;
            let activation: Versioned<ActivationState> = decode_state(&serde_json::from_value(
                serde_json::to_value(activation_object)?,
            )?)?;
            crate::control_capacity::validate_control_metadata(
                stored,
                &activation.uid,
                &activation.resource_version,
            )?;
            crate::control_capacity::validate_activation(stored, &activation.value)?;
            let inventory = RuntimeInventory {
                schema_version: SCHEMA_VERSION.into(),
                binding: binding.clone(),
                activation_uid: activation.uid,
                activation_resource_version: activation.resource_version,
                observations: vec![],
                conditions: vec![],
                drained: false,
            };
            crate::control_capacity::validate_inventory(stored, &inventory)?;
            let inventory_name = format!("apsides-inventory-{domain}");
            self.stage_object(
                registry,
                transition_id,
                "v1",
                "ConfigMap",
                &inventory_name,
                state_object(&inventory_name, &self.control_namespace, &inventory)?,
            )
            .await?;
        }
        let pull_secrets = self.controller_pull_secrets(binding).await?;
        let config_name = format!("{}-config", binding.service_account_name);
        let config = startup_config(binding, &self.control_namespace);
        self.stage_object(registry, transition_id,"v1","ConfigMap",&config_name,json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":config_name,"namespace":self.control_namespace},"immutable":true,"data":{"config.json":String::from_utf8(canonical_bytes(&config)?).map_err(|_|invalid("invalid config encoding"))?}})).await?;
        self.stage_object(
            registry,
            transition_id,
            "apps/v1",
            "Deployment",
            &binding.service_account_name,
            controller_deployment(
                binding,
                &self.control_namespace,
                &config_name,
                proxy_secret,
                &pull_secrets,
            ),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn record_staged_controller(
        &self,
        registry: &Versioned<NamespaceRegistry>,
        transition_id: &str,
        executor_id: &str,
        binding: RuntimeBinding,
        release: &StagingArtifact<'_>,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let selected = self.selected(registry, transition_id)?;
        let mut next = registry.value.clone();
        let entry = next
            .transitions
            .get_mut(transition_id)
            .ok_or_else(|| invalid("missing transition"))?;
        entry.runtime_binding = Some(binding);
        entry.staged_trust_policy_digest = Some(release.trust_policy_digest.into());
        entry.staged_provenance_digest = Some(release.provenance_digest.into());
        let event = Event::Staged {
            event_id: format!("stage-{}", entry.record.verification_epoch),
            generation_id: selected.plan.request.next.generation_id.clone(),
            service_account_name: selected
                .plan
                .generation_reservation
                .service_account_name
                .clone(),
            registry_cas_receipt_digest: digest(&(
                registry.uid.clone(),
                registry.resource_version.clone(),
                &entry.staged_identity_uids,
            ))?,
            verification_epoch: entry.record.verification_epoch,
        };
        entry.record = advance(
            &selected.plan,
            &entry.record,
            entry.record.revision,
            executor_id,
            event,
        )?;
        self.persist_registry(registry, &next).await
    }

    pub(crate) async fn controller_pull_secrets(
        &self,
        binding: &RuntimeBinding,
    ) -> Result<Vec<String>> {
        let policy = self.load_policy(&binding.domain_id).await?;
        if digest(&policy.value)? != binding.policy_digest {
            return Err(invalid(
                "platform policy changed before image pull configuration",
            ));
        }
        Ok(policy.value.image_pull_secrets)
    }

    pub(crate) async fn release_binding(
        &self,
        stored: &crate::transition_session::ResolvedStoredTransition,
        release: &StagingArtifact<'_>,
    ) -> Result<RuntimeBinding> {
        let next = &stored.plan.request.next;
        let domain = &next.compiled.ir.domain;
        let trust = self.load_trust_policy(&domain.id).await?;
        let policy = self.load_policy(&domain.id).await?;
        if release.compiled != &next.compiled
            || release.release_digest != next.artifact_digest
            || digest(&trust.value)? != release.trust_policy_digest
            || policy.value.runtime_ceiling != stored.plan.request.runtime_ceiling
            || policy.value.cleanup_ceiling != stored.plan.request.cleanup_ceiling
        {
            return Err(invalid(
                "verified release or independent policy changed before staging",
            ));
        }
        let binding = RuntimeBinding {
            domain_id: domain.id.clone(),
            cluster_binding: domain.cluster_binding.clone(),
            generation_id: next.generation_id.clone(),
            build_digest: next.artifact_digest.clone(),
            ir_digest: next.compiled.ir_digest.clone(),
            policy_digest: digest(&policy.value)?,
            capability_digest: digest(&next.compiled.capabilities)?,
            service_account_name: stored
                .plan
                .generation_reservation
                .service_account_name
                .clone(),
            controller_image: release.image.into(),
            max_handler_heap_bytes: next
                .compiled
                .ir
                .handlers
                .iter()
                .map(|handler| handler.max_heap_bytes)
                .max()
                .unwrap_or(0),
        };
        crate::runtime::validate_binding(&binding)?;
        Ok(binding)
    }

    pub(crate) async fn preprovisioned_key_id(
        &self,
        stored: &crate::transition_session::ResolvedStoredTransition,
    ) -> Result<Option<String>> {
        if !stored
            .plan
            .request
            .next
            .compiled
            .ir
            .inputs
            .iter()
            .any(|input| input.sensitive)
        {
            return Ok(None);
        }
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let secret = secrets
            .get(&format!(
                "apsides-rollout-key-{}",
                stored.plan.request.next.compiled.ir.domain.id
            ))
            .await
            .map_err(|_| invalid("platform rollout key unavailable"))?;
        let data = secret
            .data
            .ok_or_else(|| invalid("platform rollout key missing data"))?;
        let id = data
            .get("keyId")
            .and_then(|v| std::str::from_utf8(&v.0).ok())
            .filter(|id| crate::valid_name(id));
        if secret.metadata.deletion_timestamp.is_some()
            || secret.metadata.uid.is_none()
            || data.len() != 2
            || data.get("key").is_none_or(|v| v.0.len() != 32)
            || id.is_none()
        {
            return Err(invalid("platform rollout key invalid"));
        }
        Ok(id.map(str::to_owned))
    }

    pub(crate) async fn stage_object(
        &self,
        registry: &mut Versioned<NamespaceRegistry>,
        transition_id: &str,
        api_version: &str,
        kind: &str,
        name: &str,
        desired: Value,
    ) -> Result<DynamicObject> {
        if kind == "Secret" {
            return Err(invalid("Secrets require identity-only staging"));
        }
        let (group, version) = api_version.split_once('/').unwrap_or(("", api_version));
        let ar = ApiResource::from_gvk(&GroupVersionKind::gvk(group, version, kind));
        let api: Api<DynamicObject> =
            Api::namespaced_with(self.client.clone(), &self.control_namespace, &ar);
        let key = format!("{api_version}/{kind}/{}/{name}", self.control_namespace);
        let entry = self.selected(registry, transition_id)?;
        let mut desired: DynamicObject = serde_json::from_value(desired)?;
        desired.metadata.annotations = Some(
            [
                (String::from("apsides.dev/transition"), transition_id.into()),
                (
                    String::from("apsides.dev/desired-digest"),
                    digest(&desired.data)?,
                ),
            ]
            .into(),
        );
        let object = match (
            api.get_opt(name).await?,
            entry.staged_identity_uids.get(&key),
        ) {
            (None, None) => api.create(&PostParams::default(), &desired).await?,
            (Some(object), Some(uid))
                if object.metadata.uid.as_ref() == Some(uid)
                    && object.metadata.deletion_timestamp.is_none()
                    && stage_annotations_match(
                        kind,
                        object.metadata.annotations.as_ref(),
                        desired.metadata.annotations.as_ref(),
                    )
                    && entry.staged_content_digests.get(&key)
                        == Some(&digest(&stable_data(&object))?) =>
            {
                return Ok(object);
            }
            _ => {
                return Err(invalid(
                    "staged controller object differs from recorded UID/content; recovery required",
                ));
            }
        };
        let uid = object
            .metadata
            .uid
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("created object omitted UID"))?;
        let mut next = registry.value.clone();
        let entry = next
            .transitions
            .get_mut(transition_id)
            .ok_or_else(|| invalid("missing transition"))?;
        entry.staged_identity_uids.insert(key.clone(), uid);
        entry
            .staged_content_digests
            .insert(key, digest(&stable_data(&object))?);
        *registry = self.persist_registry(registry, &next).await?;
        Ok(object)
    }
}

pub(crate) fn stable_data(object: &DynamicObject) -> Value {
    let mut data = object.data.clone();
    if let Some(data) = data.as_object_mut() {
        data.remove("status");
    }
    data
}

fn stage_annotations_match(
    kind: &str,
    actual: Option<&std::collections::BTreeMap<String, String>>,
    expected: Option<&std::collections::BTreeMap<String, String>>,
) -> bool {
    let mut actual = actual.cloned();
    if kind == "Deployment"
        && let Some(revision) = actual
            .as_mut()
            .and_then(|annotations| annotations.remove("deployment.kubernetes.io/revision"))
        && !revision
            .parse::<u64>()
            .is_ok_and(|number| number > 0 && number.to_string() == revision)
    {
        return false;
    }
    actual.as_ref() == expected
}

pub(crate) fn approved_claims(
    registry: &NamespaceRegistry,
    stored: &crate::transition_session::ResolvedStoredTransition,
) -> Result<Vec<ApprovedClaim>> {
    stored
        .plan
        .request
        .next
        .compiled
        .ir
        .resources
        .iter()
        .map(|resource| {
            let identity = crate::runtime::resource_identity(resource)?;
            let claim = registry
                .claims
                .iter()
                .find(|claim| claim.identity == identity)
                .ok_or_else(|| invalid("reserved claim missing"))?;
            if claim.domain_id != stored.plan.request.next.compiled.ir.domain.id
                || claim.generation_id != stored.plan.request.next.generation_id
                || claim.transition_id != stored.plan.request.transition_id
                || !matches!(claim.state, ClaimState::Reserved | ClaimState::Active)
            {
                return Err(invalid("reserved claim changed"));
            }
            let mut claim = claim.clone();
            claim.state = ClaimState::Active;
            Ok(ApprovedClaim {
                claim,
                deletion_policy: resource.lifecycle.deletion_policy,
                allow_create: true,
            })
        })
        .collect()
}

pub(crate) fn state_object(
    name: &str,
    namespace: &str,
    state: &impl serde::Serialize,
) -> Result<Value> {
    let encoded = canonical_bytes(state)?;
    if encoded.len() > crate::MAX_STATE_BYTES {
        return Err(invalid(
            "initial control state exceeds 512 KiB; split or reduce the domain before staging",
        ));
    }
    Ok(
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":name,"namespace":namespace},"data":{"state.json":String::from_utf8(encoded).map_err(|_|invalid("invalid state encoding"))?}}),
    )
}

fn startup_config(binding: &RuntimeBinding, control_namespace: &str) -> Value {
    json!({"mode":"production","apiAccess":"proxy","controlNamespace":control_namespace,"binding":binding,"pollIntervalMillis":1_000,"baseBackoffMillis":500,"maxBackoffMillis":30_000,"minMutationIntervalMillis":100,"diagnosticsBind":"0.0.0.0:8080"})
}

/// The controller pod's memory limit, as a whole `"<N>Mi"` quantity: a fixed
/// 256Mi reconciler/runtime baseline, plus the largest `maxHeapBytes` any one
/// declared handler needs (0 for a domain with no handlers), plus a fixed
/// 32Mi allowance for the V8 platform/isolate bookkeeping and the bundle(s)
/// embedded alongside it. `max_handler_heap_bytes` is a *ceiling* (handlers
/// run one at a time, one isolate per invocation -- see the Apsides v0.3
/// plan, Phase 5b), not a sum over every declared handler, so this scales
/// with the domain's single largest handler, never with how many it has.
fn controller_memory_limit_mi(binding: &RuntimeBinding) -> String {
    const BASELINE_MI: u64 = 256;
    const ENGINE_OVERHEAD_MI: u64 = 32;
    const BYTES_PER_MI: u64 = 1024 * 1024;
    let handler_mi = binding.max_handler_heap_bytes.div_ceil(BYTES_PER_MI);
    format!("{}Mi", BASELINE_MI + handler_mi + ENGINE_OVERHEAD_MI)
}

fn controller_deployment(
    binding: &RuntimeBinding,
    namespace: &str,
    config_name: &str,
    proxy_secret: &str,
    pull_secrets: &[String],
) -> Value {
    let labels = json!({"apsides.dev/generation":binding.generation_id});
    let mounts = [
        json!({"name":"config","mountPath":"/etc/apsides","readOnly":true}),
        json!({"name":"proxy-client","mountPath":"/etc/apsides/proxy","readOnly":true}),
    ];
    let volumes = [
        json!({"name":"config","configMap":{"name":config_name}}),
        json!({"name":"proxy-client","secret":{"secretName":proxy_secret,"defaultMode":420,"items":[{"key":"ca.crt","path":"ca.crt"},{"key":"client.crt","path":"tls.crt"},{"key":"client.key","path":"tls.key"}]}}),
    ];
    let account = format!("{}-process", binding.service_account_name);
    let memory_limit = controller_memory_limit_mi(binding);
    let mut deployment = json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":binding.service_account_name,"namespace":namespace},"spec":{"replicas":0,"selector":{"matchLabels":labels},"strategy":{"type":"Recreate"},"template":{"metadata":{"labels":labels,"finalizers":[crate::TERMINATION_FINALIZER]},"spec":{"serviceAccountName":account,"automountServiceAccountToken":false,"tolerations":[{"key":"node.kubernetes.io/not-ready","operator":"Exists","effect":"NoExecute"},{"key":"node.kubernetes.io/unreachable","operator":"Exists","effect":"NoExecute"}],"securityContext":{"runAsNonRoot":true,"runAsUser":65532,"seccompProfile":{"type":"RuntimeDefault"}},"containers":[{"name":"controller","image":binding.controller_image,"imagePullPolicy":"IfNotPresent","env":[{"name":"APSIDES_RUNTIME_CONFIG_PATH","value":"/etc/apsides/config.json"}],"securityContext":{"allowPrivilegeEscalation":false,"readOnlyRootFilesystem":true,"capabilities":{"drop":["ALL"]}},"volumeMounts":mounts,"ports":[{"containerPort":8080,"name":"diagnostics"}],"resources":{"requests":{"cpu":"10m","memory":"32Mi"},"limits":{"cpu":"1","memory":memory_limit}},"livenessProbe":{"httpGet":{"path":"/livez","port":"diagnostics"}},"readinessProbe":{"httpGet":{"path":"/readyz","port":"diagnostics"}}}],"volumes":volumes}}}});
    set_image_pull_secrets(&mut deployment, pull_secrets);
    deployment
}

pub(crate) fn set_image_pull_secrets(deployment: &mut Value, names: &[String]) {
    if !names.is_empty() {
        deployment["spec"]["template"]["spec"]["imagePullSecrets"] = json!(
            names
                .iter()
                .map(|name| json!({"name":name}))
                .collect::<Vec<_>>()
        );
    }
}

#[cfg(test)]
mod stage_metadata_tests {
    use super::stage_annotations_match;
    use std::collections::BTreeMap;

    #[test]
    fn initial_state_budget_matches_the_reader_at_the_encoded_boundary()
    -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let state = "x".repeat(crate::MAX_STATE_BYTES - 2);
        let object = super::state_object("activation", "control", &state)?;
        let mut object: k8s_openapi::api::core::v1::ConfigMap = serde_json::from_value(object)?;
        object.metadata.uid = Some("control-uid".into());
        object.metadata.resource_version = Some("42".into());
        assert_eq!(crate::store::decode_state::<String>(&object)?.value, state);
        assert!(super::state_object("activation", "control", &format!("{state}x")).is_err());
        Ok(())
    }

    #[test]
    fn initial_state_budget_counts_escaped_bytes_without_printing_values() {
        let state = "\u{0001}".repeat(crate::MAX_STATE_BYTES / 6 + 1);
        assert!(state.len() < crate::MAX_STATE_BYTES);
        let error = super::state_object("activation", "control", &state)
            .err()
            .map(|error| error.to_string());
        assert_eq!(
            error.as_deref(),
            Some(
                "platform state rejected: initial control state exceeds 512 KiB; split or reduce the domain before staging"
            )
        );
    }

    #[test]
    fn live_v4_deployment_revision_is_not_staged_content_drift() {
        // Exact public staging annotations observed in the v4 disposable run.
        // Its UID and stable-data digest independently matched the registry.
        let expected: BTreeMap<String, String> = [
            ("apsides.dev/transition".into(), "install".into()),
            (
                "apsides.dev/desired-digest".into(),
                "sha256:ac6ef312cae19d9fb9323ad6331033d143b1bb735455ffb66e2a913c48762bca".into(),
            ),
        ]
        .into();
        let mut actual = expected.clone();
        actual.insert("deployment.kubernetes.io/revision".into(), "1".into());
        assert!(stage_annotations_match(
            "Deployment",
            Some(&actual),
            Some(&expected)
        ));
        assert!(!stage_annotations_match(
            "ConfigMap",
            Some(&actual),
            Some(&expected)
        ));
        for revision in ["0", "-1", "01", "1.0", "invalid", "18446744073709551616"] {
            actual.insert("deployment.kubernetes.io/revision".into(), revision.into());
            assert!(!stage_annotations_match(
                "Deployment",
                Some(&actual),
                Some(&expected)
            ));
        }
        actual.insert("deployment.kubernetes.io/revision".into(), "2".into());
        actual.insert("apsides.dev/transition".into(), "other".into());
        assert!(!stage_annotations_match(
            "Deployment",
            Some(&actual),
            Some(&expected)
        ));
        actual = expected.clone();
        actual.insert("unapproved.example/annotation".into(), "value".into());
        assert!(!stage_annotations_match(
            "Deployment",
            Some(&actual),
            Some(&expected)
        ));
    }
}

#[cfg(test)]
mod image_pull_tests {
    use super::*;
    #[test]
    fn image_pull_references_are_only_kubelet_configuration() {
        let binding = RuntimeBinding {
            domain_id: "domain".into(),
            cluster_binding: "cluster".into(),
            generation_id: "generation".into(),
            build_digest: "build".into(),
            ir_digest: "ir".into(),
            policy_digest: "policy".into(),
            capability_digest: "capability".into(),
            service_account_name: "runtime".into(),
            controller_image: "registry/controller@sha256:digest".into(),
            max_handler_heap_bytes: 0,
        };
        let names = vec!["registry-auth".to_string()];
        let populated = controller_deployment(&binding, "control", "config", "tls", &names);
        assert_eq!(
            populated["spec"]["template"]["spec"]["imagePullSecrets"],
            json!([{"name":"registry-auth"}])
        );
        let names = vec![];
        let empty = controller_deployment(&binding, "control", "config", "tls", &names);
        let mut expected = populated;
        if let Some(spec) = expected["spec"]["template"]["spec"].as_object_mut() {
            spec.remove("imagePullSecrets");
        }
        assert_eq!(expected, empty);
    }
}

pub(crate) fn verify_image_pull_secrets(
    actual: &k8s_openapi::api::core::v1::PodSpec,
    expected: &k8s_openapi::api::core::v1::PodSpec,
) -> Result<()> {
    if actual.image_pull_secrets.as_deref().map_or(&[][..], |v| v)
        != expected
            .image_pull_secrets
            .as_deref()
            .map_or(&[][..], |v| v)
    {
        return Err(invalid(
            "Pod image pull Secret references differ from protected Deployment",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod image_pull_integrity_tests {
    #[test]
    fn image_pull_secret_admission_mutation_is_rejected() -> crate::Result<()> {
        let expected: k8s_openapi::api::core::v1::PodSpec = serde_json::from_value(
            serde_json::json!({"containers":[],"imagePullSecrets":[{"name":"registry"}]}),
        )?;
        let mut actual = expected.clone();
        super::verify_image_pull_secrets(&actual, &expected)?;
        actual.image_pull_secrets = None;
        assert!(super::verify_image_pull_secrets(&actual, &expected).is_err());
        let empty = actual.clone();
        actual.image_pull_secrets = Some(vec![]);
        super::verify_image_pull_secrets(&actual, &empty)?;
        assert!(super::verify_image_pull_secrets(&expected, &empty).is_err());
        Ok(())
    }
}
