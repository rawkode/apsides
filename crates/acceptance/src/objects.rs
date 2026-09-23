//! Kubernetes objects as JSON, over kube-rs, for the kinds the suite reads or
//! writes.
//!
//! Every change the suite makes to an existing object is a full replacement
//! carrying the object's UID and resourceVersion, so the API server rejects it
//! if anything else changed the object since it was read.

use crate::{Error, Result, infrastructure};
use kube::{
    Api, Client,
    api::{
        ApiResource, DeleteParams, DynamicObject, GroupVersionKind, ListParams, Patch, PatchParams,
        PostParams, Preconditions,
    },
};
use serde_json::Value;

/// `(apiVersion, kind, plural, namespaced)` for every kind the suite handles.
const KINDS: &[(&str, &str, &str, bool)] = &[
    ("v1", "Namespace", "namespaces", false),
    ("v1", "ServiceAccount", "serviceaccounts", true),
    ("v1", "ConfigMap", "configmaps", true),
    ("v1", "Secret", "secrets", true),
    ("v1", "Service", "services", true),
    ("v1", "Pod", "pods", true),
    (
        "v1",
        "PersistentVolumeClaim",
        "persistentvolumeclaims",
        true,
    ),
    ("v1", "Event", "events", true),
    ("v1", "Node", "nodes", false),
    ("apps/v1", "Deployment", "deployments", true),
    ("apps/v1", "ReplicaSet", "replicasets", true),
    ("rbac.authorization.k8s.io/v1", "Role", "roles", true),
    (
        "rbac.authorization.k8s.io/v1",
        "RoleBinding",
        "rolebindings",
        true,
    ),
    (
        "rbac.authorization.k8s.io/v1",
        "ClusterRole",
        "clusterroles",
        false,
    ),
    (
        "rbac.authorization.k8s.io/v1",
        "ClusterRoleBinding",
        "clusterrolebindings",
        false,
    ),
    (
        "apiextensions.k8s.io/v1",
        "CustomResourceDefinition",
        "customresourcedefinitions",
        false,
    ),
    ("postgresql.cnpg.io/v1", "Cluster", "clusters", true),
    ("postgresql.cnpg.io/v1", "Backup", "backups", true),
    (
        "admissionregistration.k8s.io/v1",
        "ValidatingAdmissionPolicy",
        "validatingadmissionpolicies",
        false,
    ),
    (
        "admissionregistration.k8s.io/v1",
        "ValidatingAdmissionPolicyBinding",
        "validatingadmissionpolicybindings",
        false,
    ),
];

/// JSON access to the suite's kinds with one client's identity.
#[derive(Clone)]
pub struct Objects {
    client: Client,
}

impl Objects {
    #[must_use]
    pub const fn new(client: Client) -> Self {
        Self { client }
    }

    #[must_use]
    pub const fn client(&self) -> &Client {
        &self.client
    }

    fn api(
        &self,
        api_version: &str,
        kind: &str,
        namespace: Option<&str>,
    ) -> Result<Api<DynamicObject>> {
        let Some((_, _, plural, namespaced)) = KINDS
            .iter()
            .find(|(version, name, _, _)| *version == api_version && *name == kind)
        else {
            return infrastructure(format!("the suite does not handle {api_version} {kind}"));
        };
        let (group, version) = api_version.split_once('/').unwrap_or(("", api_version));
        let resource =
            ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk(group, version, kind), plural);
        Ok(match (namespaced, namespace) {
            (true, Some(namespace)) => {
                Api::namespaced_with(self.client.clone(), namespace, &resource)
            }
            (false, _) => Api::all_with(self.client.clone(), &resource),
            (true, None) => return infrastructure(format!("{kind} requires a namespace")),
        })
    }

    fn api_for(&self, object: &Value) -> Result<(Api<DynamicObject>, String)> {
        let api = self.api(
            object["apiVersion"].as_str().unwrap_or_default(),
            object["kind"].as_str().unwrap_or_default(),
            object["metadata"]["namespace"].as_str(),
        )?;
        let name = object["metadata"]["name"].as_str().unwrap_or_default();
        if name.is_empty() {
            return infrastructure("object has no name");
        }
        Ok((api, name.to_owned()))
    }

    /// Create `object` and return it as the API server stored it.
    ///
    /// # Errors
    /// Fails for an unhandled kind or when the API server refuses the object.
    pub async fn create(&self, object: &Value) -> Result<Value> {
        let (api, _) = self.api_for(object)?;
        let object: DynamicObject = serde_json::from_value(object.clone())?;
        Ok(serde_json::to_value(
            api.create(&PostParams::default(), &object).await?,
        )?)
    }

    /// Replace `object`, which must carry the UID and resourceVersion it was
    /// read with.
    ///
    /// # Errors
    /// Fails when either precondition is missing or the object changed since.
    pub async fn replace(&self, object: &Value) -> Result<Value> {
        if object["metadata"]["uid"].as_str().is_none_or(str::is_empty)
            || object["metadata"]["resourceVersion"]
                .as_str()
                .is_none_or(str::is_empty)
        {
            return infrastructure("a replacement must carry its UID and resourceVersion");
        }
        let (api, name) = self.api_for(object)?;
        let object: DynamicObject = serde_json::from_value(object.clone())?;
        Ok(serde_json::to_value(
            api.replace(&name, &PostParams::default(), &object).await?,
        )?)
    }

    /// Replace `object` as [`Objects::replace`] would, without persisting
    /// anything: the request passes validation and admission only.
    ///
    /// # Errors
    /// As [`Objects::replace`], including any admission denial.
    pub async fn replace_dry_run(&self, object: &Value) -> Result<Value> {
        let (api, name) = self.api_for(object)?;
        let object: DynamicObject = serde_json::from_value(object.clone())?;
        let params = PostParams {
            dry_run: true,
            ..PostParams::default()
        };
        Ok(serde_json::to_value(
            api.replace(&name, &params, &object).await?,
        )?)
    }

    /// Server-side apply `object` as `manager`, taking ownership of every
    /// field it names.
    ///
    /// # Errors
    /// Fails for an unhandled kind or when the API server refuses the apply.
    pub async fn apply(&self, object: &Value, manager: &str) -> Result<Value> {
        let (api, name) = self.api_for(object)?;
        let params = PatchParams::apply(manager).force();
        Ok(serde_json::to_value(
            api.patch(&name, &params, &Patch::Apply(object)).await?,
        )?)
    }

    /// Delete `object` only while it still has the UID and resourceVersion
    /// it was read with. Returns once the API server accepted the request;
    /// finalizers may keep the object present.
    ///
    /// # Errors
    /// Fails when either precondition is missing or no longer holds.
    pub async fn delete(&self, object: &Value) -> Result {
        let uid = object["metadata"]["uid"].as_str().unwrap_or_default();
        let version = object["metadata"]["resourceVersion"]
            .as_str()
            .unwrap_or_default();
        if uid.is_empty() || version.is_empty() {
            return infrastructure("a deletion must carry its UID and resourceVersion");
        }
        let (api, name) = self.api_for(object)?;
        let params = DeleteParams {
            preconditions: Some(Preconditions {
                uid: Some(uid.to_owned()),
                resource_version: Some(version.to_owned()),
            }),
            ..DeleteParams::default()
        };
        api.delete(&name, &params).await?;
        Ok(())
    }

    /// Delete a cluster-scoped or namespaced object by name if it exists.
    ///
    /// # Errors
    /// Fails for an unhandled kind or any API error other than absence.
    pub async fn remove(
        &self,
        api_version: &str,
        kind: &str,
        namespace: Option<&str>,
        name: &str,
    ) -> Result {
        match self
            .api(api_version, kind, namespace)?
            .delete(name, &DeleteParams::default())
            .await
        {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(status)) if status.code == 404 => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// The named object, or `None` when it does not exist.
    ///
    /// # Errors
    /// Fails for an unhandled kind or any API error other than absence.
    pub async fn get(
        &self,
        api_version: &str,
        kind: &str,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Option<Value>> {
        let object = self
            .api(api_version, kind, namespace)?
            .get_opt(name)
            .await?;
        object
            .map(serde_json::to_value)
            .transpose()
            .map_err(Error::from)
    }

    /// The named object, which must exist.
    ///
    /// # Errors
    /// As [`Objects::get`], and when the object is absent.
    pub async fn require(
        &self,
        api_version: &str,
        kind: &str,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Value> {
        self.get(api_version, kind, namespace, name)
            .await?
            .ok_or_else(|| Error::Check(format!("{kind} {name} does not exist")))
    }

    /// Every object of a kind, in `namespace` when the kind is namespaced,
    /// optionally narrowed by a label selector.
    ///
    /// # Errors
    /// Fails for an unhandled kind or an API error.
    pub async fn list(
        &self,
        api_version: &str,
        kind: &str,
        namespace: Option<&str>,
        selector: Option<&str>,
    ) -> Result<Vec<Value>> {
        let mut params = ListParams::default();
        if let Some(selector) = selector {
            params = params.labels(selector);
        }
        let listed = self
            .api(api_version, kind, namespace)?
            .list(&params)
            .await?;
        listed
            .items
            .into_iter()
            .map(|item| serde_json::to_value(item).map_err(Error::from))
            .collect()
    }
}

/// A string field of `object` at the JSON pointer `path`, or `""`.
#[must_use]
pub fn text<'a>(object: &'a Value, path: &str) -> &'a str {
    object
        .pointer(path)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// Whether `object` is being deleted.
#[must_use]
pub fn deleting(object: &Value) -> bool {
    !object["metadata"]["deletionTimestamp"].is_null()
}

/// Whether `object` lists a controlling owner of `kind` with `uid`.
#[must_use]
pub fn controlled_by(object: &Value, kind: &str, uid: &str) -> bool {
    object["metadata"]["ownerReferences"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|owner| owner["kind"] == kind && owner["uid"] == uid && owner["controller"] == true)
}

/// Whether a Pod is running, Ready and not being deleted.
#[must_use]
pub fn pod_ready(pod: &Value) -> bool {
    !deleting(pod)
        && pod["status"]["phase"] == "Running"
        && pod["status"]["conditions"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|condition| condition["type"] == "Ready" && condition["status"] == "True")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ownership_and_readiness_read_the_fields_kubernetes_sets() {
        let pod = json!({
            "metadata": {"ownerReferences": [{"kind": "ReplicaSet", "uid": "rs", "controller": true}]},
            "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}]},
        });
        assert!(controlled_by(&pod, "ReplicaSet", "rs"));
        assert!(!controlled_by(&pod, "ReplicaSet", "other"));
        assert!(pod_ready(&pod));
        let mut deleting_pod = pod.clone();
        deleting_pod["metadata"]["deletionTimestamp"] = json!("2026-01-01T00:00:00Z");
        assert!(!pod_ready(&deleting_pod));
        assert_eq!(text(&pod, "/status/phase"), "Running");
        assert_eq!(text(&pod, "/status/missing"), "");
    }
}
