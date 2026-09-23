//! Ownership collision between two domains.

use crate::support::committed_report;
use acceptance::{
    Result, aps, ensure,
    objects::text,
    observe::{metadata_rows, object_inventory, registry_metadata},
    pair::{self, Candidate, DOMAIN, WORKLOAD_NAMESPACE},
    scenario::{self, Scenario, note},
    wait,
};
use serde_json::json;
use std::{collections::BTreeMap, time::Duration};

/// How reservation refuses a target another domain claims.
const COLLISION: &str = "new target collides with a protected claim";

/// Spec §28 "Claim an object another domain owns": with the registry
/// bootstrapped for two domains, `aps deploy` of the `cnpg-owner` program
/// commits and claims `default/api-config`. `aps deploy` of the CNPG initial
/// release, another domain declaring the same `ConfigMap`, must be refused
/// for the claim collision and change nothing: the registry object and
/// state, every object in the control and workload namespaces, the owner's
/// `ConfigMap` and every metadata database row are as before, and no
/// transition or writer of the contender exists.
#[tokio::test]
#[ignore = "creates a kind cluster; needs Docker, kind and `cuenv task acceptancePrepare`"]
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
    let platform = scenario.prepare(&[owner, contender], &policies).await?;
    let objects = scenario.objects.clone();
    note("deploying the owner");
    let run = scenario
        .deploy(&platform, &owner.root, "deploy-owner")
        .await?;
    let committed = aps::committed(&run, &owner.release_digest, &owner.manifest.ir_digest)?;
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
