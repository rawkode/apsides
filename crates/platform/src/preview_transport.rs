//! Bounded raw transport for the authenticated live-impact preview.
//!
//! This module fixes request shape and response handling. It grants no approval or session
//! authority and never converts a preview into a live write.
use http::header::CONTENT_LENGTH;
use http_body_util::{BodyExt, Limited};
use kube::{
    Client,
    api::{DynamicObject, ListParams, ObjectList, Patch},
    client::Body,
    core::{Request, TypeMeta},
};
use serde_json::Value;
use std::time::Duration;

use crate::{ConditionalApplyRequest, Result, invalid};

const MAX_PREVIEW_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_PREVIEW_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const PREVIEW_DEADLINE: Duration = Duration::from_secs(15);
const MAX_UID_BYTES: usize = 128;
const MAX_RESOURCE_VERSION_BYTES: usize = 1024;

pub struct PreviewTarget {
    api_version: String,
    kind: String,
    namespace: String,
    name: String,
    path: String,
}

impl PreviewTarget {
    /// Construct one closed, namespaced managed-resource target.
    /// # Errors
    /// Rejects unsupported GVK/GVR combinations or malformed names and namespaces.
    pub fn new(
        api_version: &str,
        kind: &str,
        api_group: &str,
        resource: &str,
        namespace: &str,
        name: &str,
    ) -> Result<Self> {
        let resolved =
            compiler_core::resolve_namespaced_api(api_version, kind, api_group, resource)
                .map_err(|_| invalid("preview target API is unsupported"))?;
        if !super::valid_namespace(namespace) || !super::valid_name(name) {
            return Err(invalid("preview target identity is invalid"));
        }
        let path = if api_group.is_empty() {
            format!(
                "/api/{}/namespaces/{namespace}/{resource}",
                resolved.version
            )
        } else {
            format!(
                "/apis/{api_group}/{}/namespaces/{namespace}/{resource}",
                resolved.version
            )
        };
        Ok(Self {
            api_version: api_version.into(),
            kind: kind.into(),
            namespace: namespace.into(),
            name: name.into(),
            path,
        })
    }
}

pub struct NamedPreview {
    resource_version: String,
    object: Option<DynamicObject>,
}
impl NamedPreview {
    #[must_use]
    pub fn resource_version(&self) -> &str {
        &self.resource_version
    }
    #[must_use]
    /// Return the sensitive transient object. Callers must not log or persist it.
    pub const fn object(&self) -> Option<&DynamicObject> {
        self.object.as_ref()
    }
}

pub struct PreviewObject(DynamicObject);
impl PreviewObject {
    #[must_use]
    /// Return the sensitive transient dry-run object. Callers must not log or persist it.
    pub const fn object(&self) -> &DynamicObject {
        &self.0
    }
}

#[derive(Clone)]
pub struct PreviewTransport {
    client: Client,
}

impl PreviewTransport {
    /// Construct a low-level transport from a trusted client whose middleware does not retry or
    /// log request or response bodies. This constructor grants no preview authority.
    #[must_use]
    pub const fn new(client: Client) -> Self {
        Self { client }
    }

    /// Perform one exact-name LIST without decoding an error response body.
    /// # Errors
    /// Rejects timeout, transport failure, non-success, oversized/malformed response or drift.
    pub async fn list_named(&self, target: &PreviewTarget) -> Result<NamedPreview> {
        let params = ListParams::default()
            .fields(&format!("metadata.name={}", target.name))
            .limit(2);
        let request = Request::new(&target.path)
            .list(&params)
            .map_err(|_| invalid("preview list request is invalid"))?;
        let bytes = self.send(request).await?;
        let value = strict_response(&bytes)?;
        validate_raw_list_items(&value, target)?;
        let list: ObjectList<DynamicObject> = serde_json::from_value(value)
            .map_err(|_| invalid("preview list response is malformed"))?;
        let expected_kind = format!("{}List", target.kind);
        if list.types.api_version != target.api_version
            || list.types.kind != expected_kind
            || list
                .metadata
                .continue_
                .as_deref()
                .is_some_and(|value| !value.is_empty())
            || list
                .metadata
                .remaining_item_count
                .is_some_and(|count| count != 0)
            || list.items.len() > 1
        {
            return Err(invalid("preview list response identity is invalid"));
        }
        let resource_version = list
            .metadata
            .resource_version
            .filter(|value| valid_version(value))
            .ok_or_else(|| invalid("preview list resource version is invalid"))?;
        let mut object = list.items.into_iter().next();
        if object.as_ref().is_some_and(|object| {
            !matches_list_target(object, target)
                || !valid_uid_option(object.metadata.uid.as_deref())
                || !valid_version_option(object.metadata.resource_version.as_deref())
        }) {
            return Err(invalid("preview list object identity is invalid"));
        }
        if let Some(object) = object.as_mut()
            && object.types.is_none()
        {
            object.types = Some(TypeMeta {
                api_version: target.api_version.clone(),
                kind: target.kind.clone(),
            });
        }
        Ok(NamedPreview {
            resource_version,
            object,
        })
    }

    /// Perform one conditional SSA PATCH with forced `dryRun=All` and strict validation.
    /// # Errors
    /// Rejects mismatched payloads, timeout, transport failure, non-success or invalid response.
    pub async fn dry_run_apply(
        &self,
        target: &PreviewTarget,
        apply: &ConditionalApplyRequest,
    ) -> Result<PreviewObject> {
        validate_apply(target, apply)?;
        crate::preview_limits::validate_json(apply.payload(), MAX_PREVIEW_REQUEST_BYTES)?;
        let params = apply.dry_run_params();
        let request = Request::new(&target.path)
            .patch(&target.name, &params, &Patch::Apply(apply.payload()))
            .map_err(|_| invalid("preview patch request is invalid"))?;
        if request.body().len() > MAX_PREVIEW_REQUEST_BYTES {
            return Err(invalid("preview patch request is oversized"));
        }
        let bytes = self.send(request).await?;
        let value = strict_response(&bytes)?;
        let object: DynamicObject = serde_json::from_value(value)
            .map_err(|_| invalid("preview patch response is malformed"))?;
        let expected_uid = apply
            .payload()
            .pointer("/metadata/uid")
            .and_then(Value::as_str);
        if !matches_target(&object, target)
            || !valid_version_option(object.metadata.resource_version.as_deref())
            || object
                .metadata
                .uid
                .as_deref()
                .is_some_and(|uid| !valid_uid(uid))
            || expected_uid.is_some_and(|uid| object.metadata.uid.as_deref() != Some(uid))
        {
            return Err(invalid("preview patch response identity is invalid"));
        }
        Ok(PreviewObject(object))
    }

    async fn send(&self, request: http::Request<Vec<u8>>) -> Result<Vec<u8>> {
        tokio::time::timeout(PREVIEW_DEADLINE, async {
            let response = self
                .client
                .send(request.map(Body::from))
                .await
                .map_err(|_| invalid("preview transport failed"))?;
            if !response.status().is_success() {
                return Err(invalid("preview request was rejected"));
            }
            if response
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<usize>().ok())
                .is_some_and(|length| length > MAX_PREVIEW_RESPONSE_BYTES)
            {
                return Err(invalid("preview response is oversized"));
            }
            let body = response.into_body();
            let bytes = Limited::new(body, MAX_PREVIEW_RESPONSE_BYTES)
                .collect()
                .await
                .map_err(|_| invalid("preview response is oversized or unreadable"))?
                .to_bytes();
            Ok(bytes.to_vec())
        })
        .await
        .map_err(|_| invalid("preview request timed out"))?
    }
}

fn strict_response(bytes: &[u8]) -> Result<Value> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("preview response is malformed"))?;
    compiler_core::parse_strict(text).map_err(|_| invalid("preview response is malformed"))
}

fn validate_apply(target: &PreviewTarget, apply: &ConditionalApplyRequest) -> Result<()> {
    let payload = apply.payload();
    let uid = payload.pointer("/metadata/uid").and_then(Value::as_str);
    if payload.get("apiVersion").and_then(Value::as_str) != Some(&target.api_version)
        || payload.get("kind").and_then(Value::as_str) != Some(&target.kind)
        || payload.pointer("/metadata/name").and_then(Value::as_str) != Some(&target.name)
        || payload
            .pointer("/metadata/namespace")
            .and_then(Value::as_str)
            != Some(&target.namespace)
        || !valid_version_option(
            payload
                .pointer("/metadata/resourceVersion")
                .and_then(Value::as_str),
        )
        || uid.is_some_and(|value| !valid_uid(value))
    {
        return Err(invalid("preview patch payload identity is invalid"));
    }
    Ok(())
}

fn matches_target(object: &DynamicObject, target: &PreviewTarget) -> bool {
    object
        .types
        .as_ref()
        .is_some_and(|types| types.api_version == target.api_version && types.kind == target.kind)
        && object.metadata.name.as_deref() == Some(&target.name)
        && object.metadata.namespace.as_deref() == Some(&target.namespace)
}

fn matches_list_target(object: &DynamicObject, target: &PreviewTarget) -> bool {
    object
        .types
        .as_ref()
        .is_none_or(|types| types.api_version == target.api_version && types.kind == target.kind)
        && object.metadata.name.as_deref() == Some(&target.name)
        && object.metadata.namespace.as_deref() == Some(&target.namespace)
}

fn validate_raw_list_items(value: &Value, target: &PreviewTarget) -> Result<()> {
    let items = value
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("preview list response is malformed"))?;
    if items.len() > 1 {
        return Err(invalid("preview list response identity is invalid"));
    }
    for item in items {
        let api_version = item.get("apiVersion");
        let kind = item.get("kind");
        let valid = match (api_version, kind) {
            (None, None) => true,
            (Some(api_version), Some(kind)) => {
                api_version.as_str() == Some(&target.api_version)
                    && kind.as_str() == Some(&target.kind)
            }
            _ => false,
        };
        if !valid {
            return Err(invalid("preview list object identity is invalid"));
        }
    }
    Ok(())
}

fn valid_version_option(value: Option<&str>) -> bool {
    value.is_some_and(valid_version)
}
fn valid_version(value: &str) -> bool {
    value != "0" && valid_scalar(value, MAX_RESOURCE_VERSION_BYTES)
}
fn valid_uid_option(value: Option<&str>) -> bool {
    value.is_some_and(valid_uid)
}
fn valid_uid(value: &str) -> bool {
    valid_scalar(value, MAX_UID_BYTES)
}
fn valid_scalar(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

#[cfg(test)]
#[path = "preview_transport_tests.rs"]
mod tests;
