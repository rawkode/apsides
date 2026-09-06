//! Kubernetes field shapes come from pinned upstream generated types. The
//! allowlist below defines Reakt's smaller effect vocabulary, not a competing
//! Kubernetes schema. API admission/defaulting remains a separate release gate.
use crate::{Error, REVISION_ANNOTATION, Resource, Result};
use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{ConfigMap, Service},
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

fn invalid(path: &str, message: &str) -> Error {
    Error::Invalid(format!("{path}: {message}"))
}

pub(crate) fn string_map(value: &Value, path: &str) -> Result<()> {
    let map = value
        .as_object()
        .ok_or_else(|| invalid(path, "expected string map"))?;
    if map.len() > 1024
        || map
            .iter()
            .any(|(key, value)| key.is_empty() || key.len() > 253 || !value.is_string())
    {
        return Err(invalid(path, "invalid or oversized string map"));
    }
    Ok(())
}

fn preserved(input: &Value, output: &Value) -> bool {
    match input {
        Value::Object(map) => map
            .iter()
            .all(|(key, value)| output.get(key).is_some_and(|out| preserved(value, out))),
        Value::Array(values) => output.as_array().is_some_and(|out| {
            values.len() == out.len() && values.iter().zip(out).all(|(a, b)| preserved(a, b))
        }),
        _ => input == output,
    }
}

fn upstream_shape<T: DeserializeOwned + Serialize>(value: &Value, path: &str) -> Result<()> {
    let typed: T = serde_json::from_value(value.clone()).map_err(|_| {
        invalid(
            path,
            "resource does not match the pinned Kubernetes structural schema",
        )
    })?;
    let roundtrip = serde_json::to_value(typed)
        .map_err(|_| invalid(path, "Kubernetes resource cannot be represented"))?;
    // Generated deserializers may ignore unknown fields. No authored value may
    // disappear during the upstream type's serialization roundtrip.
    if !preserved(value, &roundtrip) {
        return Err(invalid(
            path,
            "unknown, null, or non-roundtrippable Kubernetes field",
        ));
    }
    Ok(())
}

fn allowed_fields(path: &str, kind: &str) -> Option<&'static [&'static str]> {
    match path {
        "" if kind == "ConfigMap" => Some(&["apiVersion", "kind", "metadata", "data"]),
        "" => Some(&["apiVersion", "kind", "metadata", "spec"]),
        "/metadata" => Some(&["name", "namespace", "labels", "annotations"]),
        "/spec" if kind == "Service" => Some(&["selector", "ports", "type"]),
        "/spec/ports/*" => Some(&["name", "port", "targetPort", "protocol"]),
        "/spec" => Some(&["replicas", "selector", "template"]),
        "/spec/selector" if kind == "Deployment" => Some(&["matchLabels"]),
        "/spec/template" => Some(&["metadata", "spec"]),
        "/spec/template/metadata" => Some(&["labels", "annotations"]),
        "/spec/template/spec" => Some(&[
            "serviceAccountName",
            "automountServiceAccountToken",
            "containers",
            "securityContext",
        ]),
        "/spec/template/spec/securityContext" => Some(&["runAsNonRoot"]),
        "/spec/template/spec/containers/*" => Some(&[
            "name",
            "image",
            "imagePullPolicy",
            "ports",
            "envFrom",
            "env",
            "resources",
            "command",
            "args",
            "securityContext",
        ]),
        "/spec/template/spec/containers/*/ports/*" => Some(&["name", "containerPort", "protocol"]),
        "/spec/template/spec/containers/*/envFrom/*" => {
            Some(&["configMapRef", "secretRef", "prefix"])
        }
        "/spec/template/spec/containers/*/envFrom/*/configMapRef"
        | "/spec/template/spec/containers/*/envFrom/*/secretRef" => Some(&["name"]),
        "/spec/template/spec/containers/*/env/*" => Some(&["name", "value", "valueFrom"]),
        "/spec/template/spec/containers/*/env/*/valueFrom" => Some(&["secretKeyRef"]),
        "/spec/template/spec/containers/*/env/*/valueFrom/secretKeyRef" => Some(&["name", "key"]),
        "/spec/template/spec/containers/*/resources" => Some(&["requests", "limits"]),
        "/spec/template/spec/containers/*/resources/requests"
        | "/spec/template/spec/containers/*/resources/limits" => Some(&["cpu", "memory"]),
        "/spec/template/spec/containers/*/securityContext" => Some(&[
            "runAsNonRoot",
            "allowPrivilegeEscalation",
            "readOnlyRootFilesystem",
        ]),
        _ => None,
    }
}

fn effect_vocabulary(value: &Value, path: &str, kind: &str, source: &str) -> Result<()> {
    match value {
        Value::Object(map) => {
            if allowed_fields(path, kind)
                .is_some_and(|allowed| map.keys().any(|key| !allowed.contains(&key.as_str())))
            {
                return Err(invalid(
                    source,
                    &format!("unsupported effect field at {path}"),
                ));
            }
            if path.ends_with("/annotations") && map.contains_key(REVISION_ANNOTATION) {
                return Err(invalid(
                    source,
                    "reserved rollout annotation has a compiled writer",
                ));
            }
            for (key, value) in map {
                effect_vocabulary(value, &format!("{path}/{key}"), kind, source)?;
            }
        }
        Value::Array(values) => {
            if values.len() > 128 {
                return Err(invalid(source, "array exceeds supported cardinality"));
            }
            for value in values {
                effect_vocabulary(value, &format!("{path}/*"), kind, source)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn custom_effect_vocabulary(value: &Value, path: &str, source: &str) -> Result<()> {
    match value {
        Value::Object(map) => {
            let allowed = match path {
                "" => Some(&["apiVersion", "kind", "metadata", "spec"][..]),
                "/metadata" => Some(&["name", "namespace", "labels", "annotations"][..]),
                _ => None,
            };
            if allowed.is_some_and(|fields| map.keys().any(|key| !fields.contains(&key.as_str()))) {
                return Err(invalid(
                    source,
                    &format!("unsupported custom resource field at {path}"),
                ));
            }
            if path.ends_with("/annotations") && map.contains_key(REVISION_ANNOTATION) {
                return Err(invalid(
                    source,
                    "reserved rollout annotation has a compiled writer",
                ));
            }
            if map.len() > 1024 || map.keys().any(|key| key.is_empty() || key.len() > 253) {
                return Err(invalid(source, "custom resource object is oversized"));
            }
            for (key, child) in map {
                custom_effect_vocabulary(child, &format!("{path}/{key}"), source)?;
            }
        }
        Value::Array(values) => {
            if values.len() > 128 {
                return Err(invalid(source, "array exceeds supported cardinality"));
            }
            for child in values {
                custom_effect_vocabulary(child, &format!("{path}/*"), source)?;
            }
        }
        Value::Null => {
            return Err(invalid(
                source,
                "null custom resource fields are unsupported",
            ));
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn validate_resource(resource: &Resource) -> Result<()> {
    let source = format!(
        "{}:{}:{} resource {}",
        resource.source.file, resource.source.line, resource.source.column, resource.id
    );
    let value = &resource.desired;
    if value["apiVersion"] != resource.api_version
        || value["kind"] != resource.kind
        || value["metadata"]["name"] != resource.name
        || value["metadata"]["namespace"] != resource.namespace
    {
        return Err(invalid(
            &source,
            "desired identity differs from declared fixed identity",
        ));
    }
    match (resource.kind.as_str(), resource.api_resource.is_some()) {
        ("ConfigMap", false) => upstream_shape::<ConfigMap>(value, &source)?,
        ("Service", false) => upstream_shape::<Service>(value, &source)?,
        ("Deployment", false) => upstream_shape::<Deployment>(value, &source)?,
        (_, true) => {
            if !value.is_object()
                || !value.get("spec").is_some_and(Value::is_object)
                || !value.get("metadata").is_some_and(Value::is_object)
            {
                return Err(invalid(
                    &source,
                    "custom managed resource requires object metadata and spec",
                ));
            }
            for field in ["labels", "annotations"] {
                if let Some(map) = value.pointer(&format!("/metadata/{field}")) {
                    string_map(map, &format!("{source} metadata.{field}"))?;
                }
            }
            custom_effect_vocabulary(value, "", &source)?;
        }
        _ => return Err(invalid(&source, "unsupported managed type")),
    }
    if resource.api_resource.is_none() {
        effect_vocabulary(value, "", &resource.kind, &source)?;
    }
    if resource.kind == "Service"
        && value["spec"]
            .get("type")
            .is_some_and(|kind| kind != "ClusterIP")
    {
        return Err(invalid(
            &source,
            "only ClusterIP Service effects are supported",
        ));
    }
    if resource.kind == "Deployment" {
        deployment_contract(value, &source)?;
    }
    Ok(())
}

fn deployment_contract(value: &Value, source: &str) -> Result<()> {
    let selector = value
        .pointer("/spec/selector/matchLabels")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid(source, "explicit matchLabels selector is required"))?;
    for (key, label) in selector {
        if value
            .pointer("/spec/template/metadata/labels")
            .and_then(|labels| labels.get(key))
            != Some(label)
        {
            return Err(invalid(
                source,
                "Deployment selector does not match its Pod template",
            ));
        }
    }
    if selector.is_empty() {
        return Err(invalid(source, "empty selector is unsupported"));
    }
    if value
        .pointer("/spec/replicas")
        .is_some_and(|replicas| replicas.as_u64().is_none_or(|n| n > 10000))
    {
        return Err(invalid(source, "replicas exceeds supported range"));
    }
    deployment_env_contract(value, source)?;
    let containers = value
        .pointer("/spec/template/spec/containers")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(source, "containers are required"))?;
    if containers.is_empty() {
        return Err(invalid(source, "at least one container is required"));
    }
    let mut names = std::collections::BTreeSet::new();
    for container in containers {
        if container["name"].as_str().is_none_or(str::is_empty)
            || container["image"].as_str().is_none_or(str::is_empty)
            || !names.insert(container["name"].as_str())
        {
            return Err(invalid(
                source,
                "container names/images must be present and names unique",
            ));
        }
        if let Some(security) = container.get("securityContext").and_then(Value::as_object) {
            for (key, value) in security {
                if value != &Value::Bool(key != "allowPrivilegeEscalation") {
                    return Err(invalid(
                        source,
                        "only restrictive container security values are supported",
                    ));
                }
            }
        }
    }
    if value
        .pointer("/spec/template/spec/securityContext/runAsNonRoot")
        .is_some_and(|value| value != true)
    {
        return Err(invalid(source, "runAsNonRoot must be true when declared"));
    }
    Ok(())
}

fn deployment_env_contract(value: &Value, source: &str) -> Result<()> {
    let containers = value
        .pointer("/spec/template/spec/containers")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(source, "Deployment containers are required"))?;
    for container in containers {
        let Some(env) = container.get("env").and_then(Value::as_array) else {
            continue;
        };
        for variable in env {
            let direct = variable.get("value").and_then(Value::as_str);
            let secret = variable.pointer("/valueFrom/secretKeyRef");
            if direct.is_some() == secret.is_some() {
                return Err(invalid(
                    source,
                    "Deployment env requires exactly one value or secretKeyRef",
                ));
            }
            if let Some(secret) = secret {
                let name = secret.get("name").and_then(Value::as_str);
                let key = secret.get("key").and_then(Value::as_str);
                if name.is_none_or(str::is_empty)
                    || key.is_none_or(str::is_empty)
                    || name.is_some_and(|value| value.len() > 253)
                    || key.is_some_and(|value| value.len() > 253)
                {
                    return Err(invalid(source, "Deployment secretKeyRef is invalid"));
                }
            }
        }
    }
    Ok(())
}
