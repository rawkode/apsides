//! Each generated scenario, driven against the running fixture controller.
//!
//! The fixture driver (cluster admin) seeds, mutates and deletes inputs, edits
//! the controller's Role to deny access, and cuts its network path. Negative
//! authorization checks are made with the controller's own token. Every wait
//! is a condition with a deadline; the only fixed windows are the short quiet
//! periods that establish a revision did not change.

use crate::{
    Error, Result,
    contract::{FixtureRelease, MUTATION_SCENARIO},
    driver::{Driver, POLL, wait_for},
    manifests::{
        self, CONTROLLER, FOREIGN_MANAGER, OUTSIDE, encode, grants_read, plural, without_create,
        without_input,
    },
    offline::{self, PodProbe},
    oracle::{self, Mutation},
    report::{Assertion, CaseRow, CaseStatus, Evidence, NetworkProbe, Reach},
};
use base64::Engine as _;
use compiler_core::{
    CaseResult, CaseStatus as SemanticStatus, Input, InputMode, REVISION_ANNOTATION, Reaction,
    Resource, TestCase,
};
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Deadline for a condition the controller or Kubernetes must reach.
const WAIT: Duration = Duration::from_secs(90);
/// How long a revision must hold still to count as unchanged.
const QUIET: Duration = Duration::from_secs(3);

/// Scenarios whose evidence is the semantic runner's typed primitive result;
/// the cluster cannot inject these observations without changing the
/// controller.
const SEMANTIC_ONLY: [&str; 3] = ["unauthorized-input", "unavailable-input", "invalid-input"];

/// Scenarios that need an observed input to act on.
const NEEDS_OBSERVED: [&str; 6] = [
    "projected-change",
    "coalesced-inputs",
    "missing-input",
    "input-returned",
    "restart-and-relist",
    "credential-read-denial",
];

/// Scenarios that need a managed input to act on.
const NEEDS_MANAGED: [&str; 4] = [
    "managed-drift-repair",
    "managed-missing",
    "managed-read-denial",
    "managed-unavailable",
];

/// Reasons the controller reports for a write another field manager blocks.
const CONFLICT_REASONS: [&str; 2] = ["field-or-version-conflict", "ownership-conflict"];

/// The field manager a domain's handler writes attribute to (see
/// `suite.rs`'s `domain_manager` for the reconciler's own, distinct
/// `apsides-<domainId>`).
fn handler_field_manager(domain_id: &str) -> String {
    format!("apsides-handler-{domain_id}")
}

/// The SSA payload a handler write applies to its output: identity plus
/// exactly the `data` it is writing, never the metadata the reconciler's own
/// apply of the handler output owns.
fn handler_output_payload(input: &Input, data: &Map<String, Value>) -> Value {
    json!({"apiVersion": "v1", "kind": input.kind,
        "metadata": {"name": input.name, "namespace": input.namespace},
        "data": data})
}

/// What the cases record as they run. The suite holds it too, so an
/// interrupted run still scans and writes what was recorded.
#[derive(Debug, Default)]
pub struct Record {
    /// Controller logs from Pods that have since gone.
    pub logs: Mutex<Vec<String>>,
    pub egress_logs: Mutex<Vec<String>>,
    /// Every synthetic secret value written, for the canary scan.
    pub canaries: Mutex<BTreeSet<String>>,
}

/// The running suite: the release, the driver's and the controller's
/// clients, and what has been observed so far.
pub struct Live<'a> {
    pub release: &'a FixtureRelease,
    pub driver: Driver,
    /// Authenticates as the controller's `ServiceAccount`, from the host.
    pub runtime: Client,
    pub workload: String,
    pub control: String,
    /// The controller's server-side apply field manager.
    pub domain_manager: String,
    /// The controller's Role in the workload namespace, as generated.
    pub original_rules: Vec<Value>,
    pub api_cidr: String,
    pub semantic: BTreeMap<(String, String), CaseResult>,
    /// What the cases have recorded, shared with the suite.
    pub record: Arc<Record>,
}

impl Live<'_> {
    // --- the controller Pod -------------------------------------------------

    fn selector() -> String {
        format!("app={CONTROLLER}")
    }

    /// The running controller Pod, if there is one. A terminated container
    /// in it is an error.
    pub async fn controller_pod(&self) -> Result<Option<String>> {
        let pods = self.driver.pods(&self.control, &Self::selector()).await?;
        let running = pods.iter().find(|pod| phase(pod) == "Running");
        let Some(pod) = running else {
            return Ok(None);
        };
        let terminated = pod
            .status
            .as_ref()
            .and_then(|status| status.container_statuses.as_ref())
            .is_some_and(|statuses| {
                statuses.iter().any(|status| {
                    status
                        .state
                        .as_ref()
                        .is_some_and(|state| state.terminated.is_some())
                })
            });
        if terminated {
            return Err(Error::Case("controller or egress proxy terminated".into()));
        }
        Ok(pod.metadata.name.clone())
    }

    async fn newest_pod(&self) -> Option<String> {
        let mut pods = self
            .driver
            .pods(&self.control, &Self::selector())
            .await
            .ok()?;
        pods.sort_by_key(|pod| pod.metadata.creation_timestamp.clone());
        pods.last().and_then(|pod| pod.metadata.name.clone())
    }

    /// A container's log in the newest controller Pod, preceded by its
    /// previous instance's log when the container restarted.
    pub async fn container_logs(&self, container: &str) -> String {
        let Some(pod) = self.newest_pod().await else {
            return String::new();
        };
        let previous = self.driver.logs(&self.control, &pod, container, true).await;
        let current = self
            .driver
            .logs(&self.control, &pod, container, false)
            .await;
        [previous, current]
            .into_iter()
            .filter(|log| !log.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The loopback proxy's probes: before forwarding, it must have failed to
    /// reach both the sentinel and an external address.
    pub async fn assert_proxy_canaries(&self) -> Result<Vec<NetworkProbe>> {
        let probes = [
            PodProbe::tcp("controller-sentinel", "sentinel", 8080, Reach::Blocked),
            PodProbe::tcp(
                "controller-external-tcp",
                offline::EXTERNAL_ADDRESS,
                443,
                Reach::Blocked,
            ),
        ];
        let origin = format!("controller Pod in {}", self.control);
        let results = wait_for("controller egress probes", WAIT, async || {
            let results = offline::pod_results(
                &self.container_logs("loopback-proxy").await,
                &origin,
                &probes,
            );
            Ok(results
                .iter()
                .all(|probe| probe.observed != Reach::NotRun)
                .then_some(results))
        })
        .await?;
        if results.iter().any(|probe| !probe.passed()) {
            return Err(Error::Infrastructure(
                "the controller's proxy reached the network before isolation".into(),
            ));
        }
        Ok(results)
    }

    pub async fn controller_logs(&self) -> String {
        let current = self.container_logs("controller").await;
        let mut logs = self
            .record
            .logs
            .lock()
            .map(|logs| logs.clone())
            .unwrap_or_default();
        logs.push(current);
        logs.join("\n")
    }

    /// Keep the current Pod's logs before it is replaced.
    pub async fn preserve_logs(&self) {
        let controller = self.container_logs("controller").await;
        let egress = format!(
            "{}\n{}",
            self.container_logs("egress-preflight").await,
            self.container_logs("loopback-proxy").await
        );
        if let Ok(mut logs) = self.record.logs.lock()
            && !controller.is_empty()
        {
            logs.push(controller);
        }
        if let Ok(mut logs) = self.record.egress_logs.lock()
            && !egress.trim().is_empty()
        {
            logs.push(egress);
        }
    }

    pub async fn scale_controller(&self, replicas: u32) -> Result<()> {
        if replicas == 0 {
            self.preserve_logs().await;
        }
        self.driver
            .scale(&self.control, CONTROLLER, replicas)
            .await?;
        wait_for("controller scale", WAIT, async || {
            let running = self.controller_pod().await?.is_some();
            Ok((running == (replicas > 0)).then_some(()))
        })
        .await?;
        if replicas > 0 {
            self.assert_proxy_canaries().await?;
        }
        Ok(())
    }

    async fn set_api_egress(&self, enabled: bool) -> Result<()> {
        self.driver
            .put(&manifests::controller_api_egress(
                &self.control,
                &self.api_cidr,
                enabled,
            ))
            .await?;
        Ok(())
    }

    /// A position in the controller's log; later reasons are read after it.
    async fn log_mark(&self) -> usize {
        self.controller_logs().await.lines().count()
    }

    /// The most recent reason the controller logged, after `mark`, for
    /// blocking `target`.
    async fn blocked_reason_since(&self, mark: usize, target: &str) -> Option<String> {
        let logs = self.controller_logs().await;
        let lines = logs.lines().collect::<Vec<_>>();
        blocked_reason(lines.get(mark..).unwrap_or_default(), target)
    }

    /// Wait for the controller to report `target` blocked after `mark`.
    async fn wait_blocked(&self, mark: usize, target: &str, label: &str) -> Result<String> {
        wait_for(label, WAIT, async || {
            Ok(self.blocked_reason_since(mark, target).await)
        })
        .await
    }

    // --- the reaction target ------------------------------------------------

    fn target(&self, reaction: &Reaction) -> Result<&Resource> {
        self.release
            .plan
            .ir
            .resources
            .iter()
            .find(|resource| resource.id == reaction.target)
            .ok_or_else(|| Error::Case(format!("target {} is not declared", reaction.target)))
    }

    async fn target_object(&self, reaction: &Reaction) -> Result<Value> {
        let target = self.target(reaction)?;
        self.driver
            .require(
                "apps/v1",
                "Deployment",
                Some(&target.namespace),
                &target.name,
            )
            .await
    }

    async fn revision(&self, reaction: &Reaction) -> Result<String> {
        let object = self.target_object(reaction).await?;
        Ok(
            object["spec"]["template"]["metadata"]["annotations"][REVISION_ANNOTATION]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        )
    }

    async fn generation(&self, reaction: &Reaction) -> Result<i64> {
        Ok(
            self.target_object(reaction).await?["metadata"]["generation"]
                .as_i64()
                .unwrap_or_default(),
        )
    }

    /// The target's current template is fully rolled out and ready.
    async fn ready(&self, reaction: &Reaction) -> Result<bool> {
        let object = self.target_object(reaction).await?;
        let count = object["spec"]["replicas"].as_i64().unwrap_or(1);
        let status = &object["status"];
        let number = |field: &str| status[field].as_i64().unwrap_or_default();
        Ok(count > 0
            && number("observedGeneration")
                >= object["metadata"]["generation"]
                    .as_i64()
                    .unwrap_or(i64::MAX)
            && number("updatedReplicas") == count
            && number("readyReplicas") == count)
    }

    async fn ready_pods(&self, reaction: &Reaction) -> Result<i64> {
        Ok(
            self.target_object(reaction).await?["status"]["readyReplicas"]
                .as_i64()
                .unwrap_or_default(),
        )
    }

    /// Whether `manager` owns the target's revision annotation.
    async fn owns_revision(&self, reaction: &Reaction, manager: &str) -> Result<bool> {
        Ok(owns_revision(&self.target_object(reaction).await?, manager))
    }

    async fn wait_ready(&self, reaction: &Reaction, label: &str) -> Result<()> {
        wait_for(label, WAIT, async || {
            Ok(self.ready(reaction).await?.then_some(()))
        })
        .await
    }

    /// Whether the target finishes rolling out before the case deadline. The
    /// revision can match while new Pods are still starting, so an immediate
    /// readiness check fails on a slower cluster.
    async fn converged(&self, reaction: &Reaction) -> Result<bool> {
        match self.wait_ready(reaction, "target convergence").await {
            Ok(()) => Ok(true),
            Err(Error::Deadline(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    // --- inputs -------------------------------------------------------------

    fn managed_resource(&self, input: &Input) -> Result<&Resource> {
        self.release
            .plan
            .ir
            .resources
            .iter()
            .find(|resource| input.managed_resource_id.as_ref() == Some(&resource.id))
            .ok_or_else(|| Error::Case(format!("{} has no managed resource", input.id)))
    }

    async fn input_object(&self, input: &Input) -> Result<Option<Value>> {
        self.driver
            .get("v1", &input.kind, Some(&input.namespace), &input.name)
            .await
    }

    async fn input_data(&self, input: &Input) -> Result<Map<String, Value>> {
        let object = self
            .input_object(input)
            .await?
            .ok_or_else(|| Error::Case(format!("input {} does not exist", input.id)))?;
        Ok(object["data"].as_object().cloned().unwrap_or_default())
    }

    async fn input_exists(&self, input: &Input) -> Result<bool> {
        Ok(self.input_object(input).await?.is_some())
    }

    fn remember(&self, kind: &str, value: &str) {
        if kind == "Secret"
            && let Ok(mut canaries) = self.record.canaries.lock()
        {
            canaries.insert(value.to_owned());
            canaries.insert(encode(kind, value));
        }
    }

    /// Set one data key as a writer other than the driver's apply identity.
    async fn mutate_input(&self, input: &Input, value: &str, key: &str) -> Result<()> {
        self.remember(&input.kind, value);
        self.driver
            .merge(
                "v1",
                &input.kind,
                Some(&input.namespace),
                &input.name,
                &json!({"data": {key: encode(&input.kind, value)}}),
            )
            .await?;
        Ok(())
    }

    /// Simulate a handler's own write to its output: an SSA apply of exactly
    /// the changed `data` key, under the handler field manager
    /// (`apsides-handler-<domainId>`), never forced. This increment has no
    /// handler execution runtime yet (`IR-CONTRACT.md`'s "Runtime execution
    /// of a handler's bundle … is a later phase"), so the fixture stands in
    /// for it to exercise a `rolloutOn` reaction on an output end to end; the
    /// reconciler's own manager (`apsides-<domainId>`) never claims `data` on
    /// a handler output (see `preview_execution.rs::require_apply_owner`),
    /// so this distinct manager owns it without conflict.
    async fn write_handler_output(&self, input: &Input, value: &str, key: &str) -> Result<()> {
        self.remember(&input.kind, value);
        let data = Map::from_iter([(key.to_owned(), json!(encode(&input.kind, value)))]);
        self.driver
            .apply(
                &handler_output_payload(input, &data),
                &handler_field_manager(&self.release.plan.ir.domain.id),
                false,
            )
            .await?;
        Ok(())
    }

    /// Change one data key of `input`'s live object, as whichever writer a
    /// change to that input's mode models: a foreign patch for an
    /// `Observed` input, or a simulated handler SSA write for an `Output`
    /// input (see [`Self::write_handler_output`]). `Output`-mode inputs are
    /// the only ones ever passed here for a scenario that changes data (see
    /// `test_plan.rs`'s `OUTPUT` scenario set), so `Managed` never reaches
    /// this call.
    async fn change_input(&self, input: &Input, value: &str, key: &str) -> Result<()> {
        match input.mode {
            InputMode::Output => self.write_handler_output(input, value, key).await,
            InputMode::Observed | InputMode::Managed => self.mutate_input(input, value, key).await,
        }
    }

    /// Write `data`, already in stored form, as the whole input.
    async fn apply_input_data(&self, input: &Input, data: &Map<String, Value>) -> Result<()> {
        self.driver
            .put(&json!({"apiVersion": "v1", "kind": input.kind,
                "metadata": {"name": input.name, "namespace": input.namespace}, "data": data}))
            .await?;
        Ok(())
    }

    /// Recreate the input with `data` if it is gone.
    async fn restore_input_data(&self, input: &Input, data: &Map<String, Value>) -> Result<()> {
        if !self.input_exists(input).await? {
            self.apply_input_data(input, data).await?;
        }
        wait_for(&format!("input restored: {}", input.id), WAIT, async || {
            Ok(self.input_exists(input).await?.then_some(()))
        })
        .await
    }

    async fn delete_input(&self, input: &Input) -> Result<()> {
        self.driver
            .delete("v1", &input.kind, Some(&input.namespace), &input.name)
            .await
    }

    // --- expectations -------------------------------------------------------

    /// The oracle's revision for the target, from the live observed inputs.
    async fn expected_revision(
        &self,
        reaction: &Reaction,
        mutation: Mutation<'_>,
    ) -> Result<String> {
        let plan = &self.release.plan;
        let mut observed = BTreeMap::new();
        for input in
            plan.ir.inputs.iter().filter(|input| {
                matches!(input.mode, InputMode::Observed | InputMode::Output)
                    && plan.ir.reactions.iter().any(|item| {
                        item.target == reaction.target && item.inputs.contains(&input.id)
                    })
            })
        {
            observed.insert(
                input.id.clone(),
                Value::Object(self.input_data(input).await?),
            );
        }
        oracle::expected_revision(plan, &reaction.target, mutation, &observed)
    }

    async fn wait_expected(&self, reaction: &Reaction, label: &str) -> Result<String> {
        let expected = self.expected_revision(reaction, Mutation::None).await?;
        wait_for(label, WAIT, async || {
            Ok((self.revision(reaction).await? == expected).then_some(()))
        })
        .await?;
        Ok(expected)
    }

    /// Whether the target's revision holds at `expected` for the quiet window.
    async fn holds(&self, reaction: &Reaction, expected: &str) -> Result<bool> {
        let deadline = Instant::now() + QUIET;
        while Instant::now() < deadline {
            if self.revision(reaction).await? != expected {
                return Ok(false);
            }
            tokio::time::sleep(POLL).await;
        }
        Ok(true)
    }

    /// The target's revision holds at `expected` for the quiet window.
    async fn stable(&self, reaction: &Reaction, expected: &str) -> Result<()> {
        if self.holds(reaction, expected).await? {
            return Ok(());
        }
        Err(Error::Case("unexpected target revision change".into()))
    }

    // --- authorization ------------------------------------------------------

    async fn set_rules(&self, rules: &[Value]) -> Result<()> {
        self.driver
            .put(&manifests::role(&self.workload, rules))
            .await?;
        Ok(())
    }

    async fn restore_rules(&self) -> Result<()> {
        self.set_rules(&self.original_rules).await
    }

    /// A raw API request with the controller's own credential; the HTTP
    /// status is the evidence.
    async fn runtime_api(&self, method: &str, path: &str, body: Option<&Value>) -> Result<u16> {
        let bytes = body
            .map(serde_json::to_vec)
            .transpose()?
            .unwrap_or_default();
        let request = http::Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(kube::client::Body::from(bytes))
            .map_err(|error| Error::Case(error.to_string()))?;
        Ok(self.runtime.send(request).await?.status().as_u16())
    }

    // --- cases --------------------------------------------------------------

    /// Wait until every reaction target carries the oracle's revision and is
    /// fully rolled out, so every case starts from a converged baseline.
    pub async fn establish_baseline(&self) -> Result<()> {
        let mut seen = BTreeSet::new();
        for reaction in &self.release.plan.ir.reactions {
            if seen.insert(&reaction.target) {
                self.wait_expected(reaction, "baseline revision").await?;
                self.wait_ready(reaction, "baseline readiness").await?;
            }
        }
        Ok(())
    }

    fn semantic_row(&self, case: &TestCase) -> Result<&CaseResult> {
        self.semantic
            .get(&(case.reaction_id.clone(), case.scenario.clone()))
            .ok_or_else(|| Error::Case("the semantic runner produced no row".into()))
    }

    fn semantic_passed(&self, case: &TestCase) -> Result<()> {
        let row = self.semantic_row(case)?;
        if row.status != SemanticStatus::Passed {
            return Err(Error::Case(row.detail.clone()));
        }
        Ok(())
    }

    /// Run one generated case and record its result.
    pub async fn run_case(&self, case: &TestCase) -> CaseRow {
        let started = Instant::now();
        let semantic_only = SEMANTIC_ONLY.contains(&case.scenario.as_str());
        let evidence = if semantic_only {
            vec![Evidence::Semantic]
        } else if case.layer == "semantic" {
            vec![Evidence::Semantic, Evidence::Cluster]
        } else {
            vec![Evidence::Cluster]
        };
        let mut row = self.row(case, evidence);
        if !case.required {
            row.status = CaseStatus::Excluded;
            row.detail = "not required by the test plan; not run".into();
            return row;
        }
        let outcome = match self.reaction(&case.reaction_id) {
            Ok(reaction) => {
                if case.layer == "semantic" && !semantic_only {
                    match self.semantic_passed(case) {
                        Ok(()) => self.execute(case, reaction).await,
                        Err(error) => Err(error),
                    }
                } else {
                    self.execute(case, reaction).await
                }
            }
            Err(error) => Err(error),
        };
        row.seconds = started.elapsed().as_secs_f64();
        match outcome {
            Ok(Some(assertions)) => {
                let failed = assertions
                    .iter()
                    .filter(|item| !item.passed)
                    .map(|item| item.check_id.clone())
                    .collect::<Vec<_>>();
                if assertions.is_empty() || !failed.is_empty() {
                    row.status = CaseStatus::Failed;
                    row.detail = format!("assertions failed: {}", failed.join(", "));
                } else {
                    row.status = CaseStatus::Passed;
                    row.detail = case.assertion.clone();
                }
                row.assertions = assertions;
            }
            Ok(None) => {
                row.status = CaseStatus::Unsupported;
                row.detail = format!(
                    "the cluster runner has no implementation for required scenario {}",
                    case.scenario
                );
            }
            Err(error) => {
                row.status = CaseStatus::Failed;
                row.detail = error.to_string();
            }
        }
        row
    }

    /// Check that the converged live revision differs from the oracle's
    /// revision under each mutation of this reaction: its contribution
    /// removed, and each input only it selects ignored. The mutations are
    /// applied to the oracle's model, not to the controller.
    pub async fn mutation_case(&self, case: &TestCase) -> CaseRow {
        let started = Instant::now();
        let mut row = self.row(case, vec![Evidence::Cluster]);
        let outcome = async {
            let reaction = self.reaction(&case.reaction_id)?;
            let live = self.revision(reaction).await?;
            let expected = self.expected_revision(reaction, Mutation::None).await?;
            let omitted = self
                .expected_revision(reaction, Mutation::OmitReaction(&reaction.id))
                .await?;
            let mut assertions = vec![
                Assertion::new("baseline-revision", expected, live.clone()),
                Assertion::new("live-differs-from-omitted-reaction", true, omitted != live),
            ];
            let inputs = oracle::exclusive_inputs(&self.release.plan, &reaction.id);
            for input in &inputs {
                let ignored = self
                    .expected_revision(
                        reaction,
                        Mutation::IgnoreInput {
                            reaction: &reaction.id,
                            input,
                        },
                    )
                    .await?;
                assertions.push(Assertion::new(
                    &format!("live-differs-from-ignored-input:{input}"),
                    true,
                    ignored != live,
                ));
            }
            Ok::<_, Error>((assertions, inputs.len()))
        }
        .await;
        row.seconds = started.elapsed().as_secs_f64();
        match outcome {
            Ok((assertions, inputs)) => {
                let failed = assertions
                    .iter()
                    .filter(|item| !item.passed)
                    .map(|item| item.check_id.clone())
                    .collect::<Vec<_>>();
                row.status = if failed.is_empty() {
                    CaseStatus::Passed
                } else {
                    CaseStatus::Failed
                };
                row.detail = if failed.is_empty() {
                    format!(
                        "the live revision differs from the oracle model with the reaction removed and with each of its {inputs} exclusive input(s) ignored; mutations are applied to the oracle model"
                    )
                } else {
                    format!("mutation not distinguished: {}", failed.join(", "))
                };
                row.assertions = assertions;
            }
            Err(error) => {
                row.status = CaseStatus::Failed;
                row.detail = error.to_string();
            }
        }
        row
    }

    fn row(&self, case: &TestCase, evidence: Vec<Evidence>) -> CaseRow {
        let case_digest = serde_json::to_value(case)
            .ok()
            .and_then(|value| oracle::canonical(&value).ok())
            .map(|bytes| {
                use sha2::Digest as _;
                format!("sha256:{:x}", sha2::Sha256::digest(bytes))
            })
            .unwrap_or_default();
        CaseRow {
            reaction_id: case.reaction_id.clone(),
            scenario: case.scenario.clone(),
            layer: case.layer.clone(),
            required: case.required,
            source: case.source.clone(),
            input_ids: case.input_ids.clone(),
            assertion: case.assertion.clone(),
            status: CaseStatus::Failed,
            evidence,
            fixture_digest: self.release.fixture_digest.clone(),
            release_digest: self.release.release_digest.clone(),
            overlay_digest: self.release.overlay_digest.clone(),
            case_digest,
            assertions: Vec::new(),
            detail: String::new(),
            seconds: 0.0,
        }
    }

    fn reaction(&self, id: &str) -> Result<&Reaction> {
        self.release
            .plan
            .ir
            .reactions
            .iter()
            .find(|reaction| reaction.id == id)
            .ok_or_else(|| Error::Case(format!("reaction {id} is not declared")))
    }

    /// `Ok(None)` means the runner has no implementation for the scenario.
    async fn execute(
        &self,
        case: &TestCase,
        reaction: &Reaction,
    ) -> Result<Option<Vec<Assertion>>> {
        let stimulus = self
            .release
            .plan
            .ir
            .inputs
            .iter()
            .filter(|input| {
                reaction.inputs.contains(&input.id) && case.input_ids.contains(&input.id)
            })
            .collect::<Vec<_>>();
        // An `Output`-mode input is observed live exactly like an `Observed`
        // one (see `InputMode::Output`); `test_plan.rs`'s `LIVE_SHARED`
        // scenario set gives a reaction spanning both modes one merged case
        // whose `input_ids` cover every live input together, so `observed`
        // here (and `NEEDS_OBSERVED` below) is shared between the two modes
        // and may legitimately hold a mix of both for those scenarios.
        let observed = stimulus
            .iter()
            .copied()
            .filter(|input| matches!(input.mode, InputMode::Observed | InputMode::Output))
            .collect::<Vec<_>>();
        let managed = stimulus
            .iter()
            .copied()
            .filter(|input| input.mode == InputMode::Managed)
            .collect::<Vec<_>>();
        let scenario = case.scenario.as_str();
        if SEMANTIC_ONLY.contains(&scenario) {
            return self.semantic_only(case).map(Some);
        }
        if NEEDS_OBSERVED.contains(&scenario) && observed.is_empty() {
            return Err(Error::Case(format!(
                "reaction {} has no observed input for {scenario}",
                reaction.id
            )));
        }
        if NEEDS_MANAGED.contains(&scenario) && managed.is_empty() {
            return Err(Error::Case(format!(
                "reaction {} has no managed input for {scenario}",
                reaction.id
            )));
        }
        let selected = observed
            .first()
            .or_else(|| stimulus.first())
            .copied()
            .ok_or_else(|| Error::Case("the case names no stimulus input".into()));
        let assertions = match scenario {
            "initial-evaluation" => self.initial(reaction).await?,
            "projected-change" => self.projected(reaction, &observed).await?,
            "irrelevant-change" => self.irrelevant(reaction, selected?).await?,
            "same-value-replay" => self.replay(reaction, selected?).await?,
            "coalesced-inputs" => self.coalesced(reaction, &observed).await?,
            "missing-input" => self.missing(reaction, selected?).await?,
            "input-returned" => self.returned(reaction, selected?).await?,
            "restart-and-relist" => self.relist(reaction, &observed).await?,
            "accepted-write-restart" => self.accepted_restart(reaction).await?,
            "credential-read-denial" => self.denied_read(reaction, selected?).await?,
            "forced-field-conflict" => self.forced_conflict(reaction).await?,
            "field-conflict" => self.conflict(reaction).await?,
            "out-of-scope-change" => self.out_of_scope().await?,
            "rollout-readiness" => self.readiness(reaction).await?,
            "managed-drift-repair" => self.managed_drift(reaction, &managed).await?,
            "managed-missing" => self.managed_missing(reaction, &managed).await?,
            "managed-read-denial" => self.managed_read_denial(reaction, &managed).await?,
            "managed-unavailable" => self.managed_unavailable(reaction).await?,
            _ => return Ok(None),
        };
        Ok(Some(assertions))
    }

    /// The semantic runner's typed primitive result is the evidence: it
    /// checked the block reason for every input of the case.
    fn semantic_only(&self, case: &TestCase) -> Result<Vec<Assertion>> {
        let row = self.semantic_row(case)?;
        Ok(vec![Assertion::new(
            "semantic-status",
            "passed",
            serde_json::to_value(&row.status)?,
        )])
    }

    async fn initial(&self, reaction: &Reaction) -> Result<Vec<Assertion>> {
        let expected = self
            .wait_expected(reaction, "initial exact revision")
            .await?;
        self.wait_ready(reaction, "initial ready Pods").await?;
        let revision = self.revision(reaction).await?;
        Ok(vec![
            Assertion::new("revision-present", true, !revision.is_empty()),
            Assertion::new(
                "target-converged",
                true,
                revision == expected && self.ready(reaction).await?,
            ),
        ])
    }

    /// Change every one of `inputs`' data (a mixed reaction's merged
    /// `projected-change` case lists both an Observed and an Output-mode
    /// input, see `test_plan.rs`'s `LIVE_SHARED`), then check the revision
    /// changed and converged. Each gets its own value, index-suffixed, so a
    /// multi-input case does not coincidentally leave one input's old value
    /// still equal to its new one.
    async fn projected(&self, reaction: &Reaction, inputs: &[&Input]) -> Result<Vec<Assertion>> {
        let before = self.revision(reaction).await?;
        for (index, input) in inputs.iter().enumerate() {
            self.change_input(
                input,
                &format!("projected-{}-{index}", reaction.id),
                "message",
            )
            .await?;
        }
        let expected = self.wait_expected(reaction, "projected revision").await?;
        let actual = self.revision(reaction).await?;
        Ok(vec![
            Assertion::new("revision-changed", true, actual != before),
            Assertion::new("target-converged", true, actual == expected),
        ])
    }

    /// The revision held and the template was not rewritten; `mutations`
    /// names the second check.
    async fn unchanged(
        &self,
        reaction: &Reaction,
        before: &str,
        generation: i64,
        mutations: &str,
    ) -> Result<Vec<Assertion>> {
        Ok(vec![
            Assertion::new(
                "revision-unchanged",
                true,
                self.revision(reaction).await? == before,
            ),
            Assertion::new(mutations, 0, self.generation(reaction).await? - generation),
        ])
    }

    async fn irrelevant(&self, reaction: &Reaction, input: &Input) -> Result<Vec<Assertion>> {
        let before = self.revision(reaction).await?;
        let generation = self.generation(reaction).await?;
        let noise = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        self.driver
            .merge(
                "v1",
                &input.kind,
                Some(&input.namespace),
                &input.name,
                &json!({"metadata": {"annotations": {"fixture-noise": noise.to_string()}}}),
            )
            .await?;
        self.stable(reaction, &before).await?;
        self.unchanged(reaction, &before, generation, "template-mutations")
            .await
    }

    async fn replay(&self, reaction: &Reaction, input: &Input) -> Result<Vec<Assertion>> {
        // An output-only reaction's handler output starts with no `data` at
        // all (the reconciler creates it absent-only, metadata-only) until
        // some other scenario has written it; make same-value-replay
        // self-contained by seeding it here rather than relying on
        // `projected-change` having already run first in whatever order the
        // suite happens to execute cases.
        if input.mode == InputMode::Output && self.input_data(input).await?.is_empty() {
            self.write_handler_output(input, &format!("seed-{}", reaction.id), "message")
                .await?;
            self.wait_expected(reaction, "same-value-replay seed")
                .await?;
        }
        let before = self.revision(reaction).await?;
        let generation = self.generation(reaction).await?;
        let data = self.input_data(input).await?;
        let Some((key, current)) = data.iter().next() else {
            return Err(Error::Case(format!(
                "input {} has no data to replay",
                input.id
            )));
        };
        let current = current.as_str().unwrap_or_default();
        let value = if input.kind == "Secret" {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(current)
                .map_err(|error| Error::Case(error.to_string()))?;
            String::from_utf8_lossy(&bytes).into_owned()
        } else {
            current.to_owned()
        };
        // The writer mode-appropriate to `input` (see `change_input`):
        // a foreign patch for an Observed input, or the handler-manager SSA
        // for an Output one -- replaying its own current value must use the
        // same writer identity a real change would, not always the foreign one.
        self.change_input(input, &value, key).await?;
        self.stable(reaction, &before).await?;
        self.unchanged(reaction, &before, generation, "template-mutations")
            .await
    }

    async fn coalesced(&self, reaction: &Reaction, observed: &[&Input]) -> Result<Vec<Assertion>> {
        for (index, input) in observed.iter().enumerate() {
            self.mutate_input(
                input,
                &format!("coalesced-{}-{index}", reaction.id),
                "message",
            )
            .await?;
        }
        let expected = self
            .wait_expected(reaction, "latest complete input set")
            .await?;
        self.stable(reaction, &expected).await?;
        Ok(vec![
            Assertion::new(
                "latest-input-set-used",
                true,
                self.revision(reaction).await? == expected,
            ),
            Assertion::new("target-converged", true, self.converged(reaction).await?),
        ])
    }

    async fn missing(&self, reaction: &Reaction, input: &Input) -> Result<Vec<Assertion>> {
        let before = self.revision(reaction).await?;
        let generation = self.generation(reaction).await?;
        let saved = self.input_data(input).await?;
        let mark = self.log_mark().await;
        let observed = async {
            self.delete_input(input).await?;
            let reason = self
                .wait_blocked(mark, &reaction.target, "missing input reported")
                .await?;
            self.stable(reaction, &before).await?;
            let preserved = self.ready(reaction).await? && self.revision(reaction).await? == before;
            let mutations = self.generation(reaction).await? - generation;
            Ok::<_, Error>((reason, preserved, mutations))
        }
        .await;
        let recovered = async {
            self.restore_input_data(input, &saved).await?;
            self.wait_expected(reaction, "missing-input cleanup recovery")
                .await?;
            self.wait_ready(reaction, "missing-input cleanup readiness")
                .await
        }
        .await;
        let (reason, preserved, mutations) = observed?;
        recovered?;
        Ok(vec![
            Assertion::new("block-reason", format!("{}:missing", input.id), reason),
            Assertion::new("target-mutations", 0, mutations),
            Assertion::new("target-preserved", true, preserved),
        ])
    }

    async fn returned(&self, reaction: &Reaction, input: &Input) -> Result<Vec<Assertion>> {
        let saved = self.input_data(input).await?;
        self.delete_input(input).await?;
        wait_for("returned-input missing phase", WAIT, async || {
            Ok((!self.input_exists(input).await?).then_some(()))
        })
        .await?;
        let value = format!("returned-{}", reaction.id);
        self.remember(&input.kind, &value);
        let mut changed = saved;
        changed.insert("message".into(), json!(encode(&input.kind, &value)));
        let checked = async {
            self.apply_input_data(input, &changed).await?;
            let expected = self
                .wait_expected(reaction, "returned input revision")
                .await?;
            self.wait_ready(reaction, "returned input target readiness")
                .await?;
            Ok::<_, Error>(vec![
                Assertion::new(
                    "dependency-recovered",
                    true,
                    self.revision(reaction).await? == expected,
                ),
                Assertion::new("target-converged", true, self.converged(reaction).await?),
            ])
        }
        .await;
        let restored = self.restore_input_data(input, &changed).await;
        let assertions = checked?;
        restored?;
        Ok(assertions)
    }

    /// Change every one of `inputs`' data while the controller is stopped
    /// (a mixed reaction's merged `restart-and-relist` case lists both an
    /// Observed and an Output-mode input, see `test_plan.rs`'s
    /// `LIVE_SHARED`), then restart and check the whole latest set was used.
    async fn relist(&self, reaction: &Reaction, inputs: &[&Input]) -> Result<Vec<Assertion>> {
        let mut saved = Vec::with_capacity(inputs.len());
        for input in inputs {
            saved.push(self.input_data(input).await?);
        }
        let stopped = async {
            self.scale_controller(0).await?;
            for (index, input) in inputs.iter().enumerate() {
                self.change_input(
                    input,
                    &format!("stopped-{}-{index}", reaction.id),
                    "message",
                )
                .await?;
            }
            Ok::<(), Error>(())
        }
        .await;
        let restored = async {
            for (input, data) in inputs.iter().zip(saved.iter()) {
                self.restore_input_data(input, data).await?;
            }
            Ok::<(), Error>(())
        }
        .await;
        let restarted = self.scale_controller(1).await;
        stopped?;
        restored?;
        restarted?;
        let expected = self
            .wait_expected(reaction, "restart relist revision")
            .await?;
        self.wait_ready(reaction, "restart relist target readiness")
            .await?;
        Ok(vec![
            Assertion::new(
                "latest-input-set-used",
                true,
                self.revision(reaction).await? == expected,
            ),
            Assertion::new("target-converged", true, self.converged(reaction).await?),
        ])
    }

    async fn accepted_restart(&self, reaction: &Reaction) -> Result<Vec<Assertion>> {
        self.scale_controller(1).await?;
        let before = self.revision(reaction).await?;
        let generation = self.generation(reaction).await?;
        let old = self
            .controller_pod()
            .await?
            .ok_or_else(|| Error::Case("controller Pod missing before restart".into()))?;
        self.preserve_logs().await;
        let restarted = async {
            self.driver
                .delete("v1", "Pod", Some(&self.control), &old)
                .await?;
            wait_for("new controller Pod", WAIT, async || {
                Ok(self
                    .controller_pod()
                    .await?
                    .filter(|pod| *pod != old)
                    .map(|_| ()))
            })
            .await?;
            self.stable(reaction, &before).await
        }
        .await;
        let scaled = self.scale_controller(1).await;
        restarted?;
        scaled?;
        self.unchanged(reaction, &before, generation, "template-mutations")
            .await
    }

    async fn denied_read(&self, reaction: &Reaction, input: &Input) -> Result<Vec<Assertion>> {
        let before = self.revision(reaction).await?;
        let generation = self.generation(reaction).await?;
        let denied = without_input(&self.original_rules, &input.kind, &input.name);
        self.set_rules(&denied).await?;
        let path = format!(
            "/api/v1/namespaces/{}/{}/{}",
            input.namespace,
            plural(&input.kind),
            input.name
        );
        let checked = async {
            // The API server's authorizer learns of RBAC changes through a
            // watch, so the read can still succeed briefly after the grant
            // is removed.
            let status = wait_for("denied input read to be refused", WAIT, async || {
                let status = self.runtime_api("GET", &path, None).await?;
                Ok((status == 403).then_some(status))
            })
            .await
            .map_err(|error| match error {
                Error::Deadline(_) => Error::Case(
                    "the controller's credential could still read the denied input after its grant was removed, expected 403".into(),
                ),
                other => other,
            })?;
            self.stable(reaction, &before).await?;
            Ok::<_, Error>(status)
        }
        .await;
        let restored = self.restore_rules().await;
        let status = checked?;
        restored?;
        self.wait_expected(reaction, "credential restoration")
            .await?;
        self.wait_ready(reaction, "credential restoration readiness")
            .await?;
        Ok(vec![
            Assertion::new("http-status", 403, status),
            Assertion::new(
                "permission-expanded",
                false,
                grants_read(&denied, &input.kind, &input.name),
            ),
            Assertion::new(
                "target-mutations",
                0,
                self.generation(reaction).await? - generation,
            ),
        ])
    }

    fn with_revision(&self, reaction: &Reaction, revision: &str) -> Result<Value> {
        let mut desired = self.target(reaction)?.desired.clone();
        let template = desired
            .pointer_mut("/spec/template")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| Error::Case("target has no Pod template".into()))?;
        let metadata = template.entry("metadata").or_insert_with(|| json!({}));
        metadata["annotations"][REVISION_ANNOTATION] = json!(revision);
        Ok(desired)
    }

    /// With force approved, the controller takes the revision annotation back
    /// from a foreign field manager.
    async fn forced_conflict(&self, reaction: &Reaction) -> Result<Vec<Assertion>> {
        let uid = |object: &Value| object["metadata"]["uid"].as_str().map(str::to_owned);
        let original = uid(&self.target_object(reaction).await?);
        self.driver
            .apply(
                &self.with_revision(reaction, "foreign-forced-conflict")?,
                FOREIGN_MANAGER,
                true,
            )
            .await?;
        let expected = self.expected_revision(reaction, Mutation::None).await?;
        wait_for("approved force reconciles conflict", WAIT, async || {
            Ok((self.revision(reaction).await? == expected).then_some(()))
        })
        .await?;
        self.wait_ready(reaction, "forced conflict readiness")
            .await?;
        Ok(vec![
            Assertion::new(
                "controller-owns-revision",
                true,
                self.owns_revision(reaction, &self.domain_manager).await?,
            ),
            Assertion::new(
                "foreign-manager-displaced",
                true,
                !self.owns_revision(reaction, FOREIGN_MANAGER).await?,
            ),
            Assertion::new(
                "uid-preserved",
                true,
                original == uid(&self.target_object(reaction).await?),
            ),
        ])
    }

    /// Without force, a foreign field manager's value stays and the controller
    /// reports the conflict.
    async fn conflict(&self, reaction: &Reaction) -> Result<Vec<Assertion>> {
        let foreign_value = format!("foreign-{}", reaction.id);
        let foreign = self.with_revision(reaction, &foreign_value)?;
        let repaired = self.with_revision(
            reaction,
            &self.expected_revision(reaction, Mutation::None).await?,
        )?;
        let mark = self.log_mark().await;
        let observed = async {
            self.driver.apply(&foreign, FOREIGN_MANAGER, true).await?;
            let reason = self
                .wait_blocked(mark, &reaction.target, "field conflict reported")
                .await?;
            let held = self.holds(reaction, &foreign_value).await?;
            let owned = self.owns_revision(reaction, FOREIGN_MANAGER).await?;
            Ok::<_, Error>((reason, held, owned))
        }
        .await;
        let repair = async {
            self.driver
                .apply(&repaired, &self.domain_manager, true)
                .await?;
            self.wait_ready(reaction, "field conflict repair readiness")
                .await
        }
        .await;
        let (reason, held, owned) = observed?;
        repair?;
        Ok(vec![
            Assertion::one_of("conflict-reported", &CONFLICT_REASONS, Some(&reason)),
            Assertion::new("force-applied", false, !held),
            Assertion::new("foreign-field-preserved", true, held && owned),
        ])
    }

    async fn out_of_scope(&self) -> Result<Vec<Assertion>> {
        let exists = async || {
            Ok::<_, Error>(
                self.driver
                    .get("v1", "ConfigMap", Some(OUTSIDE), "outside")
                    .await?
                    .is_some(),
            )
        };
        let before = exists().await?;
        let status = self
            .runtime_api(
                "POST",
                &format!("/api/v1/namespaces/{OUTSIDE}/configmaps"),
                Some(&json!({"apiVersion": "v1", "kind": "ConfigMap",
                    "metadata": {"name": "outside", "namespace": OUTSIDE},
                    "data": {"value": "forbidden"}})),
            )
            .await?;
        if (200..=202).contains(&status) {
            let _ = self
                .driver
                .delete("v1", "ConfigMap", Some(OUTSIDE), "outside")
                .await;
            return Err(Error::Case(
                "the controller's credential mutated an undeclared target".into(),
            ));
        }
        if status != 403 {
            return Err(Error::Case(format!(
                "the out-of-scope mutation returned HTTP {status}, expected 403"
            )));
        }
        let after = exists().await?;
        Ok(vec![
            Assertion::new("http-status", 403, status),
            Assertion::new(
                "out-of-scope-mutations",
                0,
                i64::from(after) - i64::from(before),
            ),
        ])
    }

    async fn readiness(&self, reaction: &Reaction) -> Result<Vec<Assertion>> {
        self.wait_ready(reaction, "new-template Pods ready").await?;
        Ok(vec![
            Assertion::new("ready-pods", ">=1", self.ready_pods(reaction).await?),
            Assertion::new("new-template-ready", true, self.ready(reaction).await?),
        ])
    }

    async fn managed_drift(
        &self,
        reaction: &Reaction,
        managed: &[&Input],
    ) -> Result<Vec<Assertion>> {
        let before = self.revision(reaction).await?;
        let generation = self.generation(reaction).await?;
        let (mut drift_observed, mut restored) = (true, true);
        for input in managed {
            let resource = self.managed_resource(input)?;
            let desired = resource.desired["data"].clone();
            let Some(key) = desired
                .as_object()
                .and_then(|data| data.keys().next().cloned())
            else {
                return Err(Error::Case(format!(
                    "managed drift is undefined for empty data: {}",
                    input.id
                )));
            };
            let mut drift = format!("external-drift-{}", input.id);
            if desired[&key] == drift {
                drift.push_str("-different");
            }
            let mut changed = resource.desired.clone();
            changed["data"][&key] = json!(drift);
            // Written under the controller's own field manager, so the edit
            // lands without a conflict and only the controller's
            // authoritative repair can undo it.
            let patched = self
                .driver
                .apply(&changed, &self.domain_manager, false)
                .await?;
            if patched["data"][&key] != drift {
                return Err(Error::Case(format!(
                    "managed drift was not observed for {}",
                    input.id
                )));
            }
            drift_observed &= patched["data"][&key] == drift;
            let current = async || {
                Ok::<_, Error>(
                    self.driver
                        .require(
                            "v1",
                            &resource.kind,
                            Some(&resource.namespace),
                            &resource.name,
                        )
                        .await?["data"]
                        .clone(),
                )
            };
            wait_for(
                &format!("managed source data repaired: {}", input.id),
                WAIT,
                async || Ok((current().await? == desired).then_some(())),
            )
            .await?;
            restored &= current().await? == desired;
        }
        self.stable(reaction, &before).await?;
        let mut assertions = vec![
            Assertion::new("managed-drift-observed", true, drift_observed),
            Assertion::new("managed-data-restored", true, restored),
        ];
        assertions.extend(
            self.unchanged(reaction, &before, generation, "template-mutations")
                .await?,
        );
        Ok(assertions)
    }

    /// The managed source is deleted while the controller may not create it:
    /// the controller must report its write blocked and the dependent target
    /// blocked on that managed input, and change nothing.
    async fn managed_missing(
        &self,
        reaction: &Reaction,
        managed: &[&Input],
    ) -> Result<Vec<Assertion>> {
        let before = self.revision(reaction).await?;
        let generation = self.generation(reaction).await?;
        let mut assertions = Vec::new();
        for input in managed {
            let resource = self.managed_resource(input)?;
            let absent = async || {
                Ok::<_, Error>(
                    self.driver
                        .get(
                            "v1",
                            &resource.kind,
                            Some(&resource.namespace),
                            &resource.name,
                        )
                        .await?
                        .is_none(),
                )
            };
            let mark = self.log_mark().await;
            let checked = async {
                self.scale_controller(0).await?;
                self.set_rules(&without_create(&self.original_rules, &input.kind))
                    .await?;
                self.driver
                    .delete(
                        "v1",
                        &resource.kind,
                        Some(&resource.namespace),
                        &resource.name,
                    )
                    .await?;
                self.scale_controller(1).await?;
                let dependent = self
                    .wait_blocked(mark, &reaction.target, "dependent target blocked")
                    .await?;
                let source = self
                    .wait_blocked(mark, &resource.id, "managed source write blocked")
                    .await?;
                let repair = self
                    .runtime_api(
                        "POST",
                        &format!(
                            "/api/v1/namespaces/{}/{}",
                            resource.namespace,
                            plural(&input.kind)
                        ),
                        Some(&resource.desired),
                    )
                    .await?;
                let missing = absent().await?;
                self.stable(reaction, &before).await?;
                Ok::<_, Error>((dependent, source, missing, repair))
            }
            .await;
            let restored = async {
                self.restore_rules().await?;
                self.scale_controller(1).await?;
                wait_for(
                    &format!(
                        "managed source repair after permission restore: {}",
                        input.id
                    ),
                    WAIT,
                    async || Ok((!absent().await?).then_some(())),
                )
                .await
            }
            .await;
            let (dependent, source, missing, repair) = checked?;
            restored?;
            assertions.extend([
                // A managed source the controller could not apply is not
                // authoritative, which the runtime reports as unavailable.
                Assertion::new(
                    "block-reason",
                    format!("{}:unavailable", input.id),
                    dependent,
                ),
                Assertion::new(
                    "managed-source-write",
                    "kubernetes-write-unavailable",
                    source,
                ),
                Assertion::new("managed-source-absent", true, missing),
                Assertion::new("repair-http-status", 403, repair),
            ]);
        }
        assertions.extend(
            self.unchanged(reaction, &before, generation, "target-mutations")
                .await?,
        );
        Ok(assertions)
    }

    async fn managed_read_denial(
        &self,
        reaction: &Reaction,
        managed: &[&Input],
    ) -> Result<Vec<Assertion>> {
        let before = self.revision(reaction).await?;
        let generation = self.generation(reaction).await?;
        let mut statuses = Vec::new();
        let mut expanded = false;
        for input in managed {
            let resource = self.managed_resource(input)?;
            let denied = without_input(&self.original_rules, &input.kind, &resource.name);
            self.set_rules(&denied).await?;
            let checked = async {
                let status = self
                    .runtime_api(
                        "GET",
                        &format!(
                            "/api/v1/namespaces/{}/{}/{}",
                            resource.namespace,
                            plural(&input.kind),
                            resource.name
                        ),
                        None,
                    )
                    .await?;
                self.stable(reaction, &before).await?;
                Ok::<_, Error>(status)
            }
            .await;
            let restored = self.restore_rules().await;
            statuses.push(checked?);
            restored?;
            expanded |= grants_read(&denied, &input.kind, &resource.name);
        }
        let status = if !statuses.is_empty() && statuses.iter().all(|status| *status == 403) {
            403
        } else {
            statuses.first().copied().unwrap_or_default()
        };
        let mut assertions = vec![
            Assertion::new("http-status", 403, status),
            Assertion::new("permission-expanded", false, expanded),
        ];
        assertions.extend(
            self.unchanged(reaction, &before, generation, "target-mutations")
                .await?,
        );
        Ok(assertions)
    }

    async fn managed_unavailable(&self, reaction: &Reaction) -> Result<Vec<Assertion>> {
        let before = self.revision(reaction).await?;
        let generation = self.generation(reaction).await?;
        let mark = self.log_mark().await;
        self.set_api_egress(false).await?;
        let observed = async {
            let pod = self.controller_pod().await?.ok_or_else(|| {
                Error::Case("controller Pod missing before the transport failure".into())
            })?;
            self.driver
                .exec(
                    &self.control,
                    &pod,
                    "loopback-proxy",
                    &["nginx", "-c", "/tmp/apsides-nginx.conf", "-s", "stop"],
                )
                .await?;
            let reason = self
                .wait_blocked(
                    mark,
                    &reaction.target,
                    "managed source transport unavailable",
                )
                .await?;
            self.stable(reaction, &before).await?;
            Ok::<_, Error>(reason)
        }
        .await;
        let reopened = async {
            self.set_api_egress(true).await?;
            self.scale_controller(0).await?;
            self.scale_controller(1).await
        }
        .await;
        let reason = observed?;
        reopened?;
        self.wait_expected(reaction, "managed source transport restored")
            .await?;
        // Every input is unreachable, so the reason may name any input of any
        // reaction on this target, not only this reaction's.
        let (input, kind) = reason.rsplit_once(':').unwrap_or_default();
        let mut inputs = self
            .release
            .plan
            .ir
            .reactions
            .iter()
            .filter(|other| other.target == reaction.target)
            .flat_map(|other| other.inputs.iter().map(String::as_str))
            .collect::<Vec<_>>();
        inputs.sort_unstable();
        inputs.dedup();
        let mut assertions = vec![
            Assertion::new("block-reason", "unavailable", kind),
            Assertion::one_of("blocked-input", &inputs, Some(input)),
        ];
        assertions.extend(
            self.unchanged(reaction, &before, generation, "target-mutations")
                .await?,
        );
        Ok(assertions)
    }
}

/// The last reason in `lines` the controller gave for blocking `target`.
fn blocked_reason(lines: &[&str], target: &str) -> Option<String> {
    lines
        .iter()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|line| {
            line["status"]["blocked"]
                .as_array()?
                .iter()
                .find(|blocked| blocked["target"] == target)
                .and_then(|blocked| blocked["reason"].as_str().map(str::to_owned))
        })
}

/// Whether `manager`'s managed-fields entry on `object` covers the Pod
/// template's revision annotation.
fn owns_revision(object: &Value, manager: &str) -> bool {
    let annotation = format!("f:{REVISION_ANNOTATION}");
    object["metadata"]["managedFields"]
        .as_array()
        .is_some_and(|fields| {
            fields.iter().any(|field| {
                field["manager"] == manager
                    && field["fieldsV1"]["f:spec"]["f:template"]["f:metadata"]["f:annotations"]
                        .get(&annotation)
                        .is_some()
            })
        })
}

fn phase(pod: &Pod) -> &str {
    pod.status
        .as_ref()
        .and_then(|status| status.phase.as_deref())
        .unwrap_or_default()
}

/// The mutation-check cases: one per reaction, in coverage order.
pub fn mutation_cases(release: &FixtureRelease) -> Vec<TestCase> {
    release
        .coverage
        .iter()
        .filter(|case| case.scenario == MUTATION_SCENARIO)
        .filter_map(|case| {
            let reaction = release
                .plan
                .ir
                .reactions
                .iter()
                .find(|reaction| reaction.id == case.reaction_id)?;
            Some(TestCase {
                reaction_id: reaction.id.clone(),
                input_ids: reaction.inputs.clone(),
                source: reaction.source.clone(),
                scenario: MUTATION_SCENARIO.into(),
                layer: "workload".into(),
                assertion: "removing this reaction's contribution changes the expected revision"
                    .into(),
                required: case.status == "required",
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_latest_blocked_reason_for_a_target_is_read_after_a_mark() {
        let log = [
            r#"{"status":{"applied":[],"blocked":[{"target":"deployment-api","reason":"a:missing"}]}}"#,
            "not json",
            r#"{"status":{"applied":["deployment-api"],"blocked":[{"target":"configmap","reason":"kubernetes-write-unavailable"}]}}"#,
        ];
        assert_eq!(
            blocked_reason(&log, "deployment-api").as_deref(),
            Some("a:missing")
        );
        assert_eq!(blocked_reason(&log[1..], "deployment-api"), None);
        assert_eq!(
            blocked_reason(&log, "configmap").as_deref(),
            Some("kubernetes-write-unavailable")
        );
    }

    #[test]
    fn revision_ownership_is_read_from_managed_fields() {
        let annotation = format!("f:{REVISION_ANNOTATION}");
        let object = json!({"metadata": {"managedFields": [
            {"manager": "apsides-unit", "operation": "Apply", "fieldsV1": {"f:spec": {"f:template":
                {"f:metadata": {"f:annotations": {annotation: {}}}}}}},
            {"manager": FOREIGN_MANAGER, "operation": "Apply", "fieldsV1": {"f:spec": {"f:replicas": {}}}},
        ]}});
        assert!(owns_revision(&object, "apsides-unit"));
        assert!(!owns_revision(&object, FOREIGN_MANAGER));
    }

    fn output_input() -> Input {
        Input {
            id: "output-flags-input".into(),
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            name: "flags".into(),
            namespace: "workload".into(),
            mode: InputMode::Output,
            managed_resource_id: Some("output-flags".into()),
            projection: vec!["data".into()],
            sensitive: false,
            missing: "block-dependent-mutations".into(),
            source: compiler_core::SourceLocation {
                file: "program.tsx".into(),
                line: 1,
                column: 1,
            },
        }
    }

    #[test]
    fn handler_output_stimulus_is_pure_ssa_of_only_the_changed_data_under_the_handler_manager() {
        let input = output_input();
        let data = Map::from_iter([("message".to_owned(), json!("cGF5bG9hZA=="))]);
        let payload = handler_output_payload(&input, &data);
        assert_eq!(payload["apiVersion"], json!("v1"));
        assert_eq!(payload["kind"], json!("ConfigMap"));
        assert_eq!(payload["metadata"]["name"], json!("flags"));
        assert_eq!(payload["metadata"]["namespace"], json!("workload"));
        assert_eq!(payload["data"], json!({"message": "cGF5bG9hZA=="}));
        // The reconciler's own apply of the handler output is metadata-only
        // (see IR-CONTRACT.md); the handler stimulus payload never carries
        // metadata the reconciler owns, such as the reserved label.
        assert!(payload["metadata"].get("labels").is_none());
        assert_eq!(handler_field_manager("example"), "apsides-handler-example");
        assert_ne!(handler_field_manager("example"), "apsides-example");
    }

    /// A program with `rolloutOn` on a handler output must generate only the
    /// small `OUTPUT` scenario set's cases for that reaction, and
    /// `execute()`'s stimulus selection (replicated here without a live
    /// cluster) must resolve every one of them to exactly the output's own
    /// `Output`-mode input — never empty (which would make `NEEDS_OBSERVED`
    /// reject the case, the exact failure B3 reported) and never mixed with
    /// another input.
    #[test]
    fn output_rollout_produces_only_applicable_cases_with_resolvable_stimulus() -> Result<()> {
        let source = json!({"file": "program.tsx", "line": 1, "column": 1});
        let ir = json!({
            "schemaVersion": "apsides.dev/ir/v2",
            "domain": {"id": "example", "namespace": "workload", "controlNamespace": "apsides-system",
                "clusterBinding": "test-cluster", "policyRef": "platform/example", "enforcement": "api-bound",
                "minKubernetesVersion": "1.34",
                "budgets": {"maxResources": 32, "maxInputs": 32, "maxReactions": 32, "maxObjectBytes": 262_144, "maxQueueDepth": 32, "maxConcurrentRequests": 4}},
            "resources": [
                {"id": "api", "apiVersion": "apps/v1", "kind": "Deployment", "name": "api", "namespace": "workload",
                    "desired": {"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "api", "namespace": "workload"},
                        "spec": {"replicas": 1, "selector": {"matchLabels": {"app": "api"}},
                            "template": {"metadata": {"labels": {"app": "api"}},
                                "spec": {"serviceAccountName": "api-workload",
                                    "containers": [{"name": "api", "image": format!("example.invalid/api@sha256:{}", "0".repeat(64))}]}}}},
                    "lifecycle": {"deletionPolicy": "orphan"}, "source": source},
                {"id": "output-flags", "apiVersion": "v1", "kind": "ConfigMap", "name": "flags", "namespace": "workload",
                    "desired": {"apiVersion": "v1", "kind": "ConfigMap",
                        "metadata": {"name": "flags", "namespace": "workload", "labels": {"apsides.dev/handler-output": "output-flags"}}},
                    "lifecycle": {"deletionPolicy": "orphan"}, "source": source, "handlerOutput": true},
            ],
            "inputs": [
                {"id": "settings", "apiVersion": "v1", "kind": "ConfigMap", "name": "settings", "namespace": "workload",
                    "mode": "observed", "projection": ["data"], "sensitive": false,
                    "missing": "block-dependent-mutations", "source": source},
                {"id": "output-flags-input", "apiVersion": "v1", "kind": "ConfigMap", "name": "flags", "namespace": "workload",
                    "mode": "output", "managedResourceId": "output-flags", "projection": ["data"], "sensitive": false,
                    "missing": "block-dependent-mutations", "source": source},
            ],
            "reactions": [
                {"id": "rollout-flags", "op": "rollout", "inputs": ["output-flags-input"], "target": "api", "source": source},
            ],
            "handlers": [{
                "id": "sync-flags", "module": "handlers/sync-flags.ts", "export": "syncFlags",
                "trigger": {"kind": "input-changed", "input": "settings"},
                "inputs": ["settings"], "egress": [{"host": "api.example.com", "port": 443}],
                "outputs": ["output-flags"], "timeoutMillis": 30000, "maxHeapBytes": 67_108_864,
                "bundleSha256": "1".repeat(64), "source": source,
            }],
            "outputs": [{"id": "output-flags", "name": "flags", "namespace": "workload"}],
            "provenance": {"compiler": "apsides/0.1", "inputs": [{"path": "program.tsx", "sha256": "0".repeat(64)}, {"path": "handlers/sync-flags.ts", "sha256": "1".repeat(64)}]},
        });
        let plan = compiler_core::parse_and_validate(&ir.to_string())
            .map_err(|error| Error::Case(error.to_string()))?;
        let reaction = plan
            .ir
            .reactions
            .iter()
            .find(|reaction| reaction.id == "rollout-flags")
            .ok_or_else(|| Error::Case("reaction missing".into()))?;
        let cases: Vec<_> = plan
            .test_plan
            .iter()
            .filter(|case| case.reaction_id == "rollout-flags")
            .collect();
        assert!(!cases.is_empty());
        for case in &cases {
            // Exactly `execute()`'s stimulus selection.
            let stimulus: Vec<_> = plan
                .ir
                .inputs
                .iter()
                .filter(|input| {
                    reaction.inputs.contains(&input.id) && case.input_ids.contains(&input.id)
                })
                .collect();
            let observed: Vec<_> = stimulus
                .iter()
                .copied()
                .filter(|input| matches!(input.mode, InputMode::Observed | InputMode::Output))
                .collect();
            if NEEDS_OBSERVED.contains(&case.scenario.as_str()) {
                assert!(
                    !observed.is_empty(),
                    "scenario {} has no observed-or-output stimulus (case: {case:?})",
                    case.scenario
                );
                assert_eq!(
                    observed
                        .iter()
                        .map(|input| input.id.as_str())
                        .collect::<Vec<_>>(),
                    ["output-flags-input"],
                    "scenario {} resolved unexpected stimulus",
                    case.scenario
                );
            }
        }
        // The scenarios B3 explicitly reported as broken (`missing-input`,
        // which cannot apply since the reconciler recreates the output
        // absent-only before anything observes it) must be entirely absent.
        assert!(
            !cases.iter().any(|case| case.scenario == "missing-input"),
            "missing-input must never be generated for an output reaction"
        );
        Ok(())
    }
}
