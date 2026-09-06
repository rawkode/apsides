//! Already-staged isolated writer evidence for activation and resume wire tests.
use super::LifecycleRegistry;
use crate::{
    ADMIN_IMAGE, ENVOY_IMAGE, MAX_PROXY_LEDGER_BYTES, ProxyIdentity, ProxyPolicy,
    RequestDrainQualification, RuntimeBinding, RuntimeInventory, SCHEMA_VERSION, StoredTransition,
    envoy_configuration, envoy_token_sds,
};
use compiler_core::{canonical_bytes, digest};
use k8s_openapi::api::core::v1::{ConfigMap, Pod};
use kube::core::DynamicObject;
use serde_json::{Value, json};
use std::error::Error;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub fn qualify(registry: &mut LifecycleRegistry, transition: &str) -> Result<Vec<Value>> {
    let control = registry
        .plan(transition)?
        .request
        .next
        .compiled
        .ir
        .domain
        .control_namespace
        .clone();
    let stored = registry
        .transitions
        .get_mut(transition)
        .ok_or("transition missing")?;
    let binding = stored
        .runtime_binding
        .clone()
        .ok_or("runtime binding missing")?;
    let name = format!("reakt-proxy-{}", binding.generation_id);
    let policy = policy(&binding)?;
    let config = configuration(&binding, &control)?;
    let pod = proxy_pod(&binding, &control)?;
    let mut objects = support_objects(stored, &binding, &control)?;
    let deployment = json!({
        "apiVersion":"apps/v1","kind":"Deployment",
        "metadata":{"name":name,"namespace":control,"uid":"proxy-deployment-uid","resourceVersion":"40"},
        "spec":{"replicas":1,"selector":{"matchLabels":{"reakt.dev/proxy-generation":binding.generation_id}},
            "strategy":{"type":"Recreate"},"template":{"metadata":{"labels":{"reakt.dev/proxy-generation":binding.generation_id}},"spec":pod.spec}}
    });
    objects.extend([
        serde_json::to_value(&config)?, deployment, serde_json::to_value(&pod)?,
        json!({"apiVersion":"v1","kind":"Node","metadata":{"name":"worker","uid":"worker-uid","resourceVersion":"40"},"status":{"conditions":[{"type":"Ready","status":"True"}]}}),
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":format!("reakt-proxy-policy-{}",binding.domain_id),"namespace":control,"uid":"proxy-policy-uid","resourceVersion":"40"},"data":{"state.json":serde_json::to_string(&policy)?}}),
    ]);
    stored.request_drain_qualification = Some(RequestDrainQualification::IsolatedEnvoy {
        qualification_digest: policy.environment_qualification_digest.clone(),
        node_image_digest: policy.node_image_digest.clone(),
    });
    stored.proxy_identity = Some(ProxyIdentity {
        policy_digest: digest(&policy)?,
        deployment_name: name.clone(),
        deployment_uid: "proxy-deployment-uid".into(),
        node_name: "worker".into(),
        node_uid: "worker-uid".into(),
        pod_name: format!("{name}-pod"),
        pod_uid: "proxy-pod-uid".into(),
        pod_spec_digest: digest(&pod.spec)?,
        envoy_container_id: "containerd://envoy".into(),
        admin_container_id: "containerd://admin".into(),
        config_map_name: format!("{name}-config"),
        config_map_uid: "proxy-config-uid".into(),
        config_digest: digest(&config.data)?,
        qualification_digest: policy.environment_qualification_digest,
    });
    for object in &objects {
        record_support(stored, object)?;
    }
    registry.admit(transition)?;
    Ok(objects)
}

fn policy(binding: &RuntimeBinding) -> Result<ProxyPolicy> {
    Ok(ProxyPolicy {
        schema_version: SCHEMA_VERSION.into(),
        domain_id: binding.domain_id.clone(),
        cluster_binding: binding.cluster_binding.clone(),
        envoy_image: ENVOY_IMAGE.into(),
        admin_image: ADMIN_IMAGE.into(),
        configuration_template_digest: digest(&envoy_configuration("qualified-template")?)?,
        environment_qualification_digest: digest(&"qualified-platform-environment")?,
        node_image_digest: digest(&"qualified-node-image")?,
        certificate_lifetime_seconds: 86_400,
        max_ledger_bytes: MAX_PROXY_LEDGER_BYTES,
    })
}

fn configuration(binding: &RuntimeBinding, control: &str) -> Result<ConfigMap> {
    Ok(serde_json::from_value(json!({
        "apiVersion":"v1","kind":"ConfigMap","immutable":true,
        "metadata":{"name":format!("reakt-proxy-{}-config",binding.generation_id),"namespace":control,"uid":"proxy-config-uid","resourceVersion":"40"},
        "data":{"envoy.json":String::from_utf8(canonical_bytes(&envoy_configuration(&binding.generation_id)?)?)?,
            "token-sds.json":String::from_utf8(canonical_bytes(&envoy_token_sds())?)?}
    }))?)
}

fn proxy_pod(binding: &RuntimeBinding, control: &str) -> Result<Pod> {
    let security = json!({"allowPrivilegeEscalation":false,"readOnlyRootFilesystem":true,"capabilities":{"drop":["ALL"]}});
    Ok(serde_json::from_value(json!({
        "apiVersion":"v1","kind":"Pod",
        "metadata":{"name":format!("reakt-proxy-{}-pod",binding.generation_id),"namespace":control,"uid":"proxy-pod-uid","resourceVersion":"40","labels":{"reakt.dev/proxy-generation":binding.generation_id}},
        "spec":{"nodeName":"worker","serviceAccountName":binding.service_account_name,"automountServiceAccountToken":false,
            "securityContext":{"runAsNonRoot":true,"runAsUser":65532,"runAsGroup":65532,"fsGroup":65532,"seccompProfile":{"type":"RuntimeDefault"}},
            "containers":[
                {"name":"envoy","image":ENVOY_IMAGE,"args":["-c","/etc/reakt/envoy/envoy.json","--concurrency","2"],"securityContext":security},
                {"name":"admin","image":ADMIN_IMAGE,"command":["sleep"],"args":["infinity"],"securityContext":security}
            ]},
        "status":{"phase":"Running","containerStatuses":[
            {"name":"envoy","image":ENVOY_IMAGE,"imageID":ENVOY_IMAGE,"containerID":"containerd://envoy","ready":true,"restartCount":0,"state":{"running":{}}},
            {"name":"admin","image":ADMIN_IMAGE,"imageID":ADMIN_IMAGE,"containerID":"containerd://admin","ready":true,"restartCount":0,"state":{"running":{}}}
        ]}
    }))?)
}

fn support_objects(
    stored: &StoredTransition,
    binding: &RuntimeBinding,
    control: &str,
) -> Result<Vec<Value>> {
    let name = &binding.service_account_name;
    let inventory = RuntimeInventory {
        schema_version: SCHEMA_VERSION.into(),
        binding: binding.clone(),
        activation_uid: stored
            .staged_identity_uids
            .get(&format!(
                "v1/ConfigMap/{control}/reakt-activation-{}",
                binding.domain_id
            ))
            .ok_or("activation UID missing")?
            .clone(),
        activation_resource_version: "45".into(),
        observations: vec![],
        conditions: vec![],
        drained: false,
    };
    Ok(vec![
        json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":format!("{name}-process"),"namespace":control,"uid":"process-sa-uid","resourceVersion":"40"},"automountServiceAccountToken":false}),
        json!({"apiVersion":"v1","kind":"Service","metadata":{"name":format!("reakt-proxy-{}",binding.generation_id),"namespace":control,"uid":"proxy-service-uid","resourceVersion":"40"},"spec":{"selector":{"reakt.dev/proxy-generation":binding.generation_id},"ports":[{"name":"https","port":8443,"targetPort":8443}]}}),
        // These tests resume a previously recorded immutable Secret. TLS bytes are
        // intentionally absent: certificate creation/handshake have separate tests.
        json!({"apiVersion":"v1","kind":"Secret","metadata":{"name":format!("{name}-tls"),"namespace":control,"uid":"tls-secret-uid","resourceVersion":"40"},"immutable":true,"type":"Opaque"}),
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":format!("reakt-inventory-{}",binding.domain_id),"namespace":control,"uid":"inventory-uid","resourceVersion":"40"},"data":{"state.json":serde_json::to_string(&inventory)?}}),
    ])
}

fn record_support(stored: &mut StoredTransition, value: &Value) -> Result<()> {
    let mut object: DynamicObject = serde_json::from_value(value.clone())?;
    let types = object.types.as_ref().ok_or("support type missing")?;
    // Pods and platform policy are observed support, not created stage objects.
    if types.kind == "Pod"
        || object
            .metadata
            .name
            .as_deref()
            .is_some_and(|name| name.starts_with("reakt-proxy-policy-"))
    {
        return Ok(());
    }
    let Some(namespace) = object.metadata.namespace.as_deref() else {
        return Ok(());
    };
    let key = format!(
        "{}/{}/{namespace}/{}",
        types.api_version,
        types.kind,
        object
            .metadata
            .name
            .as_deref()
            .ok_or("support name missing")?
    );
    stored.staged_identity_uids.insert(
        key.clone(),
        object.metadata.uid.clone().ok_or("support UID missing")?,
    );
    if types.kind != "Secret" {
        if let Some(data) = object.data.as_object_mut() {
            data.remove("status");
        }
        stored
            .staged_content_digests
            .insert(key, digest(&object.data)?);
    }
    Ok(())
}
