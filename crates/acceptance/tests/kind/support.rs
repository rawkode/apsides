//! Steps several scenarios share: stopping `aps deploy` at a phase, resuming
//! it, and reading back what the registry records.

use acceptance::{
    Error, Result,
    aps::{self, Committed},
    bootstrap::Platform,
    cnpg, ensure,
    hold::Hold,
    objects::{Objects, text},
    pair::{CONTROL_NAMESPACE, Candidate, DOMAIN, WORKLOAD_NAMESPACE},
    scenario::{self, Scenario},
    wait, workload,
};
use deploy::Phase;
use platform::{NamespaceRegistry, Versioned, WriterCheckpoint};
use serde_json::{Value, json};
use std::time::Duration;
use test_cluster::process::Ending;

/// How long a stopped run may take to reach its phase.
const REACH: Duration = Duration::from_secs(300);
/// How often a running `aps` is checked while it approaches its stop.
const STOP_POLL: Duration = Duration::from_millis(200);
/// How long the registry must stay unchanged after a stop.
const SETTLE: Duration = Duration::from_secs(5);

/// The report fields a scenario records for a commit.
pub fn committed_report(committed: &Committed) -> Value {
    json!({
        "status": committed.status,
        "artifactTrust": committed.artifact_trust,
        "releaseDigest": committed.release_digest,
        "irDigest": committed.ir_digest,
        "transition": committed.transition,
        "generation": committed.generation,
        "inventory": {"revision": committed.inventory.revision, "snapshotId": committed.inventory.snapshot_id},
    })
}

/// The domain's active writer checkpoint.
pub fn writer_checkpoint(registry: &Versioned<NamespaceRegistry>) -> Result<&WriterCheckpoint> {
    registry
        .value
        .active_writer_checkpoints
        .get(DOMAIN)
        .ok_or_else(|| Error::Check("the domain has no active writer checkpoint".into()))
}

/// `(apiVersion, kind, namespace, name)` of a recorded identity key such as
/// `rbac.authorization.k8s.io/v1/RoleBinding/<namespace>/<name>`.
pub fn identity_key(key: &str) -> Result<(String, String, String, String)> {
    let parts = key.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        ["v1", kind, namespace, name] => Ok((
            "v1".into(),
            (*kind).into(),
            (*namespace).into(),
            (*name).into(),
        )),
        [group, version, kind, namespace, name] => Ok((
            format!("{group}/{version}"),
            (*kind).into(),
            (*namespace).into(),
            (*name).into(),
        )),
        _ => Err(Error::Check(format!(
            "recorded identity key {key} is invalid"
        ))),
    }
}

/// The registry `ConfigMap` and the phase it stores for `transition`, read
/// as raw JSON so a concurrent `aps` run never contends for the metadata
/// database.
pub async fn stored_phase(objects: &Objects, transition: &str) -> Result<(String, Option<String>)> {
    let registry = objects
        .require(
            "v1",
            "ConfigMap",
            Some(CONTROL_NAMESPACE),
            &format!("apsides-registry-{WORKLOAD_NAMESPACE}"),
        )
        .await?;
    let state: Value = serde_json::from_str(text(&registry, "/data/state.json"))?;
    let phase = state
        .pointer(&format!("/transitions/{transition}/record/phase"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok((
        text(&registry, "/metadata/resourceVersion").to_owned(),
        phase,
    ))
}

/// A deployment stopped right after its transition's registry write into a
/// phase.
pub struct Stopped {
    pub transition: String,
    pub generation: String,
    /// The registry's `next_sequence` before the run, which reserved it.
    pub sequence: u64,
    pub evidence: Value,
}

/// Run `aps deploy` of `candidate` under a hold on its transition's stored
/// `phase`, SIGKILL it as soon as the registry records that phase (or reap
/// it if the hold's denial already ended it), and lift the hold.
///
/// # Errors
/// Fails when the phase is not reached, the run ends for another reason, or
/// the registry changes after the stop.
pub async fn stop_at(
    scenario: &mut Scenario<'_>,
    platform: &Platform,
    candidate: &Candidate,
    phase: Phase,
    label: &str,
) -> Result<Stopped> {
    let registry = platform.registry().await?;
    let sequence = registry.value.next_sequence;
    let next = deploy::allocation_names(&registry.value.registry_uid, sequence)?;
    let hold = Hold::registry(
        &scenario.objects,
        CONTROL_NAMESPACE,
        WORKLOAD_NAMESPACE,
        &next.transition_id,
        phase,
    );
    let installed = hold.activate().await?;
    let expected = format!("{phase:?}");
    let mut running = scenario.spawn(platform, &candidate.root).await?;
    let deadline = tokio::time::Instant::now() + REACH;
    let reached = loop {
        let (_, stored) = stored_phase(&scenario.objects, &next.transition_id).await?;
        if stored.as_deref() == Some(expected.as_str()) {
            break true;
        }
        if running.exited()?.is_some() || tokio::time::Instant::now() >= deadline {
            break false;
        }
        tokio::time::sleep(STOP_POLL).await;
    };
    let (ending, output) = running.kill().await?;
    scenario.document(label, &output);
    let (resource_version, stored) = stored_phase(&scenario.objects, &next.transition_id).await?;
    ensure(
        reached && stored.as_deref() == Some(expected.as_str()),
        format!(
            "aps deploy did not stop at {expected} (stored phase {stored:?}): {}",
            test_cluster::process::tail(&output.stderr, 2000)
        ),
    )?;
    if let Ending::Exited { success } = ending {
        ensure(
            !success && hold.denied(&output.stderr),
            "aps deploy ended at the phase for a reason other than the hold",
        )?;
    }
    tokio::time::sleep(SETTLE).await;
    ensure(
        stored_phase(&scenario.objects, &next.transition_id).await?
            == (resource_version.clone(), stored),
        "the registry changed after the stopped run ended",
    )?;
    hold.release().await?;
    Ok(Stopped {
        evidence: json!({
            "phase": expected,
            "transition": next.transition_id,
            "hold": installed,
            "ending": scenario::ending(ending),
            "holdDenialSeen": hold.denied(&output.stderr),
            "registryResourceVersion": resource_version,
        }),
        transition: next.transition_id,
        generation: next.generation_id,
        sequence,
    })
}

/// Rerun `aps deploy` of `candidate` with the same executor after `stopped`
/// and require it to commit the same transition without a new allocation,
/// and a second rerun to report it already committed with the same
/// inventory.
///
/// # Errors
/// Fails when either run reports anything else or the registry records
/// another allocation or an unfinished transition.
pub async fn resume_commits(
    scenario: &mut Scenario<'_>,
    platform: &Platform,
    candidate: &Candidate,
    stopped: &Stopped,
    label: &str,
) -> Result<Value> {
    let run = scenario
        .deploy(platform, &candidate.root, &format!("{label}-resumed"))
        .await?;
    let committed = aps::committed(
        &run,
        &candidate.release_digest,
        &candidate.manifest.ir_digest,
    )?;
    ensure(
        committed.transition == stopped.transition && committed.generation == stopped.generation,
        "the resumed run committed another transition",
    )?;
    let registry = platform.registry().await?;
    let unfinished = registry
        .value
        .transitions
        .values()
        .filter(|stored| stored.record.phase != Phase::Committed)
        .count();
    let same_generation = registry
        .value
        .transitions
        .values()
        .filter(|stored| stored.allocation().generation_id == stopped.generation)
        .count();
    ensure(
        registry.value.next_sequence == stopped.sequence + 1
            && unfinished == 0
            && same_generation == 1
            && registry
                .value
                .histories
                .get(DOMAIN)
                .and_then(|history| history.active_allocation.as_ref())
                .is_some_and(|active| active.generation_id == stopped.generation),
        "the resumed run left a duplicate allocation, an unfinished transition or another active generation",
    )?;
    let rerun = scenario
        .deploy(platform, &candidate.root, &format!("{label}-repeated"))
        .await?;
    let repeated = aps::already_committed(&rerun, &candidate.release_digest, &stopped.transition)?;
    ensure(
        repeated.inventory == committed.inventory,
        "repeating the committed deployment changed its inventory",
    )?;
    Ok(json!({
        "report": committed_report(&committed),
        "nextSequence": registry.value.next_sequence,
        "repeatedInventory": {"revision": repeated.inventory.revision, "snapshotId": repeated.inventory.snapshot_id},
    }))
}

/// The active generation's controller and proxy each run one ready Pod, and
/// no Pod of `retired` (the previous generation's runtime account) is still
/// running.
///
/// # Errors
/// Fails when the writers do not become ready in time or an old writer Pod
/// remains.
pub async fn writers_healthy(
    objects: &Objects,
    platform: &Platform,
    retired: Option<&str>,
) -> Result<Value> {
    let registry = platform.registry().await?;
    let checkpoint = writer_checkpoint(&registry)?;
    let runtime = checkpoint.runtime_binding.service_account_name.clone();
    let proxy = checkpoint.proxy_identity.deployment_name.clone();
    let writers = wait::until(
        "the active generation's writers to be ready",
        Duration::from_secs(180),
        async || {
            Ok(Some(
                workload::writer_identity(objects, CONTROL_NAMESPACE, &runtime, &proxy).await?,
            ))
        },
    )
    .await?;
    if let Some(retired) = retired {
        let accounts = [retired.to_owned(), format!("{retired}-process")];
        let remaining = objects
            .list("v1", "Pod", Some(CONTROL_NAMESPACE), None)
            .await?
            .into_iter()
            .filter(|pod| {
                accounts
                    .iter()
                    .any(|account| text(pod, "/spec/serviceAccountName") == account)
                    && !matches!(text(pod, "/status/phase"), "Succeeded" | "Failed")
            })
            .count();
        ensure(
            remaining == 0,
            "a Pod of the previous writer is still running",
        )?;
    }
    let cluster = wait::until(
        "the CNPG Cluster to be Ready",
        Duration::from_secs(300),
        async || {
            let cluster = objects
                .require(
                    "postgresql.cnpg.io/v1",
                    "Cluster",
                    Some(WORKLOAD_NAMESPACE),
                    "database",
                )
                .await?;
            Ok(cnpg::condition(&cluster, "Ready").then_some(cluster))
        },
    )
    .await?;
    Ok(json!({"writers": writers, "cnpgClusterUid": cluster["metadata"]["uid"]}))
}

/// No Secret value, rollout key or token the scenario knows appears in `aps`
/// output, the controllers' logs collected so far or the inventory database
/// files; recorded as `check`.
pub async fn nothing_leaks(scenario: &mut Scenario<'_>, check: &str) -> Result {
    scenario.secret_canaries().await?;
    let mut stores = Vec::new();
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut path = scenario.database.clone().into_os_string();
        path.push(suffix);
        if let Ok(bytes) = std::fs::read(&path) {
            stores.push((format!("inventory.db{suffix}"), bytes));
        }
    }
    ensure(!stores.is_empty(), "the inventory database was not written")?;
    let leaks = scenario.recorder.leaks(&stores);
    let (searched, too_short) = scenario.recorder.canary_counts();
    ensure(
        leaks.is_empty(),
        format!("a Secret value, rollout key or token appears in {leaks:?}"),
    )?;
    scenario.recorder.check(
        check,
        &json!({
            "canariesSearched": searched,
            "canariesTooShort": too_short,
            "inventoryFiles": stores.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            "occurrences": 0,
        }),
    );
    Ok(())
}
