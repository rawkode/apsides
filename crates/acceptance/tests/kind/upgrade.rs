//! P0: the CNPG upgrade through the public CLI.

use crate::support::{committed_report, identity_key, nothing_leaks, writer_checkpoint};
use acceptance::{
    Error, Result,
    aps::{self, Committed},
    bootstrap::Platform,
    cnpg::{self, DatabaseIdentity},
    ensure,
    objects::{Objects, text},
    observe::{object_inventory, registry_metadata},
    pair::{self, CONTROL_NAMESPACE, DOMAIN, Pair, WORKLOAD_NAMESPACE},
    scenario::{self, Scenario, note},
    workload::{self, Workload},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};

/// P0, spec §28 "Add a capability above the ceiling", "Upgrade to a build
/// with different permissions", "Reconcile unchanged ConfigMap/Secret input
/// repeatedly" and "Inspect artifacts, logs, metrics, and inventory":
///
/// (a) under a ceiling that allows only the initial release, `aps deploy`
///     of the initial release commits;
/// (b) `aps deploy` of the successor is rejected for the capability the
///     ceiling lacks, before any successor object exists, with the registry
///     and the running initial writer untouched;
/// (c) once the ceiling is widened, `aps deploy` of the successor commits a
///     new generation;
/// (d) the old generation's `RoleBinding`s and `ServiceAccount` are retired and
///     the new `ServiceAccount` is distinct;
/// (e) a database row written before the upgrade survives, with the CNPG
///     Cluster and volume claim UIDs unchanged;
/// (f) a `ConfigMap` change rolls the API out, equal data causes no rollout
///     for 30 s, and a Secret change rolls it out and back;
/// (g) no Secret value, rollout key or issuer token appears in `aps` output,
///     the inventory database or the controllers' logs.
#[tokio::test]
#[ignore = "creates a kind cluster; needs Docker, kind and `cuenv task acceptancePrepare`"]
async fn cnpg_upgrade_through_the_public_cli() -> Result {
    scenario::run("cnpg-upgrade", async |scenario| {
        let pair = Pair::load(
            &scenario.inputs.initial_release,
            &scenario.inputs.successor_release,
        )?;
        Upgrade { pair: &pair }.run(scenario).await
    })
    .await
}

/// The release pair every step shares.
struct Upgrade<'a> {
    pair: &'a Pair,
}

/// What the initial generation left for later steps to compare against.
struct Initial {
    committed: Committed,
    database: DatabaseIdentity,
    cluster_uid: String,
    runtime_account: String,
    retirement: Vec<(String, String)>,
}

impl Upgrade<'_> {
    async fn run(&self, scenario: &mut Scenario<'_>) -> Result {
        let policies = BTreeMap::from([(DOMAIN.to_owned(), self.pair.initial_policy.clone())]);
        let mut platform = scenario
            .prepare(&[&self.pair.initial, &self.pair.successor], &policies)
            .await?;
        note("(a) deploying the initial release");
        let initial = self.initial_commits(scenario, &platform).await?;
        note("(b) deploying the successor under the initial ceiling");
        self.successor_rejected(scenario, &platform, &initial)
            .await?;
        note("(c) widening the ceiling and deploying the successor");
        let change = platform
            .replace_policy(DOMAIN, &self.pair.successor_policy)
            .await?;
        let successor = self
            .successor_commits(scenario, &platform, &initial, change)
            .await?;
        note("(d) checking the old generation's identity is retired");
        old_identity_retired(scenario, &platform, &initial, &successor).await?;
        note("(e) checking the database survived");
        let preserved = cnpg::persistence(
            &scenario.objects,
            WORKLOAD_NAMESPACE,
            Some(&initial.database),
        )
        .await?;
        ensure(
            preserved.cluster_uid == initial.cluster_uid,
            "the CNPG Cluster was replaced",
        )?;
        scenario.recorder.check(
            "e-row-and-volumes-preserved",
            &json!({"publicRowPreserved": true, "identity": preserved}),
        );
        note("(f) exercising the input reactions");
        self.inputs_roll_out(scenario).await?;
        note("(g) scanning for secret values");
        scenario.controller_logs("successor").await?;
        nothing_leaks(scenario, "g-secret-and-rollout-key-canary-scan").await
    }

    /// (a)
    async fn initial_commits(
        &self,
        scenario: &mut Scenario<'_>,
        platform: &Platform,
    ) -> Result<Initial> {
        let initial = &self.pair.initial;
        let run = scenario
            .deploy(platform, &initial.root, "deploy-initial")
            .await?;
        let committed = aps::committed(&run, &initial.release_digest, &initial.manifest.ir_digest)?;
        let objects = scenario.objects.clone();
        let ready = cnpg::wait_ready(&objects, WORKLOAD_NAMESPACE).await?;
        scenario.secret_canaries().await?;
        let database = cnpg::persistence(&objects, WORKLOAD_NAMESPACE, None).await?;
        create_api_settings(&objects).await?;
        let registry = platform.registry().await?;
        let checkpoint = writer_checkpoint(&registry)?;
        ensure(
            checkpoint.allocation.generation_id == committed.generation
                && checkpoint.allocation.transition_id == committed.transition,
            "the active writer is not the committed initial generation",
        )?;
        scenario.recorder.check(
            "a-initial-commits-under-restricted-ceiling",
            &json!({"report": committed_report(&committed), "cnpg": ready, "database": database}),
        );
        Ok(Initial {
            cluster_uid: text(&ready, "/clusterUid").to_owned(),
            runtime_account: checkpoint.runtime_binding.service_account_name.clone(),
            retirement: checkpoint
                .retirement_identity_uids
                .iter()
                .map(|(key, uid)| (key.clone(), uid.clone()))
                .collect(),
            committed,
            database,
        })
    }

    /// (b)
    /// One `aps deploy` of the successor under the initial policy is rejected
    /// and leaves the registry's state and resourceVersion, every object and
    /// the initial writer untouched.
    async fn successor_rejected(
        &self,
        scenario: &mut Scenario<'_>,
        platform: &Platform,
        initial: &Initial,
    ) -> Result {
        let objects = scenario.objects.clone();
        let registry = platform.registry().await?;
        let checkpoint = writer_checkpoint(&registry)?;
        let runtime = checkpoint.runtime_binding.service_account_name.clone();
        let proxy = checkpoint.proxy_identity.deployment_name.clone();
        let sequence = registry.value.next_sequence;
        let next = deploy::allocation_names(&registry.value.registry_uid, sequence)?;
        let writers =
            workload::writer_identity(&objects, CONTROL_NAMESPACE, &runtime, &proxy).await?;
        let before = object_inventory(&objects).await?;
        require_successor_absent(&objects, &next).await?;

        let registry_object = registry_metadata(&objects).await?;
        let run = scenario
            .deploy(
                platform,
                &self.pair.successor.root,
                "deploy-successor-rejected",
            )
            .await?;
        aps::refused(&run, &pair::rejection(&self.pair.missing))?;
        let after_rejection = platform.registry().await?;
        ensure(
            registry_metadata(&objects).await? == registry_object
                && after_rejection.value == registry.value,
            "the rejected deployment changed the registry",
        )?;
        let after = object_inventory(&objects).await?;
        ensure(
            after == before,
            format!(
                "the rejected deployment changed objects: added {:?}, removed {:?}",
                after.difference(&before).collect::<Vec<_>>(),
                before.difference(&after).collect::<Vec<_>>()
            ),
        )?;
        require_successor_absent(&objects, &next).await?;
        ensure(
            workload::writer_identity(&objects, CONTROL_NAMESPACE, &runtime, &proxy).await?
                == writers,
            "permission rejection changed the active initial writer",
        )?;
        ensure(
            !after_rejection
                .value
                .transitions
                .contains_key(&next.transition_id)
                && after_rejection.value.next_sequence == sequence
                && after_rejection
                    .value
                    .histories
                    .get(DOMAIN)
                    .and_then(|history| history.active_allocation.as_ref())
                    .is_some_and(|active| active.generation_id == initial.committed.generation),
            "the rejected deployment reserved a transition or moved the active generation",
        )?;
        scenario.controller_logs("initial").await?;
        scenario.recorder.check(
            "b-successor-rejected-before-successor-objects",
            &json!({
                "diagnostic": pair::rejection(&self.pair.missing),
                "registryUnchangedByRejection": registry_object,
                "unallocatedServiceAccount": next.service_account_name,
                "initialWriters": writers,
                "objectsCompared": before.len(),
            }),
        );
        Ok(())
    }

    /// (c)
    async fn successor_commits(
        &self,
        scenario: &mut Scenario<'_>,
        platform: &Platform,
        initial: &Initial,
        policy_change: Value,
    ) -> Result<Committed> {
        let successor = &self.pair.successor;
        let run = scenario
            .deploy(platform, &successor.root, "deploy-successor")
            .await?;
        let committed = aps::committed(
            &run,
            &successor.release_digest,
            &successor.manifest.ir_digest,
        )?;
        ensure(
            committed.transition != initial.committed.transition
                && committed.generation != initial.committed.generation,
            "CLI upgrade reused the initial allocation",
        )?;
        let registry = platform.registry().await?;
        ensure(
            registry
                .value
                .histories
                .get(DOMAIN)
                .and_then(|history| history.active_allocation.as_ref())
                .is_some_and(|active| active.generation_id == committed.generation),
            "the successor generation is not the active allocation",
        )?;
        scenario.recorder.check(
            "c-successor-commits-after-ceiling-widened",
            &json!({"policyChange": policy_change, "report": committed_report(&committed)}),
        );
        Ok(committed)
    }

    /// (f)
    async fn inputs_roll_out(&self, scenario: &mut Scenario<'_>) -> Result {
        let objects = scenario.objects.clone();
        let workload = Workload {
            objects: &objects,
            namespace: WORKLOAD_NAMESPACE,
            node: &scenario.node,
            image: &self.pair.api_image,
        };
        objects
            .require("v1", "Service", Some(WORKLOAD_NAMESPACE), "database-api")
            .await?;
        let before = workload.identity(None, Duration::from_secs(300)).await?;
        let first = workload.messages(2, &before).await?;
        let (after, second) = workload.configmap_rollout(&before).await?;
        let equal = workload.equal_data_no_rollout(&after).await?;
        let probe = {
            use base64::Engine as _;
            let bytes = acceptance::random_bytes(24)?;
            scenario.recorder.canary(&bytes);
            base64::engine::general_purpose::STANDARD.encode(bytes)
        };
        scenario.recorder.canary(probe.as_bytes());
        let secret = workload.secret_reaction(&after, &probe).await?;
        scenario.recorder.check(
            "f-input-reactions",
            &json!({
                "postgrestMessagesLive": first,
                "configmapDrivenPodTurnover": {"before": before, "after": after},
                "postgrestMessagesAfterRollout": second,
                "equalDataConfigmapNoRollout": equal,
                "secretKeyReactionAndRestoration": secret,
            }),
        );
        Ok(())
    }
}

/// The `ConfigMap` the successor observes, which the platform team owns.
pub async fn create_api_settings(objects: &Objects) -> Result {
    objects
        .create(&json!({
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": {"name": "api-settings", "namespace": WORKLOAD_NAMESPACE},
            "data": {"PGRST_DB_MAX_ROWS": "100"},
        }))
        .await?;
    Ok(())
}

/// No object of the successor's API or of the next generation exists.
async fn require_successor_absent(objects: &Objects, next: &deploy::AllocationNames) -> Result {
    for (kind, api_version) in [("Deployment", "apps/v1"), ("Service", "v1")] {
        ensure(
            objects
                .get(api_version, kind, Some(WORKLOAD_NAMESPACE), "database-api")
                .await?
                .is_none(),
            "unapproved successor object exists",
        )?;
    }
    let names = object_inventory(objects).await?;
    ensure(
        !names.iter().any(|entry| {
            entry.contains(&next.service_account_name) || entry.contains(&next.generation_id)
        }),
        "an object of the unapproved generation exists",
    )
}

/// (d)
async fn old_identity_retired(
    scenario: &mut Scenario<'_>,
    platform: &Platform,
    initial: &Initial,
    successor: &Committed,
) -> Result {
    let objects = &scenario.objects;
    let retired = initial
        .retirement
        .iter()
        .filter(|(key, _)| key.contains("/RoleBinding/") || key.starts_with("v1/ServiceAccount/"))
        .collect::<Vec<_>>();
    let account_key = format!(
        "v1/ServiceAccount/{CONTROL_NAMESPACE}/{}",
        initial.runtime_account
    );
    let old_account_uid = retired
        .iter()
        .find(|(key, _)| *key == account_key)
        .map(|(_, uid)| uid.clone())
        .ok_or_else(|| {
            Error::Check("the old ServiceAccount is not recorded for retirement".into())
        })?;
    ensure(
        retired.iter().any(|(key, _)| key.contains("/RoleBinding/")),
        "no old-generation RoleBinding is recorded for retirement",
    )?;
    for (key, _) in &retired {
        let (api_version, kind, namespace, name) = identity_key(key)?;
        ensure(
            objects
                .get(&api_version, &kind, Some(&namespace), &name)
                .await?
                .is_none(),
            format!("old-generation {kind} {namespace}/{name} was not retired"),
        )?;
    }
    let registry = platform.registry().await?;
    let checkpoint = writer_checkpoint(&registry)?;
    ensure(
        checkpoint.allocation.generation_id == successor.generation,
        "the active writer is not the successor generation",
    )?;
    let account = checkpoint.runtime_binding.service_account_name.clone();
    let current = objects
        .require("v1", "ServiceAccount", Some(CONTROL_NAMESPACE), &account)
        .await?;
    ensure(
        account != initial.runtime_account && text(&current, "/metadata/uid") != old_account_uid,
        "the successor ServiceAccount is not distinct from the old one",
    )?;
    let old_subjects = [
        initial.runtime_account.clone(),
        format!("{}-process", initial.runtime_account),
    ];
    for namespace in [CONTROL_NAMESPACE, WORKLOAD_NAMESPACE] {
        for binding in objects
            .list(
                "rbac.authorization.k8s.io/v1",
                "RoleBinding",
                Some(namespace),
                None,
            )
            .await?
        {
            ensure(
                !binding["subjects"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|subject| {
                        old_subjects
                            .iter()
                            .any(|old| subject["name"] == old.as_str())
                    }),
                "a RoleBinding still grants the old generation's identity",
            )?;
        }
    }
    scenario.recorder.check(
        "d-old-generation-identity-retired",
        &json!({
            "retired": retired.iter().map(|(key, _)| key).collect::<Vec<_>>(),
            "oldServiceAccount": initial.runtime_account,
            "newServiceAccount": account,
        }),
    );
    Ok(())
}
