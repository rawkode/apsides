//! The CNPG release pair: the database alone, then the database with its API.
//!
//! The pair is checked to be exactly the upgrade the scenario is about before
//! any cluster exists: the successor keeps every initial resource unchanged,
//! adds only the API Deployment and Service, observes exactly the settings
//! `ConfigMap` and the database Secret, and needs at least one capability the
//! initial release does not. The platform policies the scenario installs are
//! derived here from each release's own plan.

use crate::{Result, digest, ensure};
use compiler_core::{Capability, CompiledPlan, InputMode, PermissionCeiling, Resource};
use platform::PolicyState;
use release::{ReleaseManifest, ReleaseMode};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

/// The scope both example programs declare.
pub const DOMAIN: &str = "database-demo";
pub const CLUSTER_BINDING: &str = "local-demo";
pub const CONTROL_NAMESPACE: &str = "apsides-system";
pub const WORKLOAD_NAMESPACE: &str = "default";

/// One signed release as the scenario deploys it.
pub struct Candidate {
    pub root: PathBuf,
    pub manifest: ReleaseManifest,
    pub plan: CompiledPlan,
    /// SHA-256 of `release.json`, as `aps deploy` reports it.
    pub release_digest: String,
}

impl Candidate {
    /// Read a release directory without verifying its signature; `aps
    /// deploy` does that against the platform's trust policy.
    ///
    /// # Errors
    /// Fails when the release is unreadable or not a production release.
    pub fn read(root: &Path) -> Result<Self> {
        let (manifest, plan, bytes) = release::read_release(root)?;
        ensure(
            manifest.mode == ReleaseMode::Production && !plan.ir.test_only,
            "CNPG scenario subject is not a production release",
        )?;
        Ok(Self {
            root: root.to_path_buf(),
            manifest,
            plan,
            release_digest: digest(&bytes),
        })
    }
}

/// The checked pair and the two platform policies.
pub struct Pair {
    pub initial: Candidate,
    pub successor: Candidate,
    /// Allows exactly the initial release's capabilities.
    pub initial_policy: PolicyState,
    /// Allows exactly the successor release's capabilities.
    pub successor_policy: PolicyState,
    /// The first successor capability the initial policy does not allow.
    pub missing: Capability,
    /// The single digest-pinned image of the successor's API Deployment.
    pub api_image: String,
}

impl Pair {
    /// Read and check the pair.
    ///
    /// # Errors
    /// Fails when either release is unreadable or the pair is not the bounded
    /// CNPG database-to-API upgrade.
    pub fn load(initial: &Path, successor: &Path) -> Result<Self> {
        let initial = Candidate::read(initial)?;
        let successor = Candidate::read(successor)?;
        for candidate in [&initial, &successor] {
            let domain = &candidate.plan.ir.domain;
            ensure(
                domain.id == DOMAIN
                    && domain.cluster_binding == CLUSTER_BINDING
                    && domain.control_namespace == CONTROL_NAMESPACE
                    && domain.namespace == WORKLOAD_NAMESPACE,
                "CNPG release does not use the local demo scope",
            )?;
        }
        validate_upgrade(&initial.plan, &successor.plan)?;
        let initial_policy = policy(&initial.plan);
        let successor_policy = policy(&successor.plan);
        ensure(
            compiler_core::digest(&initial_policy)? != compiler_core::digest(&successor_policy)?,
            "CNPG demo policies do not identify an upgrade",
        )?;
        let missing = missing_capability(&initial.plan, &successor.plan)?;
        let api_image = api_image(&successor.plan)?;
        Ok(Self {
            initial,
            successor,
            initial_policy,
            successor_policy,
            missing,
            api_image,
        })
    }
}

/// The last-type cleanup pair: `examples/cnpg/cleanup`, which adds one
/// `Backup` to the successor with deletion policy `delete`, then
/// `examples/cnpg`, which removes it.
pub struct CleanupPair {
    pub predecessor: Candidate,
    pub successor: Candidate,
    /// The one policy both generations deploy under: the predecessor's
    /// runtime ceiling, and a cleanup ceiling that also allows reading and
    /// deleting exactly the `Backup`.
    pub policy: PolicyState,
}

/// The `Backup` the cleanup pair removes.
pub const CLEANUP_GROUP: &str = "postgresql.cnpg.io";
pub const CLEANUP_RESOURCE: &str = "backups";
pub const CLEANUP_NAME: &str = "cleanup-backup";

impl CleanupPair {
    /// Read and check the pair.
    ///
    /// # Errors
    /// Fails when either release is unreadable, or the pair is not exactly
    /// the removal of the one `Backup` with every other resource unchanged.
    pub fn load(predecessor: &Path, successor: &Path) -> Result<Self> {
        let predecessor = Candidate::read(predecessor)?;
        let successor = Candidate::read(successor)?;
        let is_target =
            |resource: &Resource| resource.kind == "Backup" && resource.name == CLEANUP_NAME;
        let (removed, retained): (Vec<_>, Vec<_>) = predecessor
            .plan
            .ir
            .resources
            .iter()
            .partition(|resource| is_target(resource));
        ensure(
            matches!(removed.as_slice(), [backup]
                if backup.lifecycle.deletion_policy == compiler_core::DeletionPolicy::Delete
                && backup.namespace == WORKLOAD_NAMESPACE)
                && retained.len() == successor.plan.ir.resources.len()
                && retained
                    .iter()
                    .zip(&successor.plan.ir.resources)
                    .all(|(before, after)| projection(before) == projection(after))
                && !successor.plan.capabilities.iter().any(|capability| {
                    capability.api_group == CLEANUP_GROUP && capability.resource == CLEANUP_RESOURCE
                }),
            "cleanup pair is not the bounded CNPG last-type removal",
        )?;
        let runtime = ceiling(&predecessor.plan);
        let mut cleanup = runtime.clone();
        let mut capabilities = cleanup.capabilities.into_iter().collect::<BTreeSet<_>>();
        for verb in ["get", "delete"] {
            capabilities.insert(Capability {
                namespace: WORKLOAD_NAMESPACE.into(),
                api_group: CLEANUP_GROUP.into(),
                resource: CLEANUP_RESOURCE.into(),
                subresource: None,
                verb: verb.into(),
                names: vec![CLEANUP_NAME.into()],
            });
        }
        cleanup.capabilities = capabilities.into_iter().collect();
        Ok(Self {
            predecessor,
            successor,
            policy: PolicyState {
                image_pull_secrets: Vec::new(),
                schema_version: platform::SCHEMA_VERSION.into(),
                runtime_ceiling: runtime,
                cleanup_ceiling: cleanup,
            },
        })
    }
}

/// A runtime ceiling allowing exactly `plan`'s capabilities, gaps and risks.
#[must_use]
pub fn ceiling(plan: &CompiledPlan) -> PermissionCeiling {
    let domain = &plan.ir.domain;
    let mut gaps = plan.enforcement_gaps.clone();
    gaps.sort();
    let mut risks = plan.indirect_risks.clone();
    risks.sort();
    PermissionCeiling {
        policy_ref: domain.policy_ref.clone(),
        domain_id: domain.id.clone(),
        cluster_binding: domain.cluster_binding.clone(),
        workload_namespace: domain.namespace.clone(),
        control_namespace: domain.control_namespace.clone(),
        capabilities: plan.capabilities.clone(),
        accepted_enforcement_gaps: gaps,
        accepted_indirect_risks: risks,
    }
}

/// The platform policy whose runtime and cleanup ceilings are both
/// [`ceiling`] of `plan`.
#[must_use]
pub fn policy(plan: &CompiledPlan) -> PolicyState {
    let ceiling = ceiling(plan);
    PolicyState {
        image_pull_secrets: Vec::new(),
        schema_version: platform::SCHEMA_VERSION.into(),
        runtime_ceiling: ceiling.clone(),
        cleanup_ceiling: ceiling,
    }
}

/// The first successor capability, in plan order, that the initial plan
/// lacks: the one the ceiling check reports first.
///
/// # Errors
/// Fails when the successor adds no capability.
pub fn missing_capability(initial: &CompiledPlan, successor: &CompiledPlan) -> Result<Capability> {
    successor
        .capabilities
        .iter()
        .find(|capability| !initial.capabilities.contains(capability))
        .cloned()
        .ok_or_else(|| crate::Error::Check("successor has no added capability".into()))
}

/// The diagnostic `aps deploy` gives when `capability` is outside the runtime
/// ceiling.
#[must_use]
pub fn rejection(capability: &Capability) -> String {
    format!(
        "{} {}/{} in {} is not explicitly allowed",
        capability.verb, capability.api_group, capability.resource, capability.namespace
    )
}

fn validate_upgrade(initial: &CompiledPlan, successor: &CompiledPlan) -> Result {
    let keys = |plan: &CompiledPlan| {
        plan.ir
            .resources
            .iter()
            .map(|item| (item.kind.clone(), item.name.clone()))
            .collect::<BTreeSet<_>>()
    };
    let expected_initial = BTreeSet::from([
        ("Cluster".to_owned(), "database".to_owned()),
        ("ConfigMap".to_owned(), "api-config".to_owned()),
    ]);
    ensure(
        keys(initial) == expected_initial,
        "initial candidate is not the bounded CNPG database deployment",
    )?;
    let mut expected_successor = expected_initial;
    expected_successor.insert(("Deployment".into(), "database-api".into()));
    expected_successor.insert(("Service".into(), "database-api".into()));
    ensure(
        keys(successor) == expected_successor,
        "successor candidate is not the bounded CNPG API upgrade",
    )?;
    for retained in &initial.ir.resources {
        let matching = successor
            .ir
            .resources
            .iter()
            .find(|item| item.kind == retained.kind && item.name == retained.name);
        ensure(
            matching.is_some_and(|item| projection(item) == projection(retained)),
            "successor changed a retained initial resource",
        )?;
    }
    let observed = successor
        .ir
        .inputs
        .iter()
        .filter(|input| input.mode == InputMode::Observed)
        .map(|input| {
            (
                input.kind.as_str(),
                input.name.as_str(),
                input.namespace.as_str(),
            )
        })
        .collect::<BTreeSet<_>>();
    ensure(
        observed
            == BTreeSet::from([
                ("ConfigMap", "api-settings", WORKLOAD_NAMESPACE),
                ("Secret", "database-app", WORKLOAD_NAMESPACE),
            ]),
        "successor does not bind the exact observed ConfigMap and Secret",
    )?;
    ensure(
        initial.ir.inputs.is_empty(),
        "initial candidate unexpectedly depends on observed state",
    )
}

/// The fields of a resource an upgrade must not change.
fn projection(resource: &Resource) -> impl PartialEq + '_ {
    (
        &resource.api_version,
        &resource.kind,
        &resource.api_resource,
        &resource.name,
        &resource.namespace,
        &resource.desired,
        &resource.lifecycle,
    )
}

fn api_image(plan: &CompiledPlan) -> Result<String> {
    let deployment = plan
        .ir
        .resources
        .iter()
        .find(|item| item.kind == "Deployment" && item.name == "database-api");
    let containers = deployment
        .and_then(|item| item.desired.pointer("/spec/template/spec/containers"))
        .and_then(serde_json::Value::as_array);
    let image = match containers.map(Vec::as_slice) {
        Some([container]) => container["image"].as_str().unwrap_or_default(),
        _ => "",
    };
    ensure(
        image.contains("@sha256:"),
        "signed successor does not pin one PostgREST image",
    )?;
    Ok(image.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capability(verb: &str, resource: &str) -> Capability {
        Capability {
            namespace: WORKLOAD_NAMESPACE.into(),
            api_group: "apps".into(),
            resource: resource.into(),
            subresource: None,
            verb: verb.into(),
            names: vec!["database-api".into()],
        }
    }

    #[test]
    fn the_rejection_names_the_capability_the_ceiling_lacks() {
        assert_eq!(
            rejection(&capability("get", "deployments")),
            "get apps/deployments in default is not explicitly allowed"
        );
    }
}
