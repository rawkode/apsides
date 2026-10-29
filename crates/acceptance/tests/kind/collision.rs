//! Ownership collision between two domains, and live preview by the first.

use crate::support::committed_report;
use acceptance::{
    Result,
    aps::{self, Run},
    bootstrap::Platform,
    ensure,
    hold::Hold,
    objects::text,
    observe::{metadata_rows, object_inventory, object_versions, registry_metadata},
    pair::{self, CONTROL_NAMESPACE, Candidate, DOMAIN, WORKLOAD_NAMESPACE},
    scenario::{self, Scenario, note},
    wait,
};
use coordination::ReviewIdentity;
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};

/// How reservation refuses a target another domain claims.
const COLLISION: &str = "new target collides with a protected claim";

/// Spec §28 "Claim an object another domain owns": with the registry
/// bootstrapped for two domains, `aps deploy` of the `cnpg-owner` program
/// commits and claims `default/api-config`.
///
/// The owner's deployment identity approves live preview of its one
/// `ConfigMap`. Under a hold that denies every registry update by the
/// deployer, `aps deploy` of the owner runs its preview and is then refused
/// at the reservation; the registry and every object in the control and
/// workload namespaces keep their UID and resourceVersion, and `api-config`
/// does not exist.
/// Without the hold, the committed report carries the live preview of
/// `api-config`.
/// `aps deploy` of the CNPG initial
/// release, another domain declaring the same `ConfigMap`, must be refused
/// for the claim collision and change nothing: the registry object and
/// state, every object in the control and workload namespaces, the owner's
/// `ConfigMap` and every metadata database row are as before, and no
/// transition or writer of the contender exists.
#[tokio::test]
#[ignore = "creates a kind cluster; needs kind on podman or Docker and `cuenv task acceptancePrepare`"]
async fn a_claimed_object_cannot_be_claimed_by_another_domain() -> Result {
    scenario::run("ownership-collision", async |scenario| {
        let owner = Candidate::read(&scenario.inputs.owner_release)?;
        let contender = Candidate::read(&scenario.inputs.initial_release)?;
        collide(scenario, &owner, &contender).await
    })
    .await
}

async fn collide(scenario: &mut Scenario<'_>, owner: &Candidate, contender: &Candidate) -> Result {
    let owner_domain = owner.plan.ir.domain.id.clone();
    ensure(
        owner_domain != DOMAIN && contender.plan.ir.domain.id == DOMAIN,
        "the collision pair does not name two domains",
    )?;
    let policies = BTreeMap::from([
        (owner_domain.clone(), pair::policy(&owner.plan)),
        (DOMAIN.to_owned(), pair::policy(&contender.plan)),
    ]);
    let mut platform = scenario.prepare(&[owner, contender], &policies).await?;
    let objects = scenario.objects.clone();
    let approval = platform
        .approve_preview(&owner_domain, preview_targets(owner)?)
        .await?;
    note("previewing the owner under a reservation hold");
    let previewed = preview_changes_nothing(scenario, &platform, owner).await?;
    note("deploying the owner");
    let run = scenario
        .deploy(&platform, &owner.root, "deploy-owner")
        .await?;
    let committed = aps::committed(&run, &owner.release_digest, &owner.manifest.ir_digest)?;
    let preview = live_preview(&run, &approval)?;
    scenario.recorder.check(
        "owner-preview-mutates-nothing-and-is-reported",
        &json!({"approval": approval, "refusedAtReservation": previewed, "livePreview": preview}),
    );
    let claimed = wait::until(
        "the owner's controller to create api-config",
        Duration::from_secs(180),
        async || {
            Ok(objects
                .get("v1", "ConfigMap", Some(WORKLOAD_NAMESPACE), "api-config")
                .await?
                .filter(|claimed| claimed["data"]["owner"] == "database-owner"))
        },
    )
    .await?;
    let registry = platform.registry().await?;
    ensure(
        registry
            .value
            .claims
            .iter()
            .any(|claim| claim.identity.name == "api-config" && claim.domain_id == owner_domain),
        "the owner does not claim api-config",
    )?;
    let before = json!({
        "registry": registry_metadata(&objects).await?,
        "objects": object_inventory(&objects).await?,
        "metadata": metadata_rows(&scenario.database)?,
        "owned": {"uid": claimed["metadata"]["uid"], "resourceVersion": claimed["metadata"]["resourceVersion"]},
    });

    note("deploying the contender");
    let run = scenario
        .deploy(&platform, &contender.root, "deploy-contender")
        .await?;
    aps::refused(&run, COLLISION)?;
    let after_registry = platform.registry().await?;
    let claimed_after = objects
        .require("v1", "ConfigMap", Some(WORKLOAD_NAMESPACE), "api-config")
        .await?;
    let after = json!({
        "registry": registry_metadata(&objects).await?,
        "objects": object_inventory(&objects).await?,
        "metadata": metadata_rows(&scenario.database)?,
        "owned": {"uid": claimed_after["metadata"]["uid"], "resourceVersion": claimed_after["metadata"]["resourceVersion"]},
    });
    ensure(
        after == before && after_registry.value == registry.value,
        "the refused contender changed Kubernetes or metadata state",
    )?;
    ensure(
        !after_registry
            .value
            .transitions
            .values()
            .any(|stored| stored.domain_id() == DOMAIN)
            && !after_registry
                .value
                .active_writer_checkpoints
                .contains_key(DOMAIN)
            && text(&claimed_after, "/data/owner") == "database-owner",
        "the contender reserved a transition or has a writer",
    )?;
    scenario.recorder.check(
        "claim-collision-refused-without-state-change",
        &json!({
            "owner": committed_report(&committed),
            "refusal": COLLISION,
            "unchanged": before,
        }),
    );
    Ok(())
}

/// `aps deploy` of `owner` under a reservation hold: its live preview runs,
/// then the reservation's registry write is denied, and no object the
/// preview could reach has a new UID or resourceVersion. Metadata database tables the
/// reservation wrote before its denied registry write are reported, not
/// required unchanged: the preview reads only Kubernetes and the review it
/// projects in memory.
async fn preview_changes_nothing(
    scenario: &mut Scenario<'_>,
    platform: &Platform,
    owner: &Candidate,
) -> Result<Value> {
    let objects = scenario.objects.clone();
    let hold = Hold::reservation(&objects, CONTROL_NAMESPACE, WORKLOAD_NAMESPACE);
    let installed = hold.activate().await?;
    let registry = platform.registry().await?;
    let before = json!({
        "registry": registry_metadata(&objects).await?,
        "objects": object_versions(&objects).await?,
    });
    let rows = metadata_rows(&scenario.database)?;
    let run = scenario
        .deploy(platform, &owner.root, "deploy-owner-held")
        .await?;
    ensure(
        !run.success && hold.denied(&run.stderr),
        format!(
            "the held deployment was not refused at its reservation: {}",
            test_cluster::process::tail(&run.stderr, 2000)
        ),
    )?;
    let after = json!({
        "registry": registry_metadata(&objects).await?,
        "objects": object_versions(&objects).await?,
    });
    ensure(
        after == before && platform.registry().await?.value == registry.value,
        format!(
            "the previewed deployment changed Kubernetes state: before {before}, after {after}"
        ),
    )?;
    ensure(
        objects
            .get("v1", "ConfigMap", Some(WORKLOAD_NAMESPACE), "api-config")
            .await?
            .is_none(),
        "the preview created api-config",
    )?;
    hold.release().await?;
    let rows_after = metadata_rows(&scenario.database)?;
    let written = rows_after
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(table, digest)| rows.get(table.as_str()) != Some(*digest))
        .map(|(table, _)| table.clone())
        .collect::<Vec<_>>();
    Ok(json!({
        "hold": installed,
        "unchanged": before,
        "metadataTablesWrittenBeforeTheDeniedReservation": written,
    }))
}

/// The live preview a committed report carries: fresh, before the
/// reservation, of exactly the approved targets.
fn live_preview(run: &Run, approval: &Value) -> Result<Value> {
    let report: Value = serde_json::from_str(&run.stdout)?;
    let live = &report["review"]["liveImpact"];
    let previewed = live["report"]["resources"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|resource| resource["identity"].clone())
        .collect::<Vec<_>>();
    ensure(
        live["status"] == "available"
            && live["observationContext"] == "fresh-pre-reservation"
            && live["report"]["schemaVersion"] == "apsides.dev/live-preview/v1"
            && !text(live, "/report/planDigest").is_empty()
            && Value::Array(previewed) == approval["targets"],
        format!("the committed report does not carry the live preview: {live}"),
    )?;
    Ok(json!({
        "status": live["status"],
        "observationContext": live["observationContext"],
        "planDigest": live["report"]["planDigest"],
        "resources": live["report"]["resources"],
        "conditions": live["report"]["conditions"],
    }))
}

/// Every object `candidate` manages, as a preview approval names it.
fn preview_targets(candidate: &Candidate) -> Result<Vec<ReviewIdentity>> {
    candidate
        .plan
        .ir
        .resources
        .iter()
        .map(|resource| {
            let identity = deploy::resource_identity(resource)?;
            Ok(ReviewIdentity {
                api_group: identity.api_group,
                resource: identity.resource,
                namespace: identity.namespace,
                name: identity.name,
            })
        })
        .collect()
}
