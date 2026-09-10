//! Construction of strict conditional server-side-apply requests.
//!
//! These values carry desired object data, so they intentionally do not implement `Debug`.
//! Construction adds optimistic-concurrency metadata but does not prove that an object is absent,
//! establish ownership, or qualify a Kubernetes resource version.
use kube::api::{PatchParams, ValidationDirective};
use serde_json::Value;

use crate::Result;

#[derive(Clone, Copy)]
pub enum ApplyPrecondition<'a> {
    Absent {
        list_resource_version: &'a str,
    },
    Existing {
        uid: &'a str,
        resource_version: &'a str,
    },
}

pub struct ConditionalApplyRequest {
    payload: Value,
    params: PatchParams,
}

impl ConditionalApplyRequest {
    #[must_use]
    pub fn payload(&self) -> &Value {
        &self.payload
    }

    #[must_use]
    pub fn params(&self) -> &PatchParams {
        &self.params
    }

    #[must_use]
    pub fn dry_run_params(&self) -> PatchParams {
        self.params.clone().dry_run()
    }
}

/// Clone a desired object and add the exact UID/resourceVersion preconditions for strict SSA.
///
/// The caller remains responsible for authenticated claim checks, fresh absence/existence
/// evidence, dry-run interpretation, and revalidation immediately before a live write.
/// The caller must also bind the desired apiVersion/kind to its selected API endpoint.
/// Existing UID/resourceVersion fields in the desired value are discarded before the supplied
/// precondition is installed; the caller's desired value is never modified.
/// # Errors
/// Rejects malformed desired metadata, mismatched identity, invalid preconditions, or manager.
pub fn conditional_apply_request(
    desired: &Value,
    name: &str,
    namespace: &str,
    field_manager: &str,
    force: bool,
    precondition: ApplyPrecondition<'_>,
) -> Result<ConditionalApplyRequest> {
    if !super::valid_name(name)
        || !super::valid_namespace(namespace)
        || field_manager.is_empty()
        || field_manager.len() > 128
        || field_manager.chars().any(char::is_control)
    {
        return Err(super::invalid(
            "conditional apply request parameters are invalid",
        ));
    }
    let mut payload = desired.clone();
    let metadata = payload
        .get_mut("metadata")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| super::invalid("conditional apply desired metadata is invalid"))?;
    if metadata.get("name").and_then(Value::as_str) != Some(name)
        || metadata.get("namespace").and_then(Value::as_str) != Some(namespace)
    {
        return Err(super::invalid(
            "conditional apply desired metadata is invalid",
        ));
    }
    metadata.remove("uid");
    metadata.remove("resourceVersion");
    match precondition {
        ApplyPrecondition::Absent {
            list_resource_version,
        } => {
            validate_version(list_resource_version)?;
            metadata.insert(
                "resourceVersion".into(),
                Value::String(list_resource_version.into()),
            );
        }
        ApplyPrecondition::Existing {
            uid,
            resource_version,
        } => {
            if uid.is_empty() || uid.chars().any(char::is_control) {
                return Err(super::invalid("conditional apply UID is invalid"));
            }
            validate_version(resource_version)?;
            metadata.insert("uid".into(), Value::String(uid.into()));
            metadata.insert(
                "resourceVersion".into(),
                Value::String(resource_version.into()),
            );
        }
    }
    let params = PatchParams::apply(field_manager).validation(ValidationDirective::Strict);
    let params = if force { params.force() } else { params };
    Ok(ConditionalApplyRequest { payload, params })
}

fn validate_version(value: &str) -> Result<()> {
    if value.is_empty() || value == "0" || value.chars().any(char::is_control) {
        return Err(super::invalid(
            "conditional apply resource version is invalid",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn desired() -> Value {
        json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "settings",
                "namespace": "workload",
                "uid": "untrusted-uid",
                "resourceVersion": "untrusted-version"
            },
            "data": {"private": "value"}
        })
    }

    #[test]
    fn absent_request_preserves_desired_and_uses_only_list_version() -> Result<()> {
        let desired = desired();
        let original = desired.clone();
        let request = conditional_apply_request(
            &desired,
            "settings",
            "workload",
            "apsides-example",
            false,
            ApplyPrecondition::Absent {
                list_resource_version: "41",
            },
        )?;
        assert_eq!(desired, original);
        assert_eq!(request.payload()["metadata"]["resourceVersion"], "41");
        assert!(request.payload()["metadata"].get("uid").is_none());
        assert!(!request.params().dry_run);
        assert!(!request.params().force);
        assert_eq!(
            request.params().field_manager.as_deref(),
            Some("apsides-example")
        );
        assert_eq!(
            request
                .params()
                .field_validation
                .as_ref()
                .map(ValidationDirective::as_str),
            Some("Strict")
        );
        assert!(request.dry_run_params().dry_run);
        Ok(())
    }

    #[test]
    fn existing_request_preserves_exact_uid_version_and_force() -> Result<()> {
        let desired = desired();
        let request = conditional_apply_request(
            &desired,
            "settings",
            "workload",
            "apsides-example",
            true,
            ApplyPrecondition::Existing {
                uid: "protected-uid",
                resource_version: "42",
            },
        )?;
        assert_eq!(request.payload()["metadata"]["uid"], "protected-uid");
        assert_eq!(request.payload()["metadata"]["resourceVersion"], "42");
        assert!(request.params().force);
        let dry_run = request.dry_run_params();
        assert!(dry_run.dry_run && dry_run.force);
        assert_eq!(dry_run.field_manager, request.params().field_manager);
        assert!(matches!(
            dry_run.field_validation,
            Some(ValidationDirective::Strict)
        ));
        assert_eq!(request.payload()["data"]["private"], "value");
        Ok(())
    }

    #[test]
    fn malformed_metadata_versions_and_manager_are_rejected_without_values() -> Result<()> {
        let valid = desired();
        let attempt = |desired: &Value, version: &str, manager: &str| {
            conditional_apply_request(
                desired,
                "settings",
                "workload",
                manager,
                false,
                ApplyPrecondition::Existing {
                    uid: "protected-uid",
                    resource_version: version,
                },
            )
        };
        for version in ["", "0", "bad\nprivate-version"] {
            let error = attempt(&valid, version, "apsides-example");
            assert!(error.is_err());
        }
        for uid in ["", "bad\nprivate-uid"] {
            assert!(
                conditional_apply_request(
                    &valid,
                    "settings",
                    "workload",
                    "apsides-example",
                    false,
                    ApplyPrecondition::Existing {
                        uid,
                        resource_version: "42",
                    },
                )
                .is_err()
            );
        }
        let malformed = [
            json!({"metadata": []}),
            json!({"metadata":{"name":"other","namespace":"workload"}}),
            json!({"metadata":{"name":"settings","namespace":"other"}}),
        ];
        for desired in &malformed {
            assert!(attempt(desired, "42", "apsides-example").is_err());
        }
        assert!(attempt(&valid, "42", "").is_err());
        let private = "private-version";
        let error = attempt(&valid, &format!("bad\n{private}"), "apsides-example")
            .err()
            .ok_or_else(|| super::super::invalid("invalid private version was accepted"))?
            .to_string();
        assert!(!error.contains(private));
        Ok(())
    }
}
