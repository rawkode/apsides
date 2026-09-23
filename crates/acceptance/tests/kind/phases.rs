//! Phase interruption and recovery.

use crate::{
    support::{resume_commits, stop_at, writer_checkpoint, writers_healthy},
    upgrade::create_api_settings,
};
use acceptance::{
    Result, aps,
    bootstrap::Platform,
    cnpg, ensure,
    objects::Objects,
    pair::{Candidate, DOMAIN, Pair, WORKLOAD_NAMESPACE},
    scenario::{self, Scenario, note},
    wait,
};
use deploy::Phase;
use serde_json::json;
use std::{collections::BTreeMap, time::Duration};

/// Spec §28 "Interrupt each transition phase and resume": `aps deploy` is
/// stopped right after its registry write into each phase an upgrade
/// persists before it commits (Planned, Staged, Quiesced, Activated,
/// Cleaning), by a hold that denies the deployer's next registry update and
/// a SIGKILL of `aps` once the phase is stored. The same release rerun with
/// the same executor must commit the same transition with no new
/// allocation, leave no unfinished transition, report the same inventory
/// when repeated, run exactly the new generation's writers with no Pod of
/// the old one, and keep the CNPG Cluster Ready.
///
/// The upgrades alternate between the initial release and the successor on
/// one cluster. The Committed-to-inventory gap is covered in process (see
/// docs/reference/ACCEPTANCE.md).
#[tokio::test]
#[ignore = "creates a kind cluster; needs Docker, kind and `cuenv task acceptancePrepare`"]
async fn every_interrupted_phase_resumes_to_commit() -> Result {
    scenario::run("phase-interruption", async |scenario| {
        let pair = Pair::load(
            &scenario.inputs.initial_release,
            &scenario.inputs.successor_release,
        )?;
        interrupt_every_phase(scenario, &pair).await
    })
    .await
}

async fn interrupt_every_phase(scenario: &mut Scenario<'_>, pair: &Pair) -> Result {
    let policies = BTreeMap::from([(DOMAIN.to_owned(), pair.successor_policy.clone())]);
    let platform = scenario
        .prepare(&[&pair.initial, &pair.successor], &policies)
        .await?;
    let objects = scenario.objects.clone();
    create_api_settings(&objects).await?;
    let run = scenario
        .deploy(&platform, &pair.initial.root, "deploy-initial")
        .await?;
    aps::committed(
        &run,
        &pair.initial.release_digest,
        &pair.initial.manifest.ir_digest,
    )?;
    cnpg::wait_ready(&objects, WORKLOAD_NAMESPACE).await?;
    for (candidate, phase) in [
        (&pair.successor, Phase::Planned),
        (&pair.initial, Phase::Staged),
        (&pair.successor, Phase::Quiesced),
        (&pair.initial, Phase::Activated),
        (&pair.successor, Phase::Cleaning),
    ] {
        let label = format!("{phase:?}").to_lowercase();
        note(&format!("interrupting after {phase:?}"));
        if candidate.release_digest == pair.successor.release_digest {
            remove_orphaned_api(&objects, &platform).await?;
        }
        let previous = writer_checkpoint(&platform.registry().await?)?
            .runtime_binding
            .service_account_name
            .clone();
        let stopped = stop_at(scenario, &platform, candidate, phase, &label).await?;
        note(&format!("resuming after {phase:?}"));
        let resumed = resume_commits(scenario, &platform, candidate, &stopped, &label).await?;
        let health = writers_healthy(&objects, &platform, Some(&previous)).await?;
        let api = api_rolled_out(&objects, candidate, pair).await?;
        scenario.recorder.check(
            &format!("interrupted-after-{label}-resumes-to-commit"),
            &json!({
                "release": candidate.release_digest,
                "stop": stopped.evidence,
                "resumed": resumed,
                "health": health,
                "previousRuntimeAccount": previous,
                "api": api,
            }),
        );
    }
    Ok(())
}

/// Delete the `database-api` Deployment and Service a downgrade to the
/// initial release orphaned, so the successor can create them again. Their
/// claims must already be released.
async fn remove_orphaned_api(objects: &Objects, platform: &Platform) -> Result {
    let registry = platform.registry().await?;
    ensure(
        !registry
            .value
            .claims
            .iter()
            .any(|claim| claim.identity.name == "database-api"),
        "the API objects are still claimed",
    )?;
    for (api_version, kind) in [("apps/v1", "Deployment"), ("v1", "Service")] {
        if let Some(object) = objects
            .get(api_version, kind, Some(WORKLOAD_NAMESPACE), "database-api")
            .await?
        {
            objects.delete(&object).await?;
        }
        wait::until(
            "the orphaned API object to be deleted",
            Duration::from_secs(120),
            async || {
                Ok(objects
                    .get(api_version, kind, Some(WORKLOAD_NAMESPACE), "database-api")
                    .await?
                    .is_none()
                    .then_some(()))
            },
        )
        .await?;
    }
    Ok(())
}

/// When `candidate` is the successor, its API Deployment rolls out.
async fn api_rolled_out(
    objects: &Objects,
    candidate: &Candidate,
    pair: &Pair,
) -> Result<serde_json::Value> {
    if candidate.release_digest != pair.successor.release_digest {
        return Ok(serde_json::Value::Null);
    }
    let deployment = wait::until(
        "the API Deployment to roll out",
        Duration::from_secs(300),
        async || {
            let deployment = objects
                .require(
                    "apps/v1",
                    "Deployment",
                    Some(WORKLOAD_NAMESPACE),
                    "database-api",
                )
                .await?;
            Ok(cnpg::rolled_out(&deployment).then_some(deployment))
        },
    )
    .await?;
    Ok(json!({"deploymentUid": deployment["metadata"]["uid"]}))
}
