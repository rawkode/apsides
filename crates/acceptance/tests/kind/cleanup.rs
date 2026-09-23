//! Last-type cleanup: the upgrade from `examples/cnpg/cleanup` to
//! `examples/cnpg` deletes the one `Backup` only the predecessor declares.

use crate::{
    support::{committed_report, stored_phase},
    upgrade::create_api_settings,
};
use acceptance::{
    Error, Result, aps,
    bootstrap::Platform,
    cnpg, ensure,
    hold::{Hold, Target},
    objects::{Objects, deleting, text},
    observe::registry_metadata,
    pair::{
        CLEANUP_GROUP, CLEANUP_NAME, CLEANUP_RESOURCE, CONTROL_NAMESPACE, CleanupPair, DOMAIN,
        WORKLOAD_NAMESPACE,
    },
    scenario::{self, Scenario, ending, note},
    wait,
};
use deploy::ClaimState;
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};

/// The finalizer the scenario puts on the pinned `Backup`.
const FINALIZER: &str = "acceptance.apsides.dev/cleanup-hold";
/// A `Backup` of the same type no release claims.
const UNCLAIMED: &str = "cleanup-unclaimed-control";
/// What `aps deploy` reports while a deleted object keeps a finalizer.
const PENDING: &str = "cleanup remains pending";
/// How cleanup refuses an object recreated under the claimed name.
const REPLACEMENT: &str = "cleanup refuses to delete a replacement UID";

/// Spec §28 "Delete the last resource of a type" and "Hold deletion with a
/// finalizer": with a test-owned finalizer on the claimed `Backup`, `aps
/// deploy` of the successor deletes it and reports cleanup pending while the
/// finalizer remains (`aps` is then killed rather than left to its 300 s loop).
/// Once the finalizer's owner removes it, rerunning `aps deploy` commits, the
/// `Backup` is gone, an unclaimed `Backup` of the same type is untouched, and
/// the successor's workload Role grants no `Backup` access.
#[tokio::test]
#[ignore = "creates a kind cluster; needs Docker, kind and `cuenv task acceptancePrepare`"]
async fn cleanup_waits_for_a_finalizer_then_commits() -> Result {
    scenario::run("cleanup-finalizer", async |scenario| {
        let pair = CleanupPair::load(
            &scenario.inputs.cleanup_release,
            &scenario.inputs.successor_release,
        )?;
        let (platform, original) = predecessor_committed(scenario, &pair).await?;
        finalizer_held(scenario, &pair, &platform, &original).await?;
        unrelated_preserved(scenario, &platform).await
    })
    .await
}

/// Spec §28 "Replace a cleanup target's UID": after the successor's cleanup
/// has pinned the claimed `Backup`'s UID (held in Cleaning by an admission
/// policy denying the deployer's DELETE), the `Backup` is deleted and
/// recreated. Rerunning `aps deploy` must refuse to delete the new UID, keep
/// the claim Releasing for the original UID in Cleaning, leave the registry
/// unchanged and the replacement untouched, preserve the unclaimed `Backup`
/// and grant the successor's workload Role no `Backup` access.
#[tokio::test]
#[ignore = "creates a kind cluster; needs Docker, kind and `cuenv task acceptancePrepare`"]
async fn cleanup_refuses_a_replacement_uid() -> Result {
    scenario::run("cleanup-replacement", async |scenario| {
        let pair = CleanupPair::load(
            &scenario.inputs.cleanup_release,
            &scenario.inputs.successor_release,
        )?;
        let (platform, original) = predecessor_committed(scenario, &pair).await?;
        replacement_refused(scenario, &pair, &platform, &original).await?;
        unrelated_preserved(scenario, &platform).await
    })
    .await
}

/// Bootstrap under the cleanup policy, commit the predecessor, and create
/// the unclaimed `Backup`; returns the claimed `Backup`.
async fn predecessor_committed(
    scenario: &mut Scenario<'_>,
    pair: &CleanupPair,
) -> Result<(Platform, Value)> {
    let policies = BTreeMap::from([(DOMAIN.to_owned(), pair.policy.clone())]);
    let platform = scenario
        .prepare(&[&pair.predecessor, &pair.successor], &policies)
        .await?;
    let objects = scenario.objects.clone();
    create_api_settings(&objects).await?;
    let run = scenario
        .deploy(&platform, &pair.predecessor.root, "deploy-predecessor")
        .await?;
    let committed = aps::committed(
        &run,
        &pair.predecessor.release_digest,
        &pair.predecessor.manifest.ir_digest,
    )?;
    cnpg::wait_ready(&objects, WORKLOAD_NAMESPACE).await?;
    let original = backup(&objects, CLEANUP_NAME)
        .await?
        .filter(|backup| !deleting(backup))
        .ok_or_else(|| Error::Check("the claimed Backup is absent or deleting".into()))?;
    let unclaimed = objects
        .create(&json!({
            "apiVersion": "postgresql.cnpg.io/v1", "kind": "Backup",
            "metadata": {"name": UNCLAIMED, "namespace": WORKLOAD_NAMESPACE},
            "spec": {"cluster": {"name": "cleanup-unconfigured-database"}},
        }))
        .await?;
    scenario.recorder.check(
        "predecessor-committed-with-claimed-backup",
        &json!({
            "report": committed_report(&committed),
            "claimedUid": original["metadata"]["uid"],
            "unclaimedUid": unclaimed["metadata"]["uid"],
        }),
    );
    Ok((platform, original))
}

async fn finalizer_held(
    scenario: &mut Scenario<'_>,
    pair: &CleanupPair,
    platform: &Platform,
    original: &Value,
) -> Result {
    let objects = scenario.objects.clone();
    let uid = text(original, "/metadata/uid").to_owned();
    let mut held = original.clone();
    held["metadata"]["finalizers"] = json!([FINALIZER]);
    objects.replace(&held).await?;
    let sequence = platform.registry().await?.value.next_sequence;
    let transition =
        deploy::allocation_names(&platform.registry().await?.value.registry_uid, sequence)?
            .transition_id;

    note("deploying the successor until cleanup is pending");
    let mut running = scenario.spawn(platform, &pair.successor.root).await?;
    let pending = wait::until(
        "cleanup to report the held Backup pending",
        Duration::from_secs(280),
        async || {
            if running.exited()?.is_some() {
                return Err(Error::Check(
                    "aps deploy ended before cleanup was pending".into(),
                ));
            }
            let current = backup(&objects, CLEANUP_NAME).await?;
            Ok((running.stderr().contains(PENDING)
                && current.as_ref().is_some_and(|backup| {
                    deleting(backup) && text(backup, "/metadata/uid") == uid
                }))
            .then_some(current))
        },
    )
    .await?
    .ok_or_else(|| Error::Check("the held Backup disappeared".into()))?;
    let (stop, output) = running.kill().await?;
    scenario.document("deploy-successor-pending", &output);
    ensure(
        stored_phase(&objects, &transition).await?.1.as_deref() == Some("Cleaning")
            && pending["metadata"]["finalizers"] == json!([FINALIZER]),
        "pending cleanup left another phase or dropped the finalizer",
    )?;

    note("removing the finalizer and resuming");
    let mut released = backup(&objects, CLEANUP_NAME)
        .await?
        .ok_or_else(|| Error::Check("the held Backup disappeared".into()))?;
    released["metadata"]["finalizers"] = json!([]);
    objects.replace(&released).await?;
    let run = scenario
        .deploy(platform, &pair.successor.root, "deploy-successor-resumed")
        .await?;
    let committed = aps::committed(
        &run,
        &pair.successor.release_digest,
        &pair.successor.manifest.ir_digest,
    )?;
    ensure(
        committed.transition == transition && backup(&objects, CLEANUP_NAME).await?.is_none(),
        "cleanup committed another transition or before the Backup was gone",
    )?;
    let registry = platform.registry().await?;
    ensure(
        !registry
            .value
            .claims
            .iter()
            .any(|claim| claim.identity.name == CLEANUP_NAME),
        "the Backup's claim outlived its cleanup",
    )?;
    scenario.recorder.check(
        "finalizer-pending-then-last-type-cleanup",
        &json!({
            "originalUid": uid,
            "finalizer": FINALIZER,
            "pendingStop": ending(stop),
            "report": committed_report(&committed),
        }),
    );
    Ok(())
}

async fn replacement_refused(
    scenario: &mut Scenario<'_>,
    pair: &CleanupPair,
    platform: &Platform,
    original: &Value,
) -> Result {
    let objects = scenario.objects.clone();
    let uid = text(original, "/metadata/uid").to_owned();
    let hold = Hold::deletion(
        &objects,
        CONTROL_NAMESPACE,
        &Target {
            api_group: CLEANUP_GROUP,
            resource: CLEANUP_RESOURCE,
            namespace: WORKLOAD_NAMESPACE,
            name: CLEANUP_NAME,
        },
    );
    let installed = hold.activate().await?;
    let sequence = platform.registry().await?.value.next_sequence;
    let transition =
        deploy::allocation_names(&platform.registry().await?.value.registry_uid, sequence)?
            .transition_id;

    note("deploying the successor until its cleanup delete is denied");
    let run = scenario
        .deploy(platform, &pair.successor.root, "deploy-successor-held")
        .await?;
    ensure(
        !run.success && hold.denied(&run.stderr),
        "the successor's cleanup was not stopped by the deletion hold",
    )?;
    let registry = platform.registry().await?;
    ensure(
        stored_phase(&objects, &transition).await?.1.as_deref() == Some("Cleaning")
            && releasing(&registry.value.claims, &uid),
        "the held cleanup did not pin the original UID in Cleaning",
    )?;

    note("replacing the claimed Backup");
    let replacement_uid = replace_backup(&objects, pair, &uid).await?;
    hold.release().await?;

    note("resuming the successor against the replacement");
    let registry_before = registry_metadata(&objects).await?;
    let replacement = backup(&objects, CLEANUP_NAME)
        .await?
        .ok_or_else(|| Error::Check("the replacement Backup disappeared".into()))?;
    let run = scenario
        .deploy(platform, &pair.successor.root, "deploy-successor-refused")
        .await?;
    aps::refused(&run, REPLACEMENT)?;
    let after = backup(&objects, CLEANUP_NAME)
        .await?
        .ok_or_else(|| Error::Check("cleanup deleted the replacement Backup".into()))?;
    let registry = platform.registry().await?;
    ensure(
        text(&after, "/metadata/uid") == replacement_uid
            && !deleting(&after)
            && after["metadata"]["finalizers"] == replacement["metadata"]["finalizers"]
            && after["spec"] == replacement["spec"],
        "cleanup changed the replacement Backup",
    )?;
    ensure(
        registry_metadata(&objects).await? == registry_before
            && stored_phase(&objects, &transition).await?.1.as_deref() == Some("Cleaning")
            && releasing(&registry.value.claims, &uid),
        "the refusal changed the registry or released the original claim",
    )?;
    scenario.recorder.check(
        "replacement-uid-refused-with-original-claim-retained",
        &json!({
            "hold": installed,
            "transition": transition,
            "originalUid": uid,
            "replacementUid": replacement_uid,
            "refusal": REPLACEMENT,
            "registry": registry_before,
        }),
    );
    Ok(())
}

/// Delete the claimed `Backup` while it still has `uid` and recreate it
/// with the predecessor's declaration and field manager; returns its new UID.
async fn replace_backup(objects: &Objects, pair: &CleanupPair, uid: &str) -> Result<String> {
    let current = backup(objects, CLEANUP_NAME)
        .await?
        .filter(|backup| text(backup, "/metadata/uid") == uid && !deleting(backup))
        .ok_or_else(|| Error::Check("the claimed Backup changed before replacement".into()))?;
    objects.delete(&current).await?;
    wait::until(
        "the original Backup to be deleted",
        Duration::from_secs(60),
        async || Ok(backup(objects, CLEANUP_NAME).await?.is_none().then_some(())),
    )
    .await?;
    let desired = pair
        .predecessor
        .plan
        .ir
        .resources
        .iter()
        .find(|resource| resource.kind == "Backup" && resource.name == CLEANUP_NAME)
        .map(|resource| resource.desired.clone())
        .ok_or_else(|| Error::Check("the predecessor declares no Backup".into()))?;
    let replacement = objects
        .apply(&desired, &format!("apsides-{DOMAIN}"))
        .await?;
    let replacement_uid = text(&replacement, "/metadata/uid").to_owned();
    ensure(
        !replacement_uid.is_empty() && replacement_uid != uid,
        "the replacement Backup has no new UID",
    )?;
    Ok(replacement_uid)
}

/// Whether the Backup's claim is Releasing for exactly `uid`.
fn releasing(claims: &[deploy::ProtectedClaim], uid: &str) -> bool {
    matches!(claims
        .iter()
        .filter(|claim| claim.identity.name == CLEANUP_NAME)
        .collect::<Vec<_>>()
        .as_slice(), [claim] if claim.state == ClaimState::Releasing && claim.uid.as_deref() == Some(uid))
}

/// The unclaimed `Backup` is untouched and the successor's workload Role
/// grants nothing on `Backup`s.
async fn unrelated_preserved(scenario: &mut Scenario<'_>, platform: &Platform) -> Result {
    let objects = scenario.objects.clone();
    let unclaimed = backup(&objects, UNCLAIMED)
        .await?
        .ok_or_else(|| Error::Check("cleanup deleted the unclaimed Backup".into()))?;
    ensure(
        !deleting(&unclaimed)
            && unclaimed["spec"]["cluster"]["name"] == "cleanup-unconfigured-database",
        "cleanup affected the unclaimed Backup",
    )?;
    let registry = platform.registry().await?;
    let account = registry
        .value
        .transitions
        .values()
        .max_by_key(|stored| stored.allocation().sequence)
        .map(|stored| stored.allocation().service_account_name.clone())
        .ok_or_else(|| Error::Check("no successor transition".into()))?;
    let roles = runtime_roles(&objects, &account).await?;
    scenario.recorder.check(
        "unclaimed-backup-preserved-and-no-backup-access",
        &json!({
            "unclaimedUid": unclaimed["metadata"]["uid"],
            "successorAccount": account,
            "successorWorkloadRoles": roles,
        }),
    );
    Ok(())
}

/// The names of the workload Roles bound to `account`, after checking none
/// grants anything on `Backup`s.
async fn runtime_roles(objects: &Objects, account: &str) -> Result<Vec<String>> {
    let roles = objects
        .list(
            "rbac.authorization.k8s.io/v1",
            "Role",
            Some(WORKLOAD_NAMESPACE),
            None,
        )
        .await?;
    let mut names = Vec::new();
    for binding in objects
        .list(
            "rbac.authorization.k8s.io/v1",
            "RoleBinding",
            Some(WORKLOAD_NAMESPACE),
            None,
        )
        .await?
    {
        let bound = binding["subjects"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|subject| {
                subject["kind"] == "ServiceAccount"
                    && subject["name"] == account
                    && subject["namespace"] == CONTROL_NAMESPACE
            });
        if !bound {
            continue;
        }
        let name = text(&binding, "/roleRef/name");
        let role = roles
            .iter()
            .find(|role| text(role, "/metadata/name") == name)
            .filter(|_| binding["roleRef"]["kind"] == "Role")
            .ok_or_else(|| Error::Check("a successor binding names no workload Role".into()))?;
        let grants_backups = role["rules"].as_array().into_iter().flatten().any(|rule| {
            let group = rule["apiGroups"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|group| group == "*" || group == CLEANUP_GROUP);
            let resource = rule["resources"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|resource| {
                    resource == "*"
                        || resource == CLEANUP_RESOURCE
                        || resource.starts_with("backups/")
                        || resource.starts_with("*/")
                });
            group && resource
        });
        ensure(
            !grants_backups,
            "the successor's workload Role grants Backup access",
        )?;
        names.push(name.to_owned());
    }
    ensure(
        !names.is_empty(),
        "the successor has no workload Role binding",
    )?;
    names.sort();
    Ok(names)
}

async fn backup(objects: &Objects, name: &str) -> Result<Option<Value>> {
    objects
        .get(
            "postgresql.cnpg.io/v1",
            "Backup",
            Some(WORKLOAD_NAMESPACE),
            name,
        )
        .await
}
