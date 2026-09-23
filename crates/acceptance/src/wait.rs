//! Bounded polling, and the diagnostics collected when a scenario fails.
//!
//! Diagnostics name an explicit allowlist of kinds and keep only metadata,
//! status and events. They never include Secrets, container environments or
//! kubeconfigs.

use crate::{Error, Result, objects::Objects};
use serde_json::{Map, Value, json};
use std::time::Duration;

/// How often condition waits poll.
pub const POLL: Duration = Duration::from_secs(1);

/// Poll `probe` until it yields a value or `timeout` passes. Probe errors
/// count as "not yet"; the last one is reported if the deadline passes.
///
/// # Errors
/// Returns [`Error::Deadline`] naming `label` when the deadline passes.
pub async fn until<T>(
    label: &str,
    timeout: Duration,
    mut probe: impl AsyncFnMut() -> Result<Option<T>>,
) -> Result<T> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = None;
    loop {
        match probe().await {
            Ok(Some(value)) => return Ok(value),
            Ok(None) => {}
            Err(error) => last = Some(error.to_string()),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::Deadline(last.map_or_else(
                || label.to_owned(),
                |error| format!("{label} (last error: {error})"),
            )));
        }
        tokio::time::sleep(POLL).await;
    }
}

/// `(apiVersion, kind)` diagnostics may include. Secrets are deliberately
/// absent, and `ConfigMap`s contribute names only.
const DIAGNOSTIC_KINDS: &[(&str, &str)] = &[
    ("apps/v1", "Deployment"),
    ("apps/v1", "ReplicaSet"),
    ("v1", "Pod"),
    ("v1", "Service"),
    ("v1", "ServiceAccount"),
    ("v1", "PersistentVolumeClaim"),
    ("v1", "ConfigMap"),
    ("rbac.authorization.k8s.io/v1", "Role"),
    ("rbac.authorization.k8s.io/v1", "RoleBinding"),
    ("postgresql.cnpg.io/v1", "Cluster"),
    ("v1", "Event"),
];

/// The allowlisted objects of `namespaces`, reduced to what a reader needs to
/// see why a step stalled. Errors reading one kind are recorded in place.
pub async fn diagnostics(objects: &Objects, namespaces: &[&str]) -> Value {
    let mut result = Map::new();
    for namespace in namespaces {
        let mut kinds = Map::new();
        for (api_version, kind) in DIAGNOSTIC_KINDS {
            let listed = objects.list(api_version, kind, Some(namespace), None).await;
            let value = match listed {
                Ok(items) => Value::Array(items.iter().map(|item| reduce(kind, item)).collect()),
                Err(error) => json!({"error": error.to_string()}),
            };
            kinds.insert((*kind).to_owned(), value);
        }
        result.insert((*namespace).to_owned(), Value::Object(kinds));
    }
    Value::Object(result)
}

/// The fields of `item` a diagnostic keeps for its kind.
fn reduce(kind: &str, item: &Value) -> Value {
    let metadata = &item["metadata"];
    let identity = json!({
        "name": metadata["name"],
        "uid": metadata["uid"],
        "resourceVersion": metadata["resourceVersion"],
        "labels": metadata["labels"],
        "annotations": annotations(metadata),
        "ownerReferences": metadata["ownerReferences"],
        "deletionTimestamp": metadata["deletionTimestamp"],
        "finalizers": metadata["finalizers"],
    });
    match kind {
        "ConfigMap" | "ServiceAccount" => json!({"metadata": identity}),
        "Event" => json!({
            "metadata": {"name": metadata["name"]},
            "type": item["type"],
            "reason": item["reason"],
            "message": item["message"],
            "count": item["count"],
            "involvedObject": {
                "kind": item["involvedObject"]["kind"],
                "name": item["involvedObject"]["name"],
            },
            "lastTimestamp": item["lastTimestamp"],
        }),
        "Role" => json!({"metadata": identity, "rules": item["rules"]}),
        "RoleBinding" => json!({
            "metadata": identity,
            "roleRef": item["roleRef"],
            "subjects": item["subjects"],
        }),
        "Pod" => json!({
            "metadata": identity,
            "spec": {
                "nodeName": item["spec"]["nodeName"],
                "serviceAccountName": item["spec"]["serviceAccountName"],
                "containers": item["spec"]["containers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|container| json!({"name": container["name"], "image": container["image"]}))
                    .collect::<Vec<_>>(),
            },
            "status": item["status"],
        }),
        _ => json!({
            "metadata": identity,
            "spec": {"replicas": item["spec"]["replicas"], "selector": item["spec"]["selector"]},
            "status": item["status"],
        }),
    }
}

/// Annotations other than `kubectl.kubernetes.io/last-applied-configuration`,
/// which would repeat the object's whole content.
fn annotations(metadata: &Value) -> Value {
    metadata["annotations"]
        .as_object()
        .map_or(Value::Null, |annotations| {
            Value::Object(
                annotations
                    .iter()
                    .filter(|(key, _)| {
                        key.as_str() != "kubectl.kubernetes.io/last-applied-configuration"
                    })
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn waits_return_the_first_value_and_report_the_last_error() -> Result {
        let mut calls = 0;
        let value = until("counter", Duration::from_secs(10), async || {
            calls += 1;
            Ok((calls == 2).then_some(calls))
        })
        .await?;
        assert_eq!(value, 2);
        let failed: Result<()> = until("never", Duration::from_millis(10), async || {
            Err(Error::Check("still broken".into()))
        })
        .await;
        assert!(
            matches!(&failed, Err(Error::Deadline(message)) if message.contains("still broken")),
            "{failed:?}"
        );
        Ok(())
    }

    #[test]
    fn diagnostics_drop_data_and_container_environments() {
        let config = json!({"metadata": {"name": "settings"}, "data": {"key": "value"}});
        assert_eq!(reduce("ConfigMap", &config)["data"], Value::Null);
        let pod = json!({
            "metadata": {"name": "api"},
            "spec": {"containers": [{"name": "api", "image": "i", "env": [{"name": "X", "value": "secret"}]}]},
        });
        let reduced = reduce("Pod", &pod);
        assert_eq!(reduced["spec"]["containers"][0]["env"], Value::Null);
        assert_eq!(reduced["spec"]["containers"][0]["image"], "i");
    }
}
