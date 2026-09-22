//! Kubernetes operations the fixture driver performs, over kube-rs.
//!
//! Objects are handled as JSON so the driver can apply exactly what
//! [`crate::manifests`] rendered. Only the kinds in [`KINDS`] are accepted;
//! anything else is a programming error surfaced as an infrastructure
//! failure.

use crate::{Error, Result, infrastructure};
use k8s_openapi::api::{authentication::v1::TokenRequest, core::v1::Pod};
use kube::{
    Api, Client,
    api::{
        ApiResource, AttachParams, DeleteParams, DynamicObject, GroupVersionKind, ListParams,
        LogParams, Patch, PatchParams, PostParams,
    },
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::AsyncReadExt as _;

/// `(apiVersion, kind, plural, namespaced)` for every kind the driver reads
/// or writes.
const KINDS: &[(&str, &str, &str, bool)] = &[
    ("v1", "Namespace", "namespaces", false),
    ("v1", "ServiceAccount", "serviceaccounts", true),
    ("v1", "ConfigMap", "configmaps", true),
    ("v1", "Secret", "secrets", true),
    ("v1", "Service", "services", true),
    ("v1", "Pod", "pods", true),
    ("v1", "Event", "events", true),
    ("apps/v1", "Deployment", "deployments", true),
    ("rbac.authorization.k8s.io/v1", "Role", "roles", true),
    (
        "rbac.authorization.k8s.io/v1",
        "RoleBinding",
        "rolebindings",
        true,
    ),
    (
        "networking.k8s.io/v1",
        "NetworkPolicy",
        "networkpolicies",
        true,
    ),
    (
        "apiextensions.k8s.io/v1",
        "CustomResourceDefinition",
        "customresourcedefinitions",
        false,
    ),
];

/// How often condition waits poll.
pub const POLL: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub struct Driver {
    client: Client,
}

impl Driver {
    pub const fn new(client: Client) -> Self {
        Self { client }
    }

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
            return infrastructure(format!(
                "the fixture driver does not handle {api_version} {kind}"
            ));
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

    /// Server-side apply `object` as `manager`.
    pub async fn apply(&self, object: &Value, manager: &str, force: bool) -> Result<Value> {
        let (api, name) = self.api_for(object)?;
        let mut params = PatchParams::apply(manager);
        if force {
            params = params.force();
        }
        let applied = api.patch(&name, &params, &Patch::Apply(object)).await?;
        Ok(serde_json::to_value(applied)?)
    }

    /// Apply as the fixture driver, taking ownership of every field it sets.
    pub async fn put(&self, object: &Value) -> Result<Value> {
        self.apply(object, crate::manifests::DRIVER_MANAGER, true)
            .await
    }

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

    /// The object, which must exist.
    pub async fn require(
        &self,
        api_version: &str,
        kind: &str,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Value> {
        self.get(api_version, kind, namespace, name)
            .await?
            .ok_or_else(|| Error::Case(format!("{kind} {name} does not exist")))
    }

    /// Every object of a kind in a namespace, for diagnostics.
    pub async fn list(&self, api_version: &str, kind: &str, namespace: &str) -> Result<Value> {
        let listed = self
            .api(api_version, kind, Some(namespace))?
            .list(&ListParams::default())
            .await?;
        Ok(serde_json::to_value(listed.items)?)
    }

    pub async fn delete(
        &self,
        api_version: &str,
        kind: &str,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<()> {
        self.api(api_version, kind, namespace)?
            .delete(name, &DeleteParams::default())
            .await?;
        Ok(())
    }

    /// A JSON merge patch, as another writer than the fixture driver's
    /// server-side apply identity.
    pub async fn merge(
        &self,
        api_version: &str,
        kind: &str,
        namespace: Option<&str>,
        name: &str,
        patch: &Value,
    ) -> Result<Value> {
        let patched = self
            .api(api_version, kind, namespace)?
            .patch(
                name,
                &PatchParams {
                    field_manager: Some("aps-test-fixture-patch".into()),
                    ..PatchParams::default()
                },
                &Patch::Merge(patch),
            )
            .await?;
        Ok(serde_json::to_value(patched)?)
    }

    pub async fn scale(&self, namespace: &str, name: &str, replicas: u32) -> Result<()> {
        self.merge(
            "apps/v1",
            "Deployment",
            Some(namespace),
            name,
            &json!({"spec": {"replicas": replicas}}),
        )
        .await?;
        Ok(())
    }

    pub async fn pods(&self, namespace: &str, selector: &str) -> Result<Vec<Pod>> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        Ok(pods
            .list(&ListParams::default().labels(selector))
            .await?
            .items)
    }

    /// A container's log, or an empty string when there is none to read.
    pub async fn logs(
        &self,
        namespace: &str,
        pod: &str,
        container: &str,
        previous: bool,
    ) -> String {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let params = LogParams {
            container: Some(container.to_owned()),
            previous,
            ..LogParams::default()
        };
        pods.logs(pod, &params).await.unwrap_or_default()
    }

    /// Run `command` in a container and fail unless it exits zero.
    pub async fn exec(
        &self,
        namespace: &str,
        pod: &str,
        container: &str,
        command: &[&str],
    ) -> Result<()> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let params = AttachParams::default()
            .container(container)
            .stdout(false)
            .stderr(true);
        let mut attached = pods.exec(pod, command.iter().copied(), &params).await?;
        let status = attached.take_status();
        let mut output = String::new();
        if let Some(mut stderr) = attached.stderr() {
            let _ = stderr.read_to_string(&mut output).await;
        }
        let status = match status {
            Some(status) => status.await,
            None => None,
        };
        attached
            .join()
            .await
            .map_err(|error| Error::Case(error.to_string()))?;
        if status.as_ref().and_then(|status| status.status.as_deref()) != Some("Success") {
            return Err(Error::Case(format!(
                "`{}` in {pod}/{container} failed: {}",
                command.join(" "),
                output.trim()
            )));
        }
        Ok(())
    }

    /// A bound token for a `ServiceAccount`, valid for `seconds`.
    pub async fn token(&self, namespace: &str, account: &str, seconds: i64) -> Result<String> {
        let accounts: Api<k8s_openapi::api::core::v1::ServiceAccount> =
            Api::namespaced(self.client.clone(), namespace);
        let request = TokenRequest {
            spec: Some(k8s_openapi::api::authentication::v1::TokenRequestSpec {
                expiration_seconds: Some(seconds),
                ..Default::default()
            }),
            ..Default::default()
        };
        let issued = accounts
            .create_token_request(account, &PostParams::default(), &request)
            .await?;
        issued
            .status
            .and_then(|status| status.token)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| Error::Infrastructure("the API server issued no token".into()))
    }
}

/// Poll `probe` until it yields a value or `timeout` passes. Probe errors
/// count as "not yet", and the last one is reported if the deadline passes.
pub async fn wait_for<T>(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn waits_return_the_first_value_and_report_the_last_error() -> Result<()> {
        let mut calls = 0;
        let value = wait_for("counter", Duration::from_secs(5), async || {
            calls += 1;
            Ok((calls == 3).then_some(calls))
        })
        .await?;
        assert_eq!(value, 3);
        let failed: Result<()> = wait_for("never", Duration::from_millis(300), async || {
            Err(Error::Case("still broken".into()))
        })
        .await;
        let Err(Error::Deadline(message)) = failed else {
            return Err(Error::Case("expected a deadline".into()));
        };
        assert!(message.contains("still broken"), "{message}");
        Ok(())
    }
}
