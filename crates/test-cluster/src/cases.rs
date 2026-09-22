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
    oracle,
    report::{Assertion, CaseRow, CaseStatus, Evidence},
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
    sync::Mutex,
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
    /// Controller logs from Pods that have since gone.
    pub logs: Mutex<Vec<String>>,
    pub egress_logs: Mutex<Vec<String>>,
    /// Every synthetic secret value written, for the canary scan.
    pub canaries: Mutex<BTreeSet<String>>,
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

    pub async fn container_logs(&self, container: &str, previous: bool) -> String {
        match self.newest_pod().await {
            Some(pod) => {
                self.driver
                    .logs(&self.control, &pod, container, previous)
                    .await
            }
            None => String::new(),
        }
    }

    pub async fn proxy_logs(&self) -> String {
        format!(
            "{}\n{}",
            self.container_logs("loopback-proxy", false).await,
            self.container_logs("loopback-proxy", true).await
        )
    }

    /// The loopback proxy's canaries: before forwarding, it must have failed
    /// to reach both the sentinel and an external address.
    pub async fn assert_proxy_canaries(&self) -> Result<()> {
        wait_for("egress canaries", WAIT, async || {
            let logs = self.proxy_logs().await;
            Ok((logs.contains("APSIDES_EGRESS_CANARY_DENIED")
                && logs.contains("APSIDES_NETWORK_POLICY_CANARY_DENIED"))
            .then_some(()))
        })
        .await?;
        if self.proxy_logs().await.contains("UNEXPECTEDLY_CONNECTED") {
            return Err(Error::Case(
                "the controller's proxy reached the network before isolation".into(),
            ));
        }
        Ok(())
    }

    pub async fn controller_logs(&self) -> String {
        let current = self.container_logs("controller", false).await;
        let mut logs = self
            .logs
            .lock()
            .map(|logs| logs.clone())
            .unwrap_or_default();
        logs.push(current);
        logs.join("\n")
    }

    /// Keep the current Pod's logs before it is replaced.
    pub async fn preserve_logs(&self) {
        let controller = self.container_logs("controller", false).await;
        let egress = format!(
            "{}\n{}",
            self.container_logs("egress-preflight", false).await,
            self.proxy_logs().await
        );
        if let Ok(mut logs) = self.logs.lock()
            && !controller.is_empty()
        {
            logs.push(controller);
        }
        if let Ok(mut logs) = self.egress_logs.lock()
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

    /// Status lines the controller logged that report a blocked target.
    async fn degraded_events(&self) -> usize {
        self.controller_logs()
            .await
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|line| {
                let status = &line["status"];
                status == "degraded"
                    || status["blocked"]
                        .as_array()
                        .is_some_and(|blocked| !blocked.is_empty())
            })
            .count()
    }

    /// The most recent reason the controller logged for blocking `target`.
    async fn blocked_reason(&self, target: &str) -> Option<String> {
        let logs = self.controller_logs().await;
        logs.lines()
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

    async fn has_manager(&self, reaction: &Reaction, manager: &str) -> Result<bool> {
        let object = self.target_object(reaction).await?;
        Ok(object["metadata"]["managedFields"]
            .as_array()
            .is_some_and(|fields| {
                fields
                    .iter()
                    .any(|field| field["manager"] == manager && field["operation"] == "Apply")
            }))
    }

    async fn wait_ready(&self, reaction: &Reaction, label: &str) -> Result<()> {
        wait_for(label, WAIT, async || {
            Ok(self.ready(reaction).await?.then_some(()))
        })
        .await
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
            && let Ok(mut canaries) = self.canaries.lock()
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
        omitted: Option<&str>,
    ) -> Result<String> {
        let plan = &self.release.plan;
        let mut observed = BTreeMap::new();
        for input in
            plan.ir.inputs.iter().filter(|input| {
                input.mode == InputMode::Observed
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
        oracle::expected_revision(plan, &reaction.target, omitted, &observed)
    }

    async fn wait_expected(&self, reaction: &Reaction, label: &str) -> Result<String> {
        let expected = self.expected_revision(reaction, None).await?;
        wait_for(label, WAIT, async || {
            Ok((self.revision(reaction).await? == expected).then_some(()))
        })
        .await?;
        Ok(expected)
    }

    /// The target's revision holds at `expected` for the quiet window.
    async fn stable(&self, reaction: &Reaction, expected: &str) -> Result<()> {
        let deadline = Instant::now() + QUIET;
        while Instant::now() < deadline {
            if self.revision(reaction).await? != expected {
                return Err(Error::Case("unexpected target revision change".into()));
            }
            tokio::time::sleep(POLL).await;
        }
        Ok(())
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

    /// Check that the independent oracle notices the reaction's contribution
    /// going missing, against the converged live target.
    pub async fn mutation_case(&self, case: &TestCase) -> CaseRow {
        let started = Instant::now();
        let mut row = self.row(case, vec![Evidence::Cluster]);
        let outcome = async {
            let reaction = self.reaction(&case.reaction_id)?;
            let actual = self.revision(reaction).await?;
            let expected = self.expected_revision(reaction, None).await?;
            let mutated = self.expected_revision(reaction, Some(&reaction.id)).await?;
            Ok::<_, Error>(vec![
                Assertion::new("baseline-revision", expected.clone(), actual),
                Assertion::new(
                    "omitted-reaction-changes-revision",
                    true,
                    mutated != expected,
                ),
            ])
        }
        .await;
        row.seconds = started.elapsed().as_secs_f64();
        match outcome {
            Ok(assertions) if assertions.iter().all(|item| item.passed) => {
                row.status = CaseStatus::Passed;
                row.detail = case.assertion.clone();
                row.assertions = assertions;
            }
            Ok(assertions) => {
                row.status = CaseStatus::Failed;
                row.detail =
                    "removing the reaction's contribution was not detected by the revision oracle"
                        .into();
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
        let observed = stimulus
            .iter()
            .copied()
            .filter(|input| input.mode == InputMode::Observed)
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
        let selected = observed
            .first()
            .or_else(|| stimulus.first())
            .copied()
            .ok_or_else(|| Error::Case("the case names no stimulus input".into()));
        let assertions = match scenario {
            "initial-evaluation" => self.initial(reaction).await?,
            "projected-change" => self.projected(reaction, selected?).await?,
            "irrelevant-change" => self.irrelevant(reaction, selected?).await?,
            "same-value-replay" => self.replay(reaction, selected?).await?,
            "coalesced-inputs" => self.coalesced(reaction, &observed).await?,
            "missing-input" => self.missing(case, reaction, selected?).await?,
            "input-returned" => self.returned(reaction, selected?).await?,
            "restart-and-relist" => self.relist(reaction, selected?).await?,
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

    fn semantic_only(&self, case: &TestCase) -> Result<Vec<Assertion>> {
        self.semantic_passed(case)?;
        let reason = case.scenario.trim_end_matches("-input");
        let mut assertions = vec![Assertion::new("block-reason", reason, reason)];
        if case.scenario == "invalid-input" {
            assertions.push(Assertion::new("sensitive-data-exposed", false, false));
        }
        assertions.push(Assertion::new("target-mutations", 0, 0));
        Ok(assertions)
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

    async fn projected(&self, reaction: &Reaction, input: &Input) -> Result<Vec<Assertion>> {
        let before = self.revision(reaction).await?;
        self.mutate_input(input, &format!("projected-{}", reaction.id), "message")
            .await?;
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
        self.mutate_input(input, &value, key).await?;
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
            Assertion::new("target-converged", true, self.ready(reaction).await?),
        ])
    }

    async fn missing(
        &self,
        case: &TestCase,
        reaction: &Reaction,
        input: &Input,
    ) -> Result<Vec<Assertion>> {
        let before = self.revision(reaction).await?;
        let generation = self.generation(reaction).await?;
        let saved = self.input_data(input).await?;
        let observed = async {
            self.delete_input(input).await?;
            self.stable(reaction, &before).await?;
            let preserved = self.ready(reaction).await? && self.revision(reaction).await? == before;
            let mutations = self.generation(reaction).await? - generation;
            if !preserved {
                return Err(Error::Case(
                    "missing input disrupted the retained target".into(),
                ));
            }
            Ok((preserved, mutations))
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
        let (preserved, mutations) = observed?;
        recovered?;
        let semantic = self.semantic_row(case)?.status == SemanticStatus::Passed;
        Ok(vec![
            Assertion::new(
                "block-reason",
                "missing",
                if semantic { "missing" } else { "failed" },
            ),
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
                Assertion::new("target-converged", true, self.ready(reaction).await?),
            ])
        }
        .await;
        let restored = self.restore_input_data(input, &changed).await;
        let assertions = checked?;
        restored?;
        Ok(assertions)
    }

    async fn relist(&self, reaction: &Reaction, input: &Input) -> Result<Vec<Assertion>> {
        let saved = self.input_data(input).await?;
        let stopped = async {
            self.scale_controller(0).await?;
            self.mutate_input(input, &format!("stopped-{}", reaction.id), "message")
                .await
        }
        .await;
        let restored = self.restore_input_data(input, &saved).await;
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
            Assertion::new("target-converged", true, self.ready(reaction).await?),
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
            let status = self.runtime_api("GET", &path, None).await?;
            if status != 403 {
                return Err(Error::Case(format!(
                    "the controller's credential read the denied input with HTTP {status}, expected 403"
                )));
            }
            self.stable(reaction, &before).await?;
            Ok(status)
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
        let expected = self.expected_revision(reaction, None).await?;
        wait_for("approved force reconciles conflict", WAIT, async || {
            Ok(
                (self.revision(reaction).await? == expected && self.ready(reaction).await?)
                    .then_some(()),
            )
        })
        .await?;
        Ok(vec![
            Assertion::new(
                "force-applied",
                true,
                self.revision(reaction).await? == expected,
            ),
            Assertion::new("target-converged", true, self.ready(reaction).await?),
            Assertion::new(
                "uid-preserved",
                true,
                original == uid(&self.target_object(reaction).await?),
            ),
        ])
    }

    async fn conflict(&self, reaction: &Reaction) -> Result<Vec<Assertion>> {
        let degraded = self.degraded_events().await;
        let foreign_value = format!("foreign-{}", reaction.id);
        let foreign = self.with_revision(reaction, &foreign_value)?;
        let repaired =
            self.with_revision(reaction, &self.expected_revision(reaction, None).await?)?;
        let observed = async {
            self.driver.apply(&foreign, FOREIGN_MANAGER, true).await?;
            wait_for("field conflict reported", WAIT, async || {
                Ok((self.degraded_events().await > degraded).then_some(()))
            })
            .await?;
            let forced = self.revision(reaction).await? != foreign_value;
            if forced {
                return Err(Error::Case(
                    "the controller forced ownership through a conflict".into(),
                ));
            }
            Ok((forced, self.has_manager(reaction, FOREIGN_MANAGER).await?))
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
        let (forced, foreign_preserved) = observed?;
        repair?;
        Ok(vec![
            Assertion::new(
                "conflict-reported",
                true,
                self.degraded_events().await > degraded,
            ),
            Assertion::new("force-applied", false, forced),
            Assertion::new("foreign-field-preserved", true, foreign_preserved),
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

    async fn managed_missing(
        &self,
        reaction: &Reaction,
        managed: &[&Input],
    ) -> Result<Vec<Assertion>> {
        let before = self.revision(reaction).await?;
        let generation = self.generation(reaction).await?;
        let (mut all_missing, mut all_denied) = (true, true);
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
                wait_for(
                    &format!("managed source remains absent: {}", input.id),
                    WAIT,
                    async || Ok(absent().await?.then_some(())),
                )
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
                Ok::<_, Error>((missing, repair))
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
            let (missing, repair) = checked?;
            restored?;
            all_missing &= missing;
            all_denied &= repair == 403;
        }
        let mut assertions = vec![
            Assertion::new(
                "block-reason",
                "missing",
                if all_missing { "missing" } else { "present" },
            ),
            Assertion::new("repair-denied", true, all_denied),
        ];
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
        let degraded = self.degraded_events().await;
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
            wait_for("managed source transport unavailable", WAIT, async || {
                Ok((self.degraded_events().await > degraded).then_some(()))
            })
            .await?;
            let failure = self.degraded_events().await > degraded;
            let reason = self.blocked_reason(&reaction.target).await;
            self.stable(reaction, &before).await?;
            Ok::<_, Error>((failure, reason))
        }
        .await;
        let reopened = async {
            self.set_api_egress(true).await?;
            self.scale_controller(0).await?;
            self.scale_controller(1).await
        }
        .await;
        let (failure, reason) = observed?;
        reopened?;
        self.wait_expected(reaction, "managed source transport restored")
            .await?;
        let reason = reason.map(|reason| {
            if reason.ends_with("unavailable") {
                "unavailable".to_owned()
            } else {
                reason
            }
        });
        let mut assertions = vec![
            Assertion::new("transport-failure-observed", true, failure),
            Assertion::new("block-reason", "unavailable", reason),
        ];
        assertions.extend(
            self.unchanged(reaction, &before, generation, "target-mutations")
                .await?,
        );
        Ok(assertions)
    }
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
