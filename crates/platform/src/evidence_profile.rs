//! Capacity-only bounds for protected drain evidence. Validation here does not
//! establish that an observation came from Kubernetes or grant lifecycle authority.

use crate::{ExpectedWriters, Result, UpgradeDrainState, invalid};
use std::collections::BTreeSet;

pub const EVIDENCE_PROFILE_SCHEMA: &str = "apsides.dev/evidence-profile/v1";
pub const MAX_CONTROLLER_PODS: u16 = 64;
pub const MAX_DRAIN_BYTES: u32 = 128 * 1024;

const MAX_RAW_SCALAR_BYTES: usize = 1024;
const UID_OR_RV_TIERS: [u16; 3] = [128, 512, 2050];
const CONTAINER_ID_TIERS: [u16; 3] = [256, 1024, 2050];

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EvidenceProfile {
    pub schema_version: String,
    pub max_controller_pods: u16,
    pub max_regular_containers: u16,
    pub max_init_containers: u16,
    pub max_ephemeral_containers: u16,
    pub max_uid_json_bytes: u16,
    pub max_resource_version_json_bytes: u16,
    pub max_container_id_json_bytes: u16,
    pub max_drain_bytes: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapturedContainer {
    pub name: String,
    pub container_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapturedContainers {
    pub regular: Vec<CapturedContainer>,
    pub init: Vec<CapturedContainer>,
    pub ephemeral: Vec<CapturedContainer>,
}

impl EvidenceProfile {
    /// Validate the finite capacity profile without treating it as evidence.
    /// # Errors
    /// Rejects unsupported tiers, cardinality overflow, or a scalar envelope
    /// that cannot fit within the admitted complete drain byte ceiling.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != EVIDENCE_PROFILE_SCHEMA
            || !(1..=MAX_CONTROLLER_PODS).contains(&self.max_controller_pods)
            || !UID_OR_RV_TIERS.contains(&self.max_uid_json_bytes)
            || !UID_OR_RV_TIERS.contains(&self.max_resource_version_json_bytes)
            || !CONTAINER_ID_TIERS.contains(&self.max_container_id_json_bytes)
            || self.max_drain_bytes == 0
            || self.max_drain_bytes > MAX_DRAIN_BYTES
            || self.max_regular_containers < self.max_controller_pods
        {
            return Err(invalid("unsupported drain evidence profile"));
        }
        let containers = self.max_container_count()?;
        if containers == 0
            || self
                .minimum_complete_scalar_bytes(containers)?
                .gt(&usize::try_from(self.max_drain_bytes)
                    .map_err(|_| invalid("drain evidence byte ceiling invalid"))?)
        {
            return Err(invalid(
                "drain evidence profile exceeds its complete byte ceiling",
            ));
        }
        Ok(())
    }

    /// Whether this valid profile contains every allowance in another valid profile.
    #[must_use]
    pub fn contains(&self, older: &Self) -> bool {
        self.validate().is_ok()
            && older.validate().is_ok()
            && self.max_controller_pods >= older.max_controller_pods
            && self.max_regular_containers >= older.max_regular_containers
            && self.max_init_containers >= older.max_init_containers
            && self.max_ephemeral_containers >= older.max_ephemeral_containers
            && self.max_uid_json_bytes >= older.max_uid_json_bytes
            && self.max_resource_version_json_bytes >= older.max_resource_version_json_bytes
            && self.max_container_id_json_bytes >= older.max_container_id_json_bytes
            && self.max_drain_bytes >= older.max_drain_bytes
    }

    /// Validate the protected controller writer-set shape against this profile.
    /// This checks capacity and identity syntax only; live provenance remains a
    /// separate platform operation.
    /// # Errors
    /// Rejects empty, excessive, duplicate, malformed or over-tier identities.
    pub fn validate_writers(&self, writers: &ExpectedWriters) -> Result<()> {
        self.validate()?;
        validate_writer_set(self, writers, 1, usize::from(self.max_controller_pods))?;
        let mut regular = 0usize;
        let mut init = 0usize;
        let mut ephemeral = 0usize;
        let mut container_ids = BTreeSet::new();
        for pod in &writers.pods {
            self.validate_container_topology(&pod.containers)?;
            if pod
                .containers
                .regular
                .iter()
                .chain(&pod.containers.init)
                .chain(&pod.containers.ephemeral)
                .any(|container| !container_ids.insert(&container.container_id))
            {
                return Err(invalid(
                    "controller container identity duplicated across Pods",
                ));
            }
            regular = regular
                .checked_add(pod.containers.regular.len())
                .ok_or_else(|| invalid("controller regular container count overflow"))?;
            init = init
                .checked_add(pod.containers.init.len())
                .ok_or_else(|| invalid("controller init container count overflow"))?;
            ephemeral = ephemeral
                .checked_add(pod.containers.ephemeral.len())
                .ok_or_else(|| invalid("controller ephemeral container count overflow"))?;
        }
        if regular > usize::from(self.max_regular_containers)
            || init > usize::from(self.max_init_containers)
            || ephemeral > usize::from(self.max_ephemeral_containers)
        {
            return Err(invalid(
                "controller container topology exceeds admitted profile",
            ));
        }
        Ok(())
    }

    /// Validate one captured Pod's typed container topology and encoded IDs.
    /// Category maxima remain totals across the complete writer set and are
    /// enforced by [`Self::validate_writers`].
    /// # Errors
    /// Rejects empty regular sets, malformed names or IDs, and duplicate names
    /// or identities across any container category.
    pub fn validate_container_topology(&self, topology: &CapturedContainers) -> Result<()> {
        self.validate()?;
        validate_topology(self, topology)
    }

    /// Validate one UID against the admitted raw and canonical JSON tier.
    /// # Errors
    /// Rejects empty, non-graphic, oversized or over-tier values.
    pub fn validate_uid(&self, value: &str) -> Result<()> {
        self.validate()?;
        validate_uid(self, value)
    }

    /// Validate one resourceVersion against the admitted raw and JSON tier.
    /// # Errors
    /// Rejects empty, non-graphic, oversized or over-tier values.
    pub fn validate_resource_version(&self, value: &str) -> Result<()> {
        self.validate()?;
        validate_resource_version(self, value)
    }

    /// Validate one container ID against the admitted raw and JSON tier.
    /// # Errors
    /// Rejects empty, non-graphic, oversized or over-tier values.
    pub fn validate_container_id(&self, value: &str) -> Result<()> {
        self.validate()?;
        validate_container_id(self, value)
    }

    /// Validate a typed drain checkpoint against its admitted structural and
    /// canonical-byte bounds. Missing optional observations remain valid while
    /// the checkpoint is being populated.
    /// # Errors
    /// Rejects unreachable partial shapes, malformed identities or evidence,
    /// excessive cardinality, duplicates, cross-object mismatches, and encoded
    /// drain content beyond the admitted ceiling.
    pub fn validate_drain(&self, drain: &UpgradeDrainState) -> Result<()> {
        self.validate()?;
        if !crate::valid_name(&drain.previous_transition_id) {
            return Err(invalid("drain predecessor identity malformed"));
        }
        self.validate_writers(&drain.runtime_writers)?;
        validate_writer_set(self, &drain.proxy_writers, 1, 1)?;
        validate_proxy_topology(self, &drain.proxy_writers)?;
        validate_partial_shape(drain)?;
        if let Some(observation) = &drain.runtime_termination {
            validate_termination(self, observation, &drain.runtime_writers)?;
        }
        if let Some(observation) = &drain.final_proxy_ledger {
            validate_proxy_ledger(self, observation, &drain.proxy_writers)?;
        }
        if let Some(observation) = &drain.proxy_termination {
            validate_termination(self, observation, &drain.proxy_writers)?;
            let ledger = drain
                .final_proxy_ledger
                .as_ref()
                .ok_or_else(|| invalid("proxy termination lacks its final ledger"))?;
            if !observation.container_ids.contains(&ledger.container_id) {
                return Err(invalid(
                    "proxy termination differs from its final ledger identity",
                ));
            }
        }
        validate_retirement(self, drain)?;
        let bytes = compiler_core::canonical_bytes(drain)?.len();
        if bytes
            > usize::try_from(self.max_drain_bytes)
                .map_err(|_| invalid("drain evidence byte ceiling invalid"))?
            || bytes
                > usize::try_from(MAX_DRAIN_BYTES)
                    .map_err(|_| invalid("drain evidence byte ceiling invalid"))?
        {
            return Err(invalid("drain evidence exceeds canonical byte ceiling"));
        }
        Ok(())
    }

    fn max_container_count(&self) -> Result<usize> {
        usize::from(self.max_regular_containers)
            .checked_add(usize::from(self.max_init_containers))
            .and_then(|count| count.checked_add(usize::from(self.max_ephemeral_containers)))
            .ok_or_else(|| invalid("drain evidence container count overflow"))
    }

    fn minimum_complete_scalar_bytes(&self, containers: usize) -> Result<usize> {
        let pods = usize::from(self.max_controller_pods);
        let uid_slots = pods
            .checked_mul(3)
            .and_then(|count| count.checked_add(8))
            .ok_or_else(|| invalid("drain evidence UID capacity overflow"))?;
        let rv_slots = pods
            .checked_add(4)
            .ok_or_else(|| invalid("drain evidence version capacity overflow"))?;
        let container_slots = containers
            .checked_mul(2)
            .and_then(|count| count.checked_add(5))
            .ok_or_else(|| invalid("drain evidence container capacity overflow"))?;
        uid_slots
            .checked_mul(usize::from(self.max_uid_json_bytes))
            .and_then(|bytes| {
                rv_slots
                    .checked_mul(usize::from(self.max_resource_version_json_bytes))
                    .and_then(|versions| bytes.checked_add(versions))
            })
            .and_then(|bytes| {
                container_slots
                    .checked_mul(usize::from(self.max_container_id_json_bytes))
                    .and_then(|containers| bytes.checked_add(containers))
            })
            .ok_or_else(|| invalid("drain evidence scalar capacity overflow"))
    }
}

fn validate_writer_set(
    profile: &EvidenceProfile,
    writers: &ExpectedWriters,
    minimum: usize,
    maximum: usize,
) -> Result<()> {
    if writers.pods.len() < minimum
        || writers.pods.len() > maximum
        || !crate::valid_name(&writers.deployment_name)
        || !crate::valid_name(&writers.service_account_name)
    {
        return Err(invalid("drain writer set malformed or exceeds profile"));
    }
    validate_uid(profile, &writers.deployment_uid)?;
    let mut names = BTreeSet::new();
    let mut uids = BTreeSet::new();
    for pod in &writers.pods {
        if !crate::valid_name(&pod.name)
            || !crate::valid_name(&pod.node_name)
            || !names.insert(&pod.name)
            || !uids.insert(&pod.uid)
        {
            return Err(invalid("drain writer Pod identity malformed or duplicate"));
        }
        validate_uid(profile, &pod.uid)?;
        validate_uid(profile, &pod.node_uid)?;
    }
    Ok(())
}

fn validate_termination(
    profile: &EvidenceProfile,
    observation: &crate::ProcessTerminationObservation,
    writers: &ExpectedWriters,
) -> Result<()> {
    let expected_containers: BTreeSet<_> = writers
        .pods
        .iter()
        .flat_map(|pod| {
            pod.containers
                .regular
                .iter()
                .chain(&pod.containers.init)
                .chain(&pod.containers.ephemeral)
                .map(|container| container.container_id.as_str())
        })
        .collect();
    if observation.deployment_uid != writers.deployment_uid
        || observation.pod_resource_versions.len() != writers.pods.len()
        || observation.container_ids.len() != expected_containers.len()
    {
        return Err(invalid(
            "drain termination cardinality or identity mismatch",
        ));
    }
    validate_uid(profile, &observation.deployment_uid)?;
    validate_resource_version(profile, &observation.deployment_resource_version)?;
    let mut seen_pods = BTreeSet::new();
    for ((uid, version), expected) in observation.pod_resource_versions.iter().zip(&writers.pods) {
        if uid != &expected.uid || !seen_pods.insert(uid) {
            return Err(invalid("drain termination Pod identity mismatch"));
        }
        validate_uid(profile, uid)?;
        validate_resource_version(profile, version)?;
    }
    let mut containers = BTreeSet::new();
    for id in &observation.container_ids {
        if !containers.insert(id) || !expected_containers.contains(id.as_str()) {
            return Err(invalid("drain termination container identity duplicate"));
        }
        validate_container_id(profile, id)?;
    }
    Ok(())
}

fn validate_topology(profile: &EvidenceProfile, topology: &CapturedContainers) -> Result<()> {
    if topology.regular.is_empty() {
        return Err(invalid("captured regular container set is empty"));
    }
    let mut names = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for container in topology
        .regular
        .iter()
        .chain(&topology.init)
        .chain(&topology.ephemeral)
    {
        if !valid_container_name(&container.name)
            || !names.insert(&container.name)
            || !ids.insert(&container.container_id)
        {
            return Err(invalid(
                "captured container identity malformed or duplicate",
            ));
        }
        validate_container_id(profile, &container.container_id)?;
    }
    Ok(())
}

fn validate_proxy_topology(profile: &EvidenceProfile, writers: &ExpectedWriters) -> Result<()> {
    let topology = &writers
        .pods
        .first()
        .ok_or_else(|| invalid("proxy drain writer missing"))?
        .containers;
    if topology.regular.len() != 2 || !topology.init.is_empty() || !topology.ephemeral.is_empty() {
        return Err(invalid(
            "proxy drain container topology differs from fixed profile",
        ));
    }
    validate_topology(profile, topology)
}

fn validate_proxy_ledger(
    profile: &EvidenceProfile,
    ledger: &crate::ProxyDrainObservation,
    writers: &ExpectedWriters,
) -> Result<()> {
    let pod = writers
        .pods
        .first()
        .ok_or_else(|| invalid("proxy drain writer missing"))?;
    if ledger.pod_uid != pod.uid
        || !crate::valid_digest(&ledger.qualification_digest)
        || !crate::valid_digest(&ledger.ledger_digest)
        || !pod
            .containers
            .regular
            .iter()
            .chain(&pod.containers.init)
            .chain(&pod.containers.ephemeral)
            .any(|container| container.container_id == ledger.container_id)
    {
        return Err(invalid("proxy drain ledger identity or digest malformed"));
    }
    validate_uid(profile, &ledger.pod_uid)?;
    validate_resource_version(profile, &ledger.pod_resource_version)?;
    validate_container_id(profile, &ledger.container_id)
}

fn validate_partial_shape(drain: &UpgradeDrainState) -> Result<()> {
    let runtime = drain.runtime_termination.is_some();
    let ledger = drain.final_proxy_ledger.is_some();
    let proxy = drain.proxy_termination.is_some();
    if !matches!(
        (runtime, ledger, proxy),
        (false, false, false) | (true, true, _)
    ) {
        return Err(invalid("drain evidence checkpoint shape unreachable"));
    }
    if !drain.retired_identity_uids.is_empty() && !proxy {
        return Err(invalid(
            "drain retirement precedes complete termination evidence",
        ));
    }
    Ok(())
}

fn validate_retirement(profile: &EvidenceProfile, drain: &UpgradeDrainState) -> Result<()> {
    for (key, uid) in &drain.retired_identity_uids {
        let parts: Vec<_> = key.split('/').collect();
        let valid = match parts.as_slice() {
            ["v1", "ServiceAccount", namespace, name] => {
                crate::valid_namespace(namespace) && crate::valid_name(name)
            }
            [
                "rbac.authorization.k8s.io",
                "v1",
                "RoleBinding",
                namespace,
                name,
            ] => crate::valid_namespace(namespace) && crate::valid_name(name),
            _ => false,
        };
        if !valid {
            return Err(invalid("drain retirement identity malformed"));
        }
        validate_uid(profile, uid)?;
    }
    Ok(())
}

fn validate_uid(profile: &EvidenceProfile, value: &str) -> Result<()> {
    validate_scalar(
        value,
        usize::from(profile.max_uid_json_bytes),
        "drain UID malformed",
    )
}

fn validate_resource_version(profile: &EvidenceProfile, value: &str) -> Result<()> {
    validate_scalar(
        value,
        usize::from(profile.max_resource_version_json_bytes),
        "drain resourceVersion malformed",
    )
}

fn validate_container_id(profile: &EvidenceProfile, value: &str) -> Result<()> {
    validate_scalar(
        value,
        usize::from(profile.max_container_id_json_bytes),
        "drain container identity malformed",
    )
}

fn validate_scalar(value: &str, encoded_ceiling: usize, error: &'static str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_RAW_SCALAR_BYTES
        || !value.bytes().all(|byte| byte.is_ascii_graphic())
        || compiler_core::canonical_bytes(&value)?.len() > encoded_ceiling
    {
        return Err(invalid(error));
    }
    Ok(())
}

fn valid_container_name(value: &str) -> bool {
    value.len() <= 63 && !value.contains('.') && crate::valid_name(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExpectedPod, ProcessTerminationObservation, ProxyDrainObservation};
    use std::collections::BTreeMap;

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

    fn profile() -> EvidenceProfile {
        EvidenceProfile {
            schema_version: EVIDENCE_PROFILE_SCHEMA.into(),
            max_controller_pods: 1,
            max_regular_containers: 2,
            max_init_containers: 1,
            max_ephemeral_containers: 1,
            max_uid_json_bytes: 128,
            max_resource_version_json_bytes: 128,
            max_container_id_json_bytes: 256,
            max_drain_bytes: MAX_DRAIN_BYTES,
        }
    }

    fn writers(name: &str, pod: &str) -> ExpectedWriters {
        let mut regular = vec![CapturedContainer {
            name: "runtime".into(),
            container_id: "containerd://runtime".into(),
        }];
        if name == "proxy" {
            regular = vec![
                CapturedContainer {
                    name: "envoy".into(),
                    container_id: "containerd://envoy".into(),
                },
                CapturedContainer {
                    name: "admin".into(),
                    container_id: "containerd://admin".into(),
                },
            ];
        }
        ExpectedWriters {
            deployment_name: name.into(),
            deployment_uid: format!("{name}-uid"),
            service_account_name: "writer-account".into(),
            pods: vec![ExpectedPod {
                name: pod.into(),
                uid: format!("{pod}-uid"),
                node_name: "node-a".into(),
                node_uid: "node-uid".into(),
                containers: CapturedContainers {
                    regular,
                    init: vec![],
                    ephemeral: vec![],
                },
            }],
        }
    }

    fn drain() -> UpgradeDrainState {
        UpgradeDrainState {
            previous_transition_id: "transition-a".into(),
            runtime_writers: writers("runtime", "runtime-pod"),
            proxy_writers: writers("proxy", "proxy-pod"),
            runtime_termination: None,
            final_proxy_ledger: None,
            proxy_termination: None,
            retired_identity_uids: BTreeMap::new(),
        }
    }

    fn complete(mut drain: UpgradeDrainState) -> UpgradeDrainState {
        drain.runtime_termination = Some(ProcessTerminationObservation {
            deployment_uid: "runtime-uid".into(),
            deployment_resource_version: "12".into(),
            pod_resource_versions: vec![("runtime-pod-uid".into(), "13".into())],
            container_ids: vec!["containerd://runtime".into()],
        });
        drain.final_proxy_ledger = Some(ProxyDrainObservation {
            pod_uid: "proxy-pod-uid".into(),
            pod_resource_version: "14".into(),
            container_id: "containerd://envoy".into(),
            qualification_digest: format!("sha256:{}", "1".repeat(64)),
            ledger_digest: format!("sha256:{}", "2".repeat(64)),
            completed_requests: 7,
            rejected_after_freeze: 0,
        });
        drain.proxy_termination = Some(ProcessTerminationObservation {
            deployment_uid: "proxy-uid".into(),
            deployment_resource_version: "15".into(),
            pod_resource_versions: vec![("proxy-pod-uid".into(), "16".into())],
            container_ids: vec!["containerd://envoy".into(), "containerd://admin".into()],
        });
        drain
    }

    #[test]
    fn profile_tiers_cardinality_and_monotonicity_are_bounded() -> TestResult {
        let baseline = profile();
        baseline.validate()?;
        assert!(baseline.contains(&baseline));
        for invalid in [
            EvidenceProfile {
                max_controller_pods: 0,
                ..baseline.clone()
            },
            EvidenceProfile {
                max_controller_pods: 65,
                ..baseline.clone()
            },
            EvidenceProfile {
                max_uid_json_bytes: 129,
                ..baseline.clone()
            },
            EvidenceProfile {
                max_regular_containers: u16::MAX,
                ..baseline.clone()
            },
        ] {
            assert!(invalid.validate().is_err());
            assert!(!invalid.contains(&baseline));
        }
        let mut larger = baseline.clone();
        larger.max_uid_json_bytes = 512;
        larger.max_drain_bytes = MAX_DRAIN_BYTES;
        larger.validate()?;
        assert!(larger.contains(&baseline));
        assert!(!baseline.contains(&larger));

        let mut scalar_boundary = baseline;
        scalar_boundary.max_regular_containers = 249;
        scalar_boundary.max_init_containers = 0;
        scalar_boundary.max_ephemeral_containers = 0;
        scalar_boundary.validate()?;
        scalar_boundary.max_regular_containers = 250;
        assert!(scalar_boundary.validate().is_err());
        Ok(())
    }

    #[test]
    fn drain_validation_accepts_reachable_partial_shapes_and_exact_byte_boundary() -> TestResult {
        let mut current = drain();
        profile().validate_drain(&current)?;
        let mut complete = complete(current.clone());
        let runtime = complete
            .runtime_termination
            .clone()
            .ok_or("runtime termination missing")?;
        let ledger = complete
            .final_proxy_ledger
            .clone()
            .ok_or("proxy ledger missing")?;
        current.runtime_termination = Some(runtime);
        current.final_proxy_ledger = Some(ledger);
        profile().validate_drain(&current)?;
        profile().validate_drain(&complete)?;

        for index in 0..64 {
            complete.retired_identity_uids.insert(
                format!("rbac.authorization.k8s.io/v1/RoleBinding/workload/retired-{index}"),
                "u".repeat(126),
            );
        }
        let bytes = compiler_core::canonical_bytes(&complete)?.len();
        let mut exact = profile();
        exact.max_drain_bytes = u32::try_from(bytes)?;
        exact.validate_drain(&complete)?;
        exact.max_drain_bytes = exact.max_drain_bytes.saturating_sub(1);
        assert!(exact.validate_drain(&complete).is_err());
        Ok(())
    }

    #[test]
    fn drain_validation_rejects_unreachable_duplicates_and_cross_object_changes() -> TestResult {
        let baseline = complete(drain());
        let mut ledger_only = drain();
        ledger_only.final_proxy_ledger = baseline.final_proxy_ledger.clone();
        assert!(profile().validate_drain(&ledger_only).is_err());

        let mut foreign_ledger = drain();
        foreign_ledger.runtime_termination = baseline.runtime_termination.clone();
        foreign_ledger.final_proxy_ledger = baseline.final_proxy_ledger.clone();
        foreign_ledger
            .final_proxy_ledger
            .as_mut()
            .ok_or("proxy ledger missing")?
            .container_id = "containerd://foreign".into();
        assert!(profile().validate_drain(&foreign_ledger).is_err());

        let mut duplicate = baseline.clone();
        duplicate.runtime_writers.pods.push(
            duplicate
                .runtime_writers
                .pods
                .first()
                .ok_or("runtime pod missing")?
                .clone(),
        );
        let mut two_pods = profile();
        two_pods.max_controller_pods = 2;
        two_pods.max_regular_containers = 2;
        assert!(two_pods.validate_drain(&duplicate).is_err());

        let mut duplicate_container = drain();
        let mut second = duplicate_container
            .runtime_writers
            .pods
            .first()
            .ok_or("runtime pod missing")?
            .clone();
        second.name = "runtime-pod-two".into();
        second.uid = "runtime-pod-two-uid".into();
        duplicate_container.runtime_writers.pods.push(second);
        assert!(
            two_pods
                .validate_writers(&duplicate_container.runtime_writers)
                .is_err()
        );

        let mut changed = baseline;
        changed
            .runtime_termination
            .as_mut()
            .ok_or("runtime termination missing")?
            .deployment_uid = "replacement-uid".into();
        assert!(profile().validate_drain(&changed).is_err());
        Ok(())
    }

    #[test]
    fn scalar_tiers_use_complete_canonical_json_string_bytes() -> TestResult {
        let mut exact = drain();
        exact.runtime_writers.deployment_uid = "\\".repeat(63);
        profile().validate_drain(&exact)?;
        exact.runtime_writers.deployment_uid.push('\\');
        assert!(profile().validate_drain(&exact).is_err());

        let mut control = drain();
        control.runtime_writers.deployment_uid = "line\nbreak".into();
        assert!(profile().validate_drain(&control).is_err());
        Ok(())
    }
}
