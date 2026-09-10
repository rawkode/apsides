use crate::store::read_state;
use crate::tls_integrity::{TlsSecretExpectation, validate_tls_secret};
use crate::{
    ADMIN_IMAGE, ENVOY_IMAGE, MAX_PROXY_LEDGER_BYTES, NamespaceRegistry, PlatformClient,
    ProxyIdentity, Result, RuntimeBinding, SCHEMA_VERSION, Versioned, envoy_configuration,
    envoy_token_sds, invalid, valid_digest,
};
use compiler_core::{canonical_bytes, digest};
use deploy::Phase;
use k8s_openapi::api::core::v1::{Pod, Secret};
use kube::{
    Api,
    api::{ListParams, PostParams},
};
use release::IntegrityVerifiedCandidate;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProxyPolicy {
    pub schema_version: String,
    pub domain_id: String,
    pub cluster_binding: String,
    pub envoy_image: String,
    pub admin_image: String,
    pub configuration_template_digest: String,
    pub environment_qualification_digest: String,
    /// Identifies the independently qualified node image in platform bootstrap.
    /// A Node API response alone does not prove this image identity.
    pub node_image_digest: String,
    pub certificate_lifetime_seconds: i64,
    pub max_ledger_bytes: usize,
}

impl PlatformClient {
    /// Stage a native initial controller with isolated authenticated Envoy
    /// transport. Kubernetes credentials are mounted only in the separate proxy
    /// Pod; client TLS material is issued automatically per generation.
    /// # Errors
    /// Rejects unqualified policy, reused or unknown objects, and non-ready proxy
    /// epochs. Retry with the same executor while the reserved proxy becomes ready.
    pub async fn stage_proxy_controller(
        &self,
        transition_id: &str,
        executor_id: &str,
        release: &IntegrityVerifiedCandidate,
    ) -> Result<Versioned<NamespaceRegistry>> {
        self.stage_proxy_artifact(
            transition_id,
            executor_id,
            &crate::controller::StagingArtifact::from(release),
        )
        .await
    }

    pub(crate) async fn stage_proxy_artifact(
        &self,
        transition_id: &str,
        executor_id: &str,
        release: &crate::controller::StagingArtifact<'_>,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let reserved = self.load_registry().await?;
        let stored = self.selected(&reserved, transition_id)?;
        if stored.record.phase != Phase::Planned || stored.record.executor_id != executor_id {
            return Err(invalid(
                "proxy staging requires an owning Planned transition",
            ));
        }
        deploy::require_event_capacity(&stored.plan, &stored.record)?;
        let binding = self.release_binding(&stored, release).await?;
        let policy = self.load_proxy_policy(&binding.domain_id).await?;
        let pull_secrets = self.controller_pull_secrets(&binding).await?;
        let mut registry = self
            .stage_runtime_identity(transition_id, executor_id)
            .await?;
        let tls_name = format!("{}-tls", binding.service_account_name);
        self.stage_generation_tls(
            &mut registry,
            transition_id,
            &binding,
            &tls_name,
            policy.value.certificate_lifetime_seconds,
        )
        .await?;
        let process_name = format!("{}-process", binding.service_account_name);
        self.stage_object(&mut registry,transition_id,"v1","ServiceAccount",&process_name,json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":process_name,"namespace":self.control_namespace},"automountServiceAccountToken":false})).await?;
        let proxy_name = format!("apsides-proxy-{}", binding.generation_id);
        let config_name = format!("{proxy_name}-config");
        let data = json!({"envoy.json":String::from_utf8(canonical_bytes(&envoy_configuration(&binding.generation_id)?)?).map_err(|_|invalid("proxy config encoding"))?,"token-sds.json":String::from_utf8(canonical_bytes(&envoy_token_sds())?).map_err(|_|invalid("SDS config encoding"))?});
        self.stage_object(&mut registry,transition_id,"v1","ConfigMap",&config_name,json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":config_name,"namespace":self.control_namespace},"immutable":true,"data":data})).await?;
        self.stage_object(
            &mut registry,
            transition_id,
            "apps/v1",
            "Deployment",
            &proxy_name,
            proxy_deployment(
                &binding,
                &self.control_namespace,
                &config_name,
                &tls_name,
                &pull_secrets,
            ),
        )
        .await?;
        self.stage_object(&mut registry,transition_id,"v1","Service",&proxy_name,json!({"apiVersion":"v1","kind":"Service","metadata":{"name":proxy_name,"namespace":self.control_namespace},"spec":{"selector":{"apsides.dev/proxy-generation":binding.generation_id},"ports":[{"name":"https","port":8443,"targetPort":8443}]}})).await?;
        let identity = self
            .observe_staged_proxy(
                &registry,
                transition_id,
                &binding,
                &policy.value,
                &config_name,
                &data,
            )
            .await?;
        self.stage_initial_controller_resources(&mut registry, &stored, &binding, Some(&tls_name))
            .await?;
        let mut next = registry.value.clone();
        let entry = next
            .transitions
            .get_mut(transition_id)
            .ok_or_else(|| invalid("missing transition"))?;
        entry.proxy_identity = Some(identity);
        entry.request_drain_qualification = Some(crate::RequestDrainQualification::IsolatedEnvoy {
            qualification_digest: policy.value.environment_qualification_digest.clone(),
            node_image_digest: policy.value.node_image_digest.clone(),
        });
        registry = self.persist_registry(&registry, &next).await?;
        self.record_staged_controller(&registry, transition_id, executor_id, binding, release)
            .await
    }

    /// Read an independent platform admission of the fixed Envoy profile.
    /// # Errors
    /// Rejects mismatched image, profile, environment or boundedness settings.
    pub async fn load_proxy_policy(&self, domain: &str) -> Result<Versioned<ProxyPolicy>> {
        let state: Versioned<ProxyPolicy> = read_state(
            &self.client,
            &self.control_namespace,
            &format!("apsides-proxy-policy-{domain}"),
        )
        .await?;
        let policy = &state.value;
        if policy.schema_version != SCHEMA_VERSION
            || policy.domain_id != domain
            || policy.cluster_binding != self.cluster_binding
            || policy.envoy_image != ENVOY_IMAGE
            || policy.admin_image != ADMIN_IMAGE
            || policy.configuration_template_digest
                != digest(&envoy_configuration("qualified-template")?)?
            || !valid_digest(&policy.environment_qualification_digest)
            || !valid_digest(&policy.node_image_digest)
            || policy.max_ledger_bytes != MAX_PROXY_LEDGER_BYTES
            || !(86_400..=31_536_000).contains(&policy.certificate_lifetime_seconds)
        {
            return Err(invalid(
                "proxy profile lacks a matching protected platform qualification",
            ));
        }
        Ok(state)
    }

    async fn stage_generation_tls(
        &self,
        registry: &mut Versioned<NamespaceRegistry>,
        transition: &str,
        binding: &RuntimeBinding,
        name: &str,
        lifetime: i64,
    ) -> Result<()> {
        let api: Api<Secret> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let key = format!("v1/Secret/{}/{name}", self.control_namespace);
        let entry = self.selected(registry, transition)?;
        let known = entry.staged_identity_uids.get(&key);
        let uid = match (
            api.get_opt(name)
                .await
                .map_err(|_| invalid("generation TLS lookup failed"))?,
            known,
        ) {
            (Some(secret), Some(uid)) => {
                validate_tls_secret(
                    &secret,
                    name,
                    &self.control_namespace,
                    TlsSecretExpectation::RecordedUid(uid),
                )?;
                return Ok(());
            }
            (None, None) => {
                let expected = crate::tls::generation_tls_secret(
                    name,
                    &self.control_namespace,
                    &binding.generation_id,
                    lifetime,
                )?;
                let actual = api
                    .create(&PostParams::default(), &expected)
                    .await
                    .map_err(|_| {
                        invalid("generation TLS creation failed; outcome requires inspection")
                    })?;
                validate_tls_secret(
                    &actual,
                    name,
                    &self.control_namespace,
                    TlsSecretExpectation::Created(&expected),
                )?
            }
            _ => {
                return Err(invalid(
                    "generation TLS object lacks matching recorded identity",
                ));
            }
        };
        let mut next = registry.value.clone();
        let entry = next
            .transitions
            .get_mut(transition)
            .ok_or_else(|| invalid("missing transition"))?;
        entry.staged_identity_uids.insert(key, uid);
        *registry = self.persist_registry(registry, &next).await?;
        Ok(())
    }

    async fn observe_staged_proxy(
        &self,
        registry: &Versioned<NamespaceRegistry>,
        transition: &str,
        binding: &RuntimeBinding,
        policy: &ProxyPolicy,
        config_name: &str,
        data: &Value,
    ) -> Result<ProxyIdentity> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let listed = pods
            .list(&ListParams::default().labels(&format!(
                "apsides.dev/proxy-generation={}",
                binding.generation_id
            )))
            .await?;
        if listed.items.is_empty() {
            return Err(crate::pending("proxy Pod has not been created yet"));
        }
        if listed.items.len() != 1
            || listed
                .metadata
                .continue_
                .as_ref()
                .is_some_and(|s| !s.is_empty())
        {
            return Err(invalid(
                "proxy is not yet one recorded ready Pod; retry owning transition",
            ));
        }
        let [pod] = listed.items.as_slice() else {
            return Err(invalid("proxy requires one recorded Pod"));
        };
        let status = pod
            .status
            .as_ref()
            .ok_or_else(|| crate::pending("proxy status pending"))?;
        if pod.metadata.deletion_timestamp.is_some() {
            return Err(invalid("proxy Pod is deleting"));
        }
        if status.phase.as_deref() != Some("Running") {
            return Err(crate::pending("proxy Pod is not ready"));
        }
        let (deployment_uid, node_name, node_uid) = self
            .verify_proxy_origin(registry, transition, binding, pod)
            .await?;
        let id = |name: &str| -> Result<String> {
            let status = status
                .container_statuses
                .as_ref()
                .and_then(|v| v.iter().find(|c| c.name == name))
                .ok_or_else(|| crate::pending("proxy container status pending"))?;
            if status.restart_count != 0 {
                return Err(invalid("proxy restarted before staging"));
            }
            if !status.ready
                || status
                    .state
                    .as_ref()
                    .and_then(|s| s.running.as_ref())
                    .is_none()
            {
                return Err(crate::pending("proxy container is not ready"));
            }
            status
                .container_id
                .clone()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| invalid("proxy container ID missing"))
        };
        let config_key = format!("v1/ConfigMap/{}/{config_name}", self.control_namespace);
        let identity = ProxyIdentity {
            policy_digest: digest(policy)?,
            deployment_name: format!("apsides-proxy-{}", binding.generation_id),
            deployment_uid,
            node_name,
            node_uid,
            pod_name: pod
                .metadata
                .name
                .clone()
                .ok_or_else(|| invalid("proxy Pod name missing"))?,
            pod_uid: pod
                .metadata
                .uid
                .clone()
                .ok_or_else(|| invalid("proxy Pod UID missing"))?,
            pod_spec_digest: digest(&pod.spec)?,
            envoy_container_id: id("envoy")?,
            admin_container_id: id("admin")?,
            config_map_name: config_name.into(),
            config_map_uid: registry
                .value
                .transition(transition)?
                .staged_identity_uids
                .get(&config_key)
                .cloned()
                .ok_or_else(|| invalid("proxy config UID not recorded"))?,
            config_digest: digest(&Some(data))?,
            qualification_digest: policy.environment_qualification_digest.clone(),
        };
        self.inspect_proxy(&identity).await?;
        Ok(identity)
    }
}

fn proxy_deployment(
    binding: &RuntimeBinding,
    namespace: &str,
    config: &str,
    tls: &str,
    pull_secrets: &[String],
) -> Value {
    let name = format!("apsides-proxy-{}", binding.generation_id);
    let labels = json!({"apsides.dev/proxy-generation":binding.generation_id});
    let security = json!({"allowPrivilegeEscalation":false,"readOnlyRootFilesystem":true,"capabilities":{"drop":["ALL"]}});
    let mut deployment = json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":name,"namespace":namespace},"spec":{"replicas":1,"strategy":{"type":"Recreate"},"selector":{"matchLabels":labels},"template":{"metadata":{"labels":labels,"finalizers":[crate::TERMINATION_FINALIZER]},"spec":{"serviceAccountName":binding.service_account_name,"automountServiceAccountToken":false,"securityContext":{"runAsNonRoot":true,"runAsUser":65532,"runAsGroup":65532,"fsGroup":65532,"seccompProfile":{"type":"RuntimeDefault"}},"tolerations":[{"key":"node.kubernetes.io/not-ready","operator":"Exists","effect":"NoExecute"},{"key":"node.kubernetes.io/unreachable","operator":"Exists","effect":"NoExecute"}],"containers":[{"name":"envoy","image":ENVOY_IMAGE,"args":["-c","/etc/apsides/envoy/envoy.json","--concurrency","2","--log-level","warning","--file-flush-interval-msec","50"],"securityContext":security,"ports":[{"name":"https","containerPort":8443}],"volumeMounts":[{"name":"config","mountPath":"/etc/apsides/envoy","readOnly":true},{"name":"server-tls","mountPath":"/etc/apsides/server-tls","readOnly":true},{"name":"api-token","mountPath":"/var/run/secrets/kubernetes.io/serviceaccount","readOnly":true},{"name":"evidence","mountPath":"/evidence"}],"resources":{"requests":{"cpu":"25m","memory":"64Mi","ephemeral-storage":"32Mi"},"limits":{"cpu":"1","memory":"256Mi","ephemeral-storage":"64Mi"}},"readinessProbe":{"tcpSocket":{"port":8443}}},{"name":"admin","image":ADMIN_IMAGE,"command":["sleep"],"args":["infinity"],"securityContext":security,"volumeMounts":[{"name":"evidence","mountPath":"/evidence"}],"resources":{"requests":{"cpu":"5m","memory":"8Mi"},"limits":{"cpu":"100m","memory":"32Mi"}}}],"volumes":[{"name":"config","configMap":{"name":config}},{"name":"server-tls","secret":{"secretName":tls,"defaultMode":416,"items":[{"key":"ca.crt","path":"ca.crt"},{"key":"server.crt","path":"tls.crt"},{"key":"server.key","path":"tls.key"}]}},{"name":"api-token","projected":{"defaultMode":416,"sources":[{"serviceAccountToken":{"path":"token","expirationSeconds":3600}},{"configMap":{"name":"kube-root-ca.crt","items":[{"key":"ca.crt","path":"ca.crt"}]}}]}},{"name":"evidence","emptyDir":{"sizeLimit":"32Mi"}}]}}}});
    crate::controller::set_image_pull_secrets(&mut deployment, pull_secrets);
    deployment
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
        };
        let names = vec!["registry-auth".to_string()];
        let populated = proxy_deployment(&binding, "control", "config", "tls", &names);
        assert_eq!(
            populated["spec"]["template"]["spec"]["imagePullSecrets"],
            json!([{"name":"registry-auth"}])
        );
        let names = vec![];
        let empty = proxy_deployment(&binding, "control", "config", "tls", &names);
        let mut expected = populated;
        if let Some(spec) = expected["spec"]["template"]["spec"].as_object_mut() {
            spec.remove("imagePullSecrets");
        }
        assert_eq!(expected, empty);
    }
}
