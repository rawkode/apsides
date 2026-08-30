//! Async production ports. Kubernetes remains behind its sealed typed API and
//! renderer socket I/O is isolated from Tokio's executor.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use async_trait::async_trait;
use futures_util::future::select_all;
use oxideloop_bundle_contract::BundleDescriptorV0;
use oxideloop_controller_api::ObjectIdentity;
use oxideloop_controller_core::{DesiredObservationV0, RecoveryProofV0};
use oxideloop_kube_transport::{
    AutomationCompletionAckV0, EventMaterializationRefV0, KubeTransportV0, ObjectRefV0, ObjectV0,
    ResourceKindV0, WatchEventV0,
};
use oxideloop_mutation_model::{
    BackendQualification, ObjectIdentity as MutationIdentity, Observation as CoreObservation,
};
use oxideloop_renderer_abi::{
    RenderInputV0,
    model::{Observation, RenderOutputV0},
};
use oxideloop_renderer_protocol::{PhaseV0, RequestV0, ResponseV0};
use renderer_client::RendererClient;

use crate::{
    AutomationPortV0, BarrierV0, FixedPointPortV0, ObservationBarrierV0, RendererPortV0,
    RuntimeErrorV0, canonical_render_input, deterministic_request_id, establish_dependencies_async,
    renderer_request,
};

/// Renderer port for an explicitly configured Unix socket. Each call owns a
/// short-lived client, while `spawn_blocking` keeps the deadline-bounded sync
/// protocol implementation off the async executor.
#[derive(Clone, Debug)]
pub struct UnixRendererPortV0 {
    socket: PathBuf,
}

impl UnixRendererPortV0 {
    #[must_use]
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }
}

#[async_trait]
impl RendererPortV0 for UnixRendererPortV0 {
    async fn call(&mut self, request: &RequestV0) -> Result<ResponseV0, RuntimeErrorV0> {
        let socket = self.socket.clone();
        let request = request.clone();
        tokio::task::spawn_blocking(move || {
            #[cfg(unix)]
            {
                let mut client =
                    RendererClient::new_unix(socket).map_err(|_| RuntimeErrorV0::Renderer)?;
                client.call(&request).map_err(|_| RuntimeErrorV0::Renderer)
            }
            #[cfg(not(unix))]
            {
                let _ = socket;
                let _ = request;
                Err(RuntimeErrorV0::Renderer)
            }
        })
        .await
        .map_err(|_| RuntimeErrorV0::Renderer)?
    }
}

/// Typed adapter for Event materialization and `AutomationRun` `/status` only.
pub struct KubeAutomationPortV0<'a> {
    pub transport: &'a KubeTransportV0,
}

#[async_trait]
impl AutomationPortV0 for KubeAutomationPortV0<'_> {
    async fn create_or_exact_recover_event(
        &mut self,
        plan: &oxideloop_event_planner::PlannedEventV0,
        event_time: &str,
    ) -> Result<EventMaterializationRefV0, RuntimeErrorV0> {
        self.transport
            .create_or_exact_recover_event(plan, event_time)
            .await
            .map_err(RuntimeErrorV0::Discovery)
    }
    async fn persist_completion(
        &mut self,
        namespace: &str,
        name: &str,
        expected_uid: &str,
        expected_resource_version: &str,
        expected_spec: &oxideloop_controller_api::AutomationRunSpec,
        status: &oxideloop_controller_api::AutomationRunStatus,
    ) -> Result<AutomationCompletionAckV0, RuntimeErrorV0> {
        self.transport
            .persist_automation_run_completion(
                namespace,
                name,
                expected_uid,
                expected_resource_version,
                expected_spec,
                status,
            )
            .await
            .map_err(RuntimeErrorV0::Discovery)
    }
}

/// Render and discovery adapter. Its descriptor pin is part of every render
/// request; no discovered object can select a different renderer module.
pub struct KubeFixedPointPortV0<'a, R> {
    pub transport: &'a KubeTransportV0,
    pub renderer: R,
    pub descriptor: BundleDescriptorV0,
    pub barrier: BarrierV0,
    retained: Option<RetainedObservationBarrierV0>,
}

impl<'a, R> KubeFixedPointPortV0<'a, R> {
    #[must_use]
    pub fn new(
        transport: &'a KubeTransportV0,
        renderer: R,
        descriptor: BundleDescriptorV0,
        barrier: BarrierV0,
    ) -> Self {
        Self {
            transport,
            renderer,
            descriptor,
            barrier,
            retained: None,
        }
    }

    /// Consume already-buffered dependency changes from the stable barrier.
    /// A caller that receives `Some` must begin another reconciliation using
    /// the returned snapshot. Watches remain retained while the caller renders.
    pub async fn observe_retained_dependency_change(
        &mut self,
    ) -> Result<Option<Vec<Observation>>, RuntimeErrorV0> {
        let Some(retained) = self.retained.as_mut() else {
            return Ok(None);
        };
        if retained.drain_buffered_events().await? {
            Ok(Some(retained.observations()))
        } else {
            Ok(None)
        }
    }

    /// Efficiently waits for the first genuine change on any retained watch,
    /// then drains already-buffered siblings into one deterministic snapshot.
    /// Terminal 410/overflow errors propagate to the epoch restart loop.
    pub async fn await_retained_dependency_change(
        &mut self,
    ) -> Result<Vec<Observation>, RuntimeErrorV0> {
        let retained = self.retained.as_mut().ok_or_else(|| {
            RuntimeErrorV0::Hold("no retained dependency barrier is available".into())
        })?;
        retained.await_change().await?;
        Ok(retained.observations())
    }

    #[must_use]
    pub fn has_retained_observation_barrier(&self) -> bool {
        self.retained.is_some()
    }
}

#[async_trait]
impl<R: RendererPortV0> FixedPointPortV0 for KubeFixedPointPortV0<'_, R> {
    async fn render(&mut self, input: &RenderInputV0) -> Result<RenderOutputV0, RuntimeErrorV0> {
        let payload = canonical_render_input(input)?;
        let hash = oxideloop_renderer_abi::sha256_hex(&payload);
        let request = renderer_request(
            PhaseV0::Render,
            deterministic_request_id(&hash)?,
            &input.bundle_digest,
            Some(self.descriptor.render_module.digest.clone()),
            payload,
        )?;
        let response = self.renderer.call(&request).await?;
        // Keep the parser/correlation rule identical to the direct client path.
        let payload = crate::renderer_payload(&response, &request)?;
        let text = std::str::from_utf8(&payload).map_err(|_| RuntimeErrorV0::RendererAbi)?;
        Ok(oxideloop_renderer_abi::parse_validate_render_output(text)
            .map_err(|_| RuntimeErrorV0::RendererAbi)?
            .document)
    }

    async fn observe_after_barrier(
        &mut self,
        dependencies: &[oxideloop_renderer_abi::model::Dependency],
    ) -> Result<Vec<Observation>, RuntimeErrorV0> {
        let barrier =
            establish_dependencies_async(self.transport, dependencies, self.barrier).await?;
        let mut retained = RetainedObservationBarrierV0::new(barrier);
        retained.drain_buffered_events().await?;
        let observations = retained.observations();
        // The fixed-point loop returns immediately after its stable render.
        // Keep these handles rather than dropping them at that boundary so a
        // later dependency event can trigger the next reconciliation.
        self.retained = Some(retained);
        Ok(observations)
    }

    fn watch_invalidated(&mut self, barrier: BarrierV0) {
        // A terminal watch error makes every snapshot from this barrier stale.
        // Dropping the handles is intentional; the restart establishes a fresh
        // list/watch/reread barrier at the actor's new epoch.
        self.retained = None;
        self.barrier = barrier;
    }
}

type ObservationKeyV0 = (ResourceKindV0, String, String);

enum CoalescedWatchStateV0 {
    Present(ObjectV0),
    Tombstone(ObjectV0),
}

/// The stable barrier's snapshot plus all watch changes seen since its reread.
/// A delete stays a tombstone (JSON null) rather than resurrecting the deleted
/// object from the list snapshot. `BTreeMap` makes cross-dependency output
/// ordering deterministic while the last event per object owns its RV.
struct RetainedObservationBarrierV0 {
    barrier: ObservationBarrierV0,
    latest: BTreeMap<ObservationKeyV0, CoalescedWatchStateV0>,
}

impl RetainedObservationBarrierV0 {
    fn new(barrier: ObservationBarrierV0) -> Self {
        Self {
            barrier,
            latest: BTreeMap::new(),
        }
    }

    async fn drain_buffered_events(&mut self) -> Result<bool, RuntimeErrorV0> {
        let mut changed = false;
        for index in 0..self.barrier.watch_count() {
            loop {
                match tokio::time::timeout(Duration::ZERO, self.barrier.next_event(index)).await {
                    Ok(Ok(event)) => changed |= self.record(event),
                    Ok(Err(error)) => return Err(error),
                    Err(_) => break,
                }
            }
        }
        Ok(changed)
    }

    async fn await_change(&mut self) -> Result<(), RuntimeErrorV0> {
        if self.barrier.watches.is_empty() {
            return Err(RuntimeErrorV0::Hold(
                "retained dependency barrier has no watch handles".into(),
            ));
        }
        loop {
            let pending = self
                .barrier
                .watches
                .iter_mut()
                .enumerate()
                .map(|(index, watch)| Box::pin(async move { (index, watch.next_event().await) }))
                .collect::<Vec<_>>();
            let ((_, event), _, _) = select_all(pending).await;
            let mut changed = self.record(event.map_err(RuntimeErrorV0::Discovery)?);
            changed |= self.drain_buffered_events().await?;
            if changed {
                return Ok(());
            }
        }
    }

    fn record(&mut self, event: WatchEventV0) -> bool {
        match event {
            WatchEventV0::Added(object) | WatchEventV0::Modified(object) => {
                let key = observation_key(&object.reference);
                if self.present_identity(&key).is_some_and(|(uid, rv)| {
                    uid == object.metadata.uid && rv == object.metadata.resource_version
                }) {
                    return false;
                }
                self.latest
                    .insert(key, CoalescedWatchStateV0::Present(object));
                true
            }
            WatchEventV0::Deleted(object) => {
                let key = observation_key(&object.reference);
                if self.latest.get(&key).is_some_and(|state| {
                    matches!(
                        state,
                        CoalescedWatchStateV0::Tombstone(existing)
                            if existing.metadata.uid == object.metadata.uid
                                && existing.metadata.resource_version
                                    == object.metadata.resource_version
                    )
                }) {
                    return false;
                }
                self.latest
                    .insert(key, CoalescedWatchStateV0::Tombstone(object));
                true
            }
            WatchEventV0::Bookmark { .. } => false,
        }
    }

    fn present_identity(&self, key: &ObservationKeyV0) -> Option<(&str, &str)> {
        if let Some(state) = self.latest.get(key) {
            return match state {
                CoalescedWatchStateV0::Present(object) => Some((
                    object.metadata.uid.as_str(),
                    object.metadata.resource_version.as_str(),
                )),
                CoalescedWatchStateV0::Tombstone(_) => None,
            };
        }
        self.barrier
            .collections
            .values()
            .flat_map(|list| &list.items)
            .chain(
                self.barrier
                    .objects
                    .iter()
                    .filter_map(|(_, object)| object.as_ref()),
            )
            .find(|object| observation_key(&object.reference) == *key)
            .map(|object| {
                (
                    object.metadata.uid.as_str(),
                    object.metadata.resource_version.as_str(),
                )
            })
    }

    fn observations(&self) -> Vec<Observation> {
        let mut present = BTreeMap::new();
        let mut absent = BTreeMap::new();
        for list in self.barrier.collections.values() {
            for object in &list.items {
                present.insert(observation_key(&object.reference), object.clone());
            }
        }
        for (reference, object) in &self.barrier.objects {
            if let Some(object) = object {
                present.insert(observation_key(&object.reference), object.clone());
            } else {
                absent.insert(observation_key(reference), absent_observation(reference));
            }
        }
        for (key, state) in &self.latest {
            match state {
                CoalescedWatchStateV0::Present(object) => {
                    absent.remove(key);
                    present.insert(key.clone(), object.clone());
                }
                CoalescedWatchStateV0::Tombstone(object) => {
                    present.remove(key);
                    // Keep the object metadata internally for duplicate-event
                    // suppression, but do not expose deletion pins to a
                    // renderer alongside the JSON null value.
                    absent.insert(key.clone(), tombstone_observation(object));
                }
            }
        }
        let mut observations = present
            .into_values()
            .map(renderer_observation)
            .chain(absent.into_values())
            .collect::<Vec<_>>();
        observations.sort_by(observation_sort_key);
        observations
    }
}

fn renderer_observation(object: ObjectV0) -> Observation {
    let is_present = !object.value.is_null();
    Observation {
        api_version: object.reference.resource.api_version().into(),
        kind: object.reference.resource.kind().into(),
        namespace: object.reference.namespace,
        name: object.reference.name,
        uid: if is_present {
            object.metadata.uid
        } else {
            String::new()
        },
        resource_version: if is_present {
            object.metadata.resource_version
        } else {
            String::new()
        },
        value: object.value,
    }
}

fn observation_key(reference: &ObjectRefV0) -> ObservationKeyV0 {
    (
        reference.resource,
        reference.namespace.clone(),
        reference.name.clone(),
    )
}

fn absent_observation(reference: &ObjectRefV0) -> Observation {
    Observation {
        api_version: reference.resource.api_version().into(),
        kind: reference.resource.kind().into(),
        namespace: reference.namespace.clone(),
        name: reference.name.clone(),
        uid: String::new(),
        resource_version: String::new(),
        value: serde_json::Value::Null,
    }
}

fn tombstone_observation(object: &ObjectV0) -> Observation {
    Observation {
        api_version: object.reference.resource.api_version().into(),
        kind: object.reference.resource.kind().into(),
        namespace: object.reference.namespace.clone(),
        name: object.reference.name.clone(),
        uid: String::new(),
        resource_version: String::new(),
        value: serde_json::Value::Null,
    }
}

fn observation_sort_key(left: &Observation, right: &Observation) -> std::cmp::Ordering {
    (&left.api_version, &left.kind, &left.namespace, &left.name).cmp(&(
        &right.api_version,
        &right.kind,
        &right.namespace,
        &right.name,
    ))
}

/// Converts exact Kubernetes observations for reducer reconciliation. Only
/// built-in ConfigMap/Service objects qualify at Kubernetes v1.34. Recovery
/// proof is issued only for one exact controller owner reference plus all
/// proof annotations; a same-name foreign object therefore has no adoption
/// path.
pub fn desired_observation(
    root: &ObjectRefV0,
    root_uid: &str,
    object: &ObjectV0,
) -> Result<DesiredObservationV0, RuntimeErrorV0> {
    if root.resource != ResourceKindV0::WebApplication
        || root.namespace.is_empty()
        || root.name.is_empty()
        || root_uid.is_empty()
    {
        return Err(RuntimeErrorV0::Hold(
            "root ownership identity is invalid".into(),
        ));
    }
    let gvr = match object.reference.resource {
        ResourceKindV0::ConfigMap => "v1/configmaps",
        ResourceKindV0::Service => "v1/services",
        _ => return Err(RuntimeErrorV0::Hold("unsupported observed resource".into())),
    };
    let recovery_proof = match exact_recovery_annotations(object, root_uid) {
        Some((transaction_id, command_id)) if exact_controller_owner(object, root, root_uid) => {
            Some(RecoveryProofV0 {
                root_uid: root_uid.into(),
                transaction_id,
                command_id,
                uid: object.metadata.uid.clone(),
                resource_version: object.metadata.resource_version.clone(),
            })
        }
        _ => None,
    };
    Ok(DesiredObservationV0 {
        identity: ObjectIdentity {
            api_version: object.reference.resource.api_version().into(),
            kind: object.reference.resource.kind().into(),
            namespace: object.reference.namespace.clone(),
            name: object.reference.name.clone(),
        },
        observation: CoreObservation::Existing {
            identity: MutationIdentity {
                uid: object.metadata.uid.clone(),
                resource_version: object.metadata.resource_version.clone(),
            },
        },
        qualification: BackendQualification::BuiltInTested {
            kubernetes_minor: "1.34".into(),
            gvr: gvr.into(),
        },
        recovery_proof,
        deletion_recovery_proof: None,
    })
}

fn exact_recovery_annotations(object: &ObjectV0, root_uid: &str) -> Option<(String, String)> {
    let annotations = object.value.pointer("/metadata/annotations")?.as_object()?;
    let root = annotations
        .get(oxideloop_kube_transport::ROOT_UID_ANNOTATION)?
        .as_str()?;
    let transaction_id = annotations
        .get(oxideloop_kube_transport::TRANSACTION_ID_ANNOTATION)?
        .as_str()?;
    let command_id = annotations
        .get(oxideloop_kube_transport::COMMAND_ID_ANNOTATION)?
        .as_str()?;
    if root != root_uid
        || object
            .metadata
            .annotations
            .get(oxideloop_kube_transport::ROOT_UID_ANNOTATION)
            .is_none_or(|value| value != root)
        || object
            .metadata
            .annotations
            .get(oxideloop_kube_transport::TRANSACTION_ID_ANNOTATION)
            .is_none_or(|value| value != transaction_id)
        || object
            .metadata
            .annotations
            .get(oxideloop_kube_transport::COMMAND_ID_ANNOTATION)
            .is_none_or(|value| value != command_id)
    {
        return None;
    }
    Some((transaction_id.into(), command_id.into()))
}

fn exact_controller_owner(object: &ObjectV0, root: &ObjectRefV0, root_uid: &str) -> bool {
    object
        .value
        .pointer("/metadata/ownerReferences")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|owners| {
            owners.len() == 1
                && owners[0].as_object().is_some_and(|owner| {
                    owner.get("apiVersion").and_then(serde_json::Value::as_str)
                        == Some(root.resource.api_version())
                        && owner.get("kind").and_then(serde_json::Value::as_str)
                            == Some(root.resource.kind())
                        && owner.get("name").and_then(serde_json::Value::as_str)
                            == Some(root.name.as_str())
                        && owner.get("uid").and_then(serde_json::Value::as_str) == Some(root_uid)
                        && owner.get("controller").and_then(serde_json::Value::as_bool)
                            == Some(true)
                        && owner
                            .get("blockOwnerDeletion")
                            .and_then(serde_json::Value::as_bool)
                            == Some(false)
                })
        })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use oxideloop_kube_transport::{ListV0, ObjectMetaV0};
    use serde_json::json;

    use super::*;

    fn reference(name: &str) -> ObjectRefV0 {
        ObjectRefV0 {
            resource: ResourceKindV0::ConfigMap,
            namespace: "ns".into(),
            name: name.into(),
        }
    }

    fn root() -> ObjectRefV0 {
        ObjectRefV0 {
            resource: ResourceKindV0::WebApplication,
            namespace: "ns".into(),
            name: "root".into(),
        }
    }

    fn object(name: &str, uid: &str, resource_version: &str) -> ObjectV0 {
        ObjectV0 {
            reference: reference(name),
            metadata: ObjectMetaV0 {
                uid: uid.into(),
                resource_version: resource_version.into(),
                annotations: BTreeMap::new(),
                owner_uids: vec!["root-uid".into()],
            },
            value: json!({
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {"name": name, "namespace": "ns"},
                "data": {"rv": resource_version},
            }),
        }
    }

    fn retained_with(object: ObjectV0) -> RetainedObservationBarrierV0 {
        let mut collections = BTreeMap::new();
        collections.insert(
            (ResourceKindV0::ConfigMap, "ns".into()),
            ListV0 {
                items: vec![object],
                resource_version: "1".into(),
            },
        );
        RetainedObservationBarrierV0::new(ObservationBarrierV0 {
            generation: BarrierV0 {
                generation: 1,
                cache_epoch: 4,
            },
            collections,
            objects: vec![],
            watches: vec![],
        })
    }

    #[test]
    fn retained_snapshot_survives_stable_render_and_dependency_changes() {
        let mut retained = retained_with(object("config", "uid-1", "1"));
        let stable = retained.observations();
        assert_eq!(stable.len(), 1);
        assert_eq!(stable[0].resource_version, "1");

        assert!(retained.record(WatchEventV0::Modified(object("config", "uid-1", "2"))));
        let changed = retained.observations();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].resource_version, "2");
        assert_eq!(changed[0].value["data"]["rv"], "2");
    }

    #[test]
    fn retained_watch_ignores_duplicate_uid_rv_but_admits_a_genuine_change() {
        let mut retained = retained_with(object("config", "uid-1", "1"));
        assert!(!retained.record(WatchEventV0::Modified(object("config", "uid-1", "1"))));
        assert!(retained.record(WatchEventV0::Modified(object("config", "uid-1", "2"))));
        assert!(!retained.record(WatchEventV0::Modified(object("config", "uid-1", "2"))));
        assert!(retained.record(WatchEventV0::Deleted(object("config", "uid-1", "3"))));
        assert!(!retained.record(WatchEventV0::Deleted(object("config", "uid-1", "3"))));
    }

    #[test]
    fn coalescing_uses_last_event_and_deleted_is_an_unpinned_tombstone() {
        let mut retained = retained_with(object("config", "uid-1", "1"));
        retained.record(WatchEventV0::Added(object("config", "uid-1", "2")));
        retained.record(WatchEventV0::Modified(object("config", "uid-1", "3")));
        retained.record(WatchEventV0::Deleted(object("config", "uid-1", "4")));

        let observations = retained.observations();
        assert_eq!(observations.len(), 1);
        assert!(observations[0].uid.is_empty());
        assert!(observations[0].resource_version.is_empty());
        assert_eq!(observations[0].value, serde_json::Value::Null);
    }

    #[test]
    fn tombstone_pins_are_absent_from_raw_renderer_input() {
        let mut retained = retained_with(object("config", "uid-1", "1"));
        retained.record(WatchEventV0::Deleted(object("config", "uid-1", "4")));
        let observations = retained.observations();
        let input = RenderInputV0 {
            abi_version: oxideloop_renderer_abi::model::ABI_VERSION_V0.into(),
            root: oxideloop_renderer_abi::model::RootProjection {
                api_version: "demo.oxideloop.dev/v1alpha1".into(),
                kind: "WebApplication".into(),
                namespace: "ns".into(),
                name: "root".into(),
                uid: "root-uid".into(),
                generation: 1,
                spec: json!({}),
            },
            bundle_digest: "a".repeat(64),
            cache_epoch: "1".into(),
            observation_token: "token".into(),
            observations,
        };
        let bytes = crate::canonical_render_input(&input).expect("valid raw renderer input");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON input");
        let observation = &value["observations"][0];
        assert_eq!(observation["value"], serde_json::Value::Null);
        assert_eq!(observation["uid"], "");
        assert_eq!(observation["resourceVersion"], "");
    }

    #[test]
    fn coalescing_delete_then_add_restores_the_latest_present_object() {
        let mut retained = retained_with(object("config", "uid-1", "1"));
        retained.record(WatchEventV0::Deleted(object("config", "uid-1", "2")));
        retained.record(WatchEventV0::Added(object("config", "uid-2", "3")));

        let observations = retained.observations();
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].uid, "uid-2");
        assert_eq!(observations[0].resource_version, "3");
        assert_eq!(observations[0].value["data"]["rv"], "3");
    }

    #[test]
    fn absent_nominated_object_is_explicit_null_without_identity_pins() {
        let retained = RetainedObservationBarrierV0::new(ObservationBarrierV0 {
            generation: BarrierV0 {
                generation: 1,
                cache_epoch: 0,
            },
            collections: BTreeMap::new(),
            objects: vec![(reference("missing"), None)],
            watches: vec![],
        });
        assert_eq!(
            retained.observations(),
            vec![Observation {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "ns".into(),
                name: "missing".into(),
                uid: String::new(),
                resource_version: String::new(),
                value: serde_json::Value::Null,
            }]
        );
    }

    fn owned_proven_object() -> ObjectV0 {
        let mut object = object("owned", "child-uid", "7");
        let annotations = BTreeMap::from([
            (
                oxideloop_kube_transport::ROOT_UID_ANNOTATION.into(),
                "root-uid".into(),
            ),
            (
                oxideloop_kube_transport::TRANSACTION_ID_ANNOTATION.into(),
                "tx-1".into(),
            ),
            (
                oxideloop_kube_transport::COMMAND_ID_ANNOTATION.into(),
                "command-1".into(),
            ),
        ]);
        object.metadata.annotations = annotations.clone();
        object.value["metadata"]["annotations"] = serde_json::to_value(annotations).unwrap();
        object.value["metadata"]["ownerReferences"] = json!([{
            "apiVersion": "demo.oxideloop.dev/v1alpha1",
            "kind": "WebApplication",
            "name": "root",
            "uid": "root-uid",
            "controller": true,
            "blockOwnerDeletion": false,
        }]);
        object
    }

    #[test]
    fn desired_observation_requires_exact_owner_and_proof_annotations() {
        let object = owned_proven_object();
        let desired = desired_observation(&root(), "root-uid", &object).unwrap();
        assert_eq!(
            desired.qualification,
            BackendQualification::BuiltInTested {
                kubernetes_minor: "1.34".into(),
                gvr: "v1/configmaps".into(),
            }
        );
        assert_eq!(desired.recovery_proof.unwrap().transaction_id, "tx-1");
    }

    #[test]
    fn ambiguous_owner_or_proof_never_adopts() {
        let mut extra_owner = owned_proven_object();
        extra_owner.value["metadata"]["ownerReferences"]
            .as_array_mut()
            .unwrap()
            .push(json!({"uid": "foreign"}));
        assert!(
            desired_observation(&root(), "root-uid", &extra_owner)
                .unwrap()
                .recovery_proof
                .is_none()
        );

        let mut blocking_owner = owned_proven_object();
        blocking_owner.value["metadata"]["ownerReferences"][0]["blockOwnerDeletion"] = json!(true);
        assert!(
            desired_observation(&root(), "root-uid", &blocking_owner)
                .unwrap()
                .recovery_proof
                .is_none()
        );

        let mut mismatched_proof = owned_proven_object();
        mismatched_proof.metadata.annotations.insert(
            oxideloop_kube_transport::COMMAND_ID_ANNOTATION.into(),
            "different".into(),
        );
        assert!(
            desired_observation(&root(), "root-uid", &mismatched_proof)
                .unwrap()
                .recovery_proof
                .is_none()
        );
    }
}
