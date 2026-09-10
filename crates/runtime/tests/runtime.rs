use std::{collections::BTreeMap, sync::Mutex, time::Duration};

use async_trait::async_trait;
use compiler_core::parse_and_validate;
use runtime::{FixtureConfig, Observation, RunReport, RuntimeClient, RuntimeError, reconcile_once};
use serde_json::Value;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const IR: &str = r#"{
  "schemaVersion":"apsides.dev/ir/v1",
  "domain":{"id":"example","namespace":"workload","controlNamespace":"apsides-system","clusterBinding":"fixture","policyRef":"platform/example","enforcement":"api-bound","budgets":{"maxResources":8,"maxInputs":8,"maxReactions":8,"maxObjectBytes":262144,"maxQueueDepth":8,"maxConcurrentRequests":2}},
  "resources":[
    {"id":"deployment","apiVersion":"apps/v1","kind":"Deployment","name":"api","namespace":"workload","desired":{"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"api","namespace":"workload"},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"api"}},"template":{"metadata":{"labels":{"app":"api"}},"spec":{"containers":[{"name":"api","image":"fixture.invalid/api@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","imagePullPolicy":"Never"}]}}}},"lifecycle":{"deletionPolicy":"orphan"},"source":{"file":"controller.ts","line":1,"column":1}}
  ],
  "inputs":[{"id":"config","apiVersion":"v1","kind":"ConfigMap","name":"input","namespace":"workload","mode":"observed","projection":["data"],"sensitive":false,"missing":"block-dependent-mutations","source":{"file":"controller.ts","line":2,"column":1}}],
  "reactions":[{"id":"rollout","op":"rollout","inputs":["config"],"target":"deployment","source":{"file":"controller.ts","line":3,"column":1}}],
  "provenance":{"compiler":"apsides/0.1","inputs":[{"path":"controller.ts","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]}
}"#;

#[derive(Default)]
struct FakeClient {
    observation: Mutex<Option<Observation>>,
    writes: Mutex<Vec<(String, Value)>>,
}

#[async_trait]
impl RuntimeClient for FakeClient {
    async fn apply_ssa(
        &self,
        resource: &compiler_core::Resource,
        desired: &Value,
    ) -> Result<(), RuntimeError> {
        self.writes
            .lock()
            .map_err(|_| RuntimeError::Kubernetes("writes mutex is poisoned".into()))?
            .push((resource.id.clone(), desired.clone()));
        Ok(())
    }
    async fn observe_named(&self, _: &compiler_core::Input) -> Result<Observation, RuntimeError> {
        Ok(self
            .observation
            .lock()
            .map_err(|_| RuntimeError::Kubernetes("observation mutex is poisoned".into()))?
            .clone()
            .unwrap_or(Observation::Missing))
    }
    async fn wait_for_named_change(
        &self,
        _: &[compiler_core::Input],
        _: Duration,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}

#[tokio::test]
async fn missing_input_blocks_the_dependent_target_without_a_write() -> TestResult {
    let plan = parse_and_validate(IR)?;
    let client = FakeClient::default();
    let report = reconcile_once(&plan, None, &client).await?;
    assert_eq!(report.applied, Vec::<String>::new());
    assert_eq!(report.blocked[0].target, "deployment");
    assert!(
        client
            .writes
            .lock()
            .map_err(|_| RuntimeError::Kubernetes("writes mutex is poisoned".into()))?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn available_input_produces_a_stable_revision_and_ssa_write() -> TestResult {
    let plan = parse_and_validate(IR)?;
    let client = FakeClient {
        observation: Mutex::new(Some(Observation::Available {
            data: BTreeMap::from([("message".into(), "vanilla".into())]),
            resource_version: "10".into(),
        })),
        ..Default::default()
    };
    let first = reconcile_once(&plan, None, &client).await?;
    let second = reconcile_once(&plan, None, &client).await?;
    assert_eq!(
        first,
        RunReport {
            applied: vec!["deployment".into()],
            blocked: vec![]
        }
    );
    assert_eq!(second, first);
    let writes = client
        .writes
        .lock()
        .map_err(|_| RuntimeError::Kubernetes("writes mutex is poisoned".into()))?;
    let a = writes[0]
        .1
        .pointer("/spec/template/metadata/annotations/apsides.dev~1revision");
    let b = writes[1]
        .1
        .pointer("/spec/template/metadata/annotations/apsides.dev~1revision");
    assert_eq!(a, b);
    assert!(
        a.and_then(Value::as_str)
            .ok_or("write carries no revision annotation")?
            .starts_with("sha256:")
    );
    Ok(())
}

#[test]
fn fixture_config_is_explicitly_bounded() {
    let _ = FixtureConfig {
        acknowledgement: "disposable-cluster-only".into(),
        ir_digest: "sha256:unused".into(),
        cluster_binding: "fixture".into(),
        kubeconfig: "/tmp/fixture-kubeconfig".into(),
        context: "kind-disposable-context".into(),
        poll_interval_millis: 100,
        max_watch_cycles: 1,
        rollout_key: None,
    };
}
