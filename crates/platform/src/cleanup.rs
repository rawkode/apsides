use crate::store::replace_state;
use crate::{NamespaceRegistry, PlatformClient, Result, Versioned, invalid};
#[cfg(test)]
use compiler_core::Resource;
use deploy::{ClaimState, Phase, ResourceIdentity};
use kube::{
    Api,
    api::{ApiResource, DynamicObject},
    core::GroupVersionKind,
};

impl PlatformClient {
    /// Narrow existing protected claims to independently observed object UIDs.
    /// This is a planning snapshot, not writer termination or deletion proof.
    /// Runtime-written inventory never supplies a UID to this operation.
    /// # Errors
    /// Rejects incomplete ownership, replacements, concurrent transitions or CAS changes.
    pub async fn capture_active_claim_uids(
        &self,
        domain_id: &str,
        inventory: &inventory::InventorySnapshot,
    ) -> Result<Versioned<NamespaceRegistry>> {
        let current = self.load_registry().await?;
        if current.value.transitions.values().any(|s| {
            s.plan.request.next.compiled.ir.domain.id == domain_id
                && s.record.phase != Phase::Committed
        }) {
            return Err(invalid(
                "cannot capture active claims during an unfinished transition",
            ));
        }
        let generation = current
            .value
            .histories
            .get(domain_id)
            .and_then(|h| h.active_generation_id.as_ref())
            .ok_or_else(|| invalid("domain has no active generation"))?;
        let stored = current
            .value
            .transitions
            .values()
            .find(|s| {
                s.record.phase == Phase::Committed
                    && s.plan.request.next.generation_id == *generation
                    && s.plan.request.next.compiled.ir.domain.id == domain_id
            })
            .ok_or_else(|| invalid("active generation lacks its committed approved plan"))?;
        let scope = inventory::InventoryScope {
            cluster: self.cluster_binding.clone(),
            namespace: self.workload_namespace.clone(),
            controller: domain_id.into(),
        };
        validate_active_inventory(&scope, inventory, generation, &current.value.claims)?;
        let mut next = current.value.clone();
        for resource in &inventory.resources {
            let identity = ResourceIdentity {
                api_group: resource.identity.api_group.clone(),
                resource: resource.identity.resource.clone(),
                namespace: resource.identity.namespace.clone(),
                name: resource.identity.name.clone(),
            };
            let matching: Vec<_> = next
                .claims
                .iter_mut()
                .filter(|c| c.identity == identity)
                .collect();
            if matching.len() != 1 {
                return Err(invalid(
                    "active resource must have exactly one protected claim",
                ));
            }
            let claim = matching
                .into_iter()
                .next()
                .ok_or_else(|| invalid("claim absent"))?;
            if claim.domain_id != domain_id
                || claim.generation_id != *generation
                || claim.transition_id != stored.plan.request.transition_id
                || claim.state != ClaimState::Active
                || resource
                    .uid
                    .as_ref()
                    .is_some_and(|uid| claim.uid.as_ref() != Some(uid))
            {
                return Err(invalid("active resource claim belongs to another owner"));
            }
            let api = metadata_api(
                &self.client,
                &identity,
                &resource.api_version,
                &resource.kind,
            )?;
            if let Some(object) = api.get_opt(&resource.identity.name).await? {
                if object.metadata.deletion_timestamp.is_some() {
                    return Err(invalid("active UID capture refuses a deleting object"));
                }
                let uid = owned_metadata_uid(&object, &identity, &resource.api_version, domain_id)?;
                if claim.uid.as_ref().is_some_and(|expected| expected != &uid) {
                    return Err(invalid(
                        "live resource is a replacement of the protected UID",
                    ));
                }
                claim.uid = Some(uid);
            }
        }
        replace_state(
            &self.client,
            &self.control_namespace,
            &format!("reakt-registry-{}", self.workload_namespace),
            &current,
            &next,
        )
        .await
    }
}

fn validate_active_inventory(
    scope: &inventory::InventoryScope,
    inventory: &inventory::InventorySnapshot,
    generation: &str,
    claims: &[deploy::ProtectedClaim],
) -> Result<()> {
    inventory
        .encode(scope)
        .map_err(|_| invalid("invalid active inventory metadata"))?;
    if inventory.generation != generation {
        return Err(invalid(
            "active inventory generation differs from protected history",
        ));
    }
    let identities: std::collections::BTreeSet<_> = inventory
        .resources
        .iter()
        .map(|resource| ResourceIdentity {
            api_group: resource.identity.api_group.clone(),
            resource: resource.identity.resource.clone(),
            namespace: resource.identity.namespace.clone(),
            name: resource.identity.name.clone(),
        })
        .collect();
    if claims.iter().any(|claim| {
        claim.domain_id == scope.controller
            && claim.generation_id == generation
            && !identities.contains(&claim.identity)
    }) {
        return Err(invalid("active inventory omits a protected resource"));
    }
    Ok(())
}

#[cfg(test)]
fn resource_identity(resource: &Resource) -> Result<ResourceIdentity> {
    let (group, plural) = match (resource.api_version.as_str(), resource.kind.as_str()) {
        ("v1", "ConfigMap") => ("", "configmaps"),
        ("v1", "Service") => ("", "services"),
        ("apps/v1", "Deployment") => ("apps", "deployments"),
        _ => return Err(invalid("unsupported cleanup resource")),
    };
    Ok(ResourceIdentity {
        api_group: group.into(),
        resource: plural.into(),
        namespace: resource.namespace.clone(),
        name: resource.name.clone(),
    })
}

fn metadata_api(
    client: &kube::Client,
    identity: &ResourceIdentity,
    api_version: &str,
    kind: &str,
) -> Result<Api<DynamicObject>> {
    let (group, version) = api_version.split_once('/').unwrap_or(("", api_version));
    if group != identity.api_group
        || !matches!(
            (api_version, kind, identity.resource.as_str()),
            ("v1", "ConfigMap", "configmaps")
                | ("v1", "Service", "services")
                | ("apps/v1", "Deployment", "deployments")
        )
    {
        return Err(invalid("unsupported cleanup API identity"));
    }
    let ar = ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk(group, version, kind),
        &identity.resource,
    );
    Ok(Api::namespaced_with(
        client.clone(),
        &identity.namespace,
        &ar,
    ))
}

#[cfg(test)]
fn owned_uid(object: &DynamicObject, resource: &Resource, domain: &str) -> Result<String> {
    owned_metadata_uid(
        object,
        &resource_identity(resource)?,
        &resource.api_version,
        domain,
    )
}

fn owned_metadata_uid(
    object: &DynamicObject,
    identity: &ResourceIdentity,
    api_version: &str,
    domain: &str,
) -> Result<String> {
    let manager = format!("reakt-{domain}");
    let entries: Vec<_> = object
        .metadata
        .managed_fields
        .iter()
        .flatten()
        .filter(|e| {
            e.manager.as_deref() == Some(&manager)
                && e.subresource.as_deref().is_none_or(str::is_empty)
        })
        .collect();
    if object.metadata.name.as_deref() != Some(&identity.name)
        || object.metadata.namespace.as_deref() != Some(&identity.namespace)
        || object
            .metadata
            .resource_version
            .as_deref()
            .is_none_or(|s| s.is_empty() || s == "0")
        || !matches!(entries.as_slice(), [entry]
            if entry.operation.as_deref() == Some("Apply")
            && entry.api_version.as_deref() == Some(api_version)
            && entry.fields_type.as_deref() == Some("FieldsV1")
            && entry.fields_v1.is_some())
    {
        return Err(invalid(
            "live cleanup candidate lacks exact approved Apply ownership",
        ));
    }
    object
        .metadata
        .uid
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("cleanup candidate lacks UID"))
}

/// One bounded cleanup pass. Pending objects retain their protected claims.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupProgress {
    pub completed: Vec<deploy::CleanupReceipt>,
    pub pending: Vec<ResourceIdentity>,
}

impl PlatformClient {
    /// Perform only removals authorized by the protected Cleaning transition.
    /// This never strips finalizers and never deletes a replacement object.
    /// # Errors
    /// Fails closed on ownership, policy, identity or transition changes.
    pub async fn execute_cleanup(
        &self,
        transition_id: &str,
        executor_id: &str,
    ) -> Result<CleanupProgress> {
        let registry = self.load_registry().await?;
        let stored = registry
            .value
            .transitions
            .get(transition_id)
            .ok_or_else(|| invalid("unknown cleanup transition"))?;
        if stored.record.executor_id != executor_id || stored.record.phase != Phase::Cleaning {
            return Err(invalid("cleanup requires the owning executor in Cleaning"));
        }
        if stored.plan.request.previous.is_none() {
            return Err(invalid("cleanup requires an approved previous generation"));
        }
        let domain_id = &stored.plan.request.next.compiled.ir.domain.id;
        let policy = self.load_policy(domain_id).await?;
        if policy.value.cleanup_ceiling != stored.plan.request.cleanup_ceiling {
            return Err(invalid("cleanup ceiling changed since transition approval"));
        }
        let mut progress = CleanupProgress {
            completed: vec![],
            pending: vec![],
        };
        for item in &stored.plan.cleanup {
            let claims: Vec<_> = registry
                .value
                .claims
                .iter()
                .filter(|c| c.identity == item.identity)
                .collect();
            let [claim] = claims.as_slice() else {
                return Err(invalid("cleanup requires one exact protected claim"));
            };
            let api = metadata_api(&self.client, &item.identity, &item.api_version, &item.kind)?;
            let live = if item.policy == compiler_core::DeletionPolicy::Orphan {
                None // Orphaning releases a claim without reading or mutating the object.
            } else {
                api.get_opt(&item.identity.name).await?
            };
            let observation = match &live {
                None => deploy::LiveCleanupObject::Absent,
                Some(object) => deploy::LiveCleanupObject::Present {
                    uid: owned_metadata_uid(object, &item.identity, &item.api_version, domain_id)?,
                    finalizers: object.metadata.finalizers.clone().unwrap_or_default(),
                    deletion_started: object.metadata.deletion_timestamp.is_some(),
                },
            };
            let decision = deploy::cleanup_decision(
                &stored.plan,
                &stored.record,
                &item.identity,
                claim,
                &observation,
            )?;
            let outcome = match decision {
                deploy::CleanupDecision::ReleaseOrphanClaim => {
                    Some(deploy::CleanupOutcome::OrphanReleased)
                }
                deploy::CleanupDecision::AlreadyAbsent => {
                    Some(deploy::CleanupOutcome::AlreadyAbsent)
                }
                deploy::CleanupDecision::WaitForFinalizers => None,
                deploy::CleanupDecision::DeleteWithUidPrecondition { .. } => {
                    self.delete_cleanup_object(
                        &registry,
                        &policy,
                        &api,
                        live.as_ref()
                            .ok_or_else(|| invalid("delete lacks live identity"))?,
                    )
                    .await?
                }
            };
            if let Some(outcome) = outcome {
                let evidence_digest = compiler_core::digest(&(
                    &stored.plan.plan_digest,
                    &registry.uid,
                    &registry.resource_version,
                    item,
                    &outcome,
                ))?;
                progress.completed.push(deploy::CleanupReceipt {
                    identity: item.identity.clone(),
                    expected_uid: item.expected_uid.clone(),
                    outcome,
                    evidence_digest,
                });
            } else {
                progress.pending.push(item.identity.clone());
            }
        }
        Ok(progress)
    }
}

impl PlatformClient {
    async fn delete_cleanup_object(
        &self,
        registry: &Versioned<NamespaceRegistry>,
        policy: &Versioned<crate::PolicyState>,
        api: &Api<DynamicObject>,
        live: &DynamicObject,
    ) -> Result<Option<deploy::CleanupOutcome>> {
        let fresh = self.load_registry().await?;
        if fresh.uid != registry.uid || fresh.resource_version != registry.resource_version {
            return Err(invalid("cleanup transition changed before deletion"));
        }
        let current_policy = self
            .load_policy(&policy.value.cleanup_ceiling.domain_id)
            .await?;
        if current_policy.uid != policy.uid
            || current_policy.resource_version != policy.resource_version
        {
            return Err(invalid("cleanup policy changed before deletion"));
        }
        let name = live
            .metadata
            .name
            .as_deref()
            .ok_or_else(|| invalid("cleanup name absent"))?;
        let params = kube::api::DeleteParams {
            preconditions: Some(kube::api::Preconditions {
                uid: live.metadata.uid.clone(),
                resource_version: live.metadata.resource_version.clone(),
            }),
            ..Default::default()
        };
        api.delete(name, &params).await?;
        match api.get_opt(name).await? {
            None => Ok(Some(deploy::CleanupOutcome::Deleted)),
            Some(after) if after.metadata.uid != live.metadata.uid => Err(invalid(
                "replacement appeared during cleanup; claim retained",
            )),
            Some(_) => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn delete_preserves_finalizers_and_sends_both_preconditions() {
        use http::{Request, Response};
        use http_body_util::BodyExt;
        use kube::client::Body;
        use std::{
            collections::VecDeque,
            convert::Infallible,
            sync::{Arc, Mutex},
        };
        let registry = crate::Versioned {
            value: NamespaceRegistry {
                schema_version: crate::SCHEMA_VERSION.into(),
                cluster_binding: "cluster-a".into(),
                workload_namespace: "workload".into(),
                claims: vec![],
                histories: std::collections::BTreeMap::default(),
                transitions: std::collections::BTreeMap::default(),
            },
            uid: "registry-uid".into(),
            resource_version: "7".into(),
        };
        let ceiling: compiler_core::PermissionCeiling = serde_json::from_value(json!({"policyRef":"platform","domainId":"example","clusterBinding":"cluster-a","workloadNamespace":"workload","controlNamespace":"control","capabilities":[],"acceptedEnforcementGaps":[],"acceptedIndirectRisks":[]})).unwrap();
        let policy = crate::Versioned {
            value: crate::PolicyState {
                schema_version: crate::SCHEMA_VERSION.into(),
                runtime_ceiling: ceiling.clone(),
                cleanup_ceiling: ceiling,
            },
            uid: "policy-uid".into(),
            resource_version: "9".into(),
        };
        let live: DynamicObject = serde_json::from_value(json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"settings","namespace":"workload","uid":"target-uid","resourceVersion":"42","finalizers":["example.test/hold"]}})).unwrap();
        let replies = Arc::new(Mutex::new(VecDeque::from([
            json!({"metadata":{"uid":registry.uid,"resourceVersion":registry.resource_version},"data":{"state.json":serde_json::to_string(&registry.value).unwrap()}}),
            json!({"metadata":{"uid":policy.uid,"resourceVersion":policy.resource_version},"data":{"state.json":serde_json::to_string(&policy.value).unwrap()}}),
            serde_json::to_value(&live).unwrap(),
            serde_json::to_value(&live).unwrap(),
        ])));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let service = tower::service_fn(move |request: Request<Body>| {
            let replies = replies.clone();
            let captured = captured.clone();
            async move {
                let (parts, body) = request.into_parts();
                let bytes = body.collect().await.unwrap().to_bytes();
                captured.lock().unwrap().push((
                    parts.method.to_string(),
                    if bytes.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::from_slice(&bytes).unwrap()
                    },
                ));
                let reply = replies.lock().unwrap().pop_front().unwrap();
                Ok::<_, Infallible>(
                    Response::builder()
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&reply).unwrap()))
                        .unwrap(),
                )
            }
        });
        let client = kube::Client::new(service, "control");
        let identity = ResourceIdentity {
            api_group: String::new(),
            resource: "configmaps".into(),
            namespace: "workload".into(),
            name: "settings".into(),
        };
        assert!(metadata_api(&client, &identity, "apps/v1", "ConfigMap").is_err());
        assert!(metadata_api(&client, &identity, "v1", "Secret").is_err());
        let platform =
            PlatformClient::new(client.clone(), "control", "workload", "cluster-a").unwrap();
        let ar = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk("", "v1", "ConfigMap"),
            "configmaps",
        );
        let api = Api::namespaced_with(client, "workload", &ar);
        assert!(
            platform
                .delete_cleanup_object(&registry, &policy, &api, &live)
                .await
                .unwrap()
                .is_none()
        );
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["GET", "GET", "DELETE", "GET"]
        );
        assert_eq!(
            requests[2].1["preconditions"],
            json!({"uid":"target-uid","resourceVersion":"42"})
        );
        assert!(requests[2].1.get("finalizers").is_none());
    }

    #[test]
    fn trusted_capture_requires_exact_main_resource_apply_owner() {
        let resource: Resource = serde_json::from_value(json!({"id":"settings","apiVersion":"v1","kind":"ConfigMap","name":"settings","namespace":"workload","desired":{},"lifecycle":{"deletionPolicy":"delete"},"source":{"file":"test.ts","line":1,"column":1}})).unwrap();
        let value = json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"settings","namespace":"workload","uid":"created-uid","resourceVersion":"19","managedFields":[{"manager":"reakt-example","operation":"Apply","apiVersion":"v1","fieldsType":"FieldsV1","fieldsV1":{"f:data":{}}}]}});
        let object: DynamicObject = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            owned_uid(&object, &resource, "example").unwrap(),
            "created-uid"
        );
        for (field, replacement) in [
            ("operation", json!("Update")),
            ("manager", json!("other")),
            ("apiVersion", json!("v2")),
            ("subresource", json!("status")),
        ] {
            let mut changed = value.clone();
            changed["metadata"]["managedFields"][0][field] = replacement;
            assert!(
                owned_uid(
                    &serde_json::from_value(changed).unwrap(),
                    &resource,
                    "example"
                )
                .is_err()
            );
        }
        let mut duplicate = value;
        let entry = duplicate["metadata"]["managedFields"][0].clone();
        duplicate["metadata"]["managedFields"]
            .as_array_mut()
            .unwrap()
            .push(entry);
        assert!(
            owned_uid(
                &serde_json::from_value(duplicate).unwrap(),
                &resource,
                "example"
            )
            .is_err()
        );
    }
}
