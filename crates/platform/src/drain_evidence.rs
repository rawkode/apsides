use serde::{Deserialize, Serialize};

/// Produced from live UID-bound objects, never caller-supplied receipt strings.
/// This proves only the checked process observations; API drain is separate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessTerminationObservation {
    pub(crate) deployment_uid: String,
    pub(crate) deployment_resource_version: String,
    pub(crate) pod_resource_versions: Vec<(String, String)>,
    pub(crate) container_ids: Vec<String>,
}

/// Produced by actual UID-bound Kubernetes reads and isolated helper execution.
/// This is a proxy observation. It is not sufficient to activate another writer:
/// runtime/proxy termination, credential retirement and policy qualification are
/// separate mandatory checks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProxyDrainObservation {
    pub(crate) pod_uid: String,
    pub(crate) pod_resource_version: String,
    pub(crate) container_id: String,
    pub(crate) qualification_digest: String,
    pub(crate) ledger_digest: String,
    pub(crate) completed_requests: u64,
    pub(crate) rejected_after_freeze: u64,
}

#[cfg(test)]
mod tests {
    use crate::UpgradeDrainState;
    use serde_json::{Value, json};

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

    fn legacy_drain() -> Value {
        json!({
            "previousTransitionId": "transition-a",
            "runtimeWriters": {
                "deploymentName": "runtime-a",
                "deploymentUid": "runtime-deployment-uid",
                "serviceAccountName": "runtime-account",
                "pods": [{
                    "name": "runtime-pod", "uid": "runtime-pod-uid",
                    "nodeName": "node-a", "nodeUid": "node-uid",
                    "containers": {"regular": [{"name":"controller","containerId":"containerd://runtime"}],"init":[],"ephemeral":[]}
                }]
            },
            "proxyWriters": {
                "deploymentName": "proxy-a",
                "deploymentUid": "proxy-deployment-uid",
                "serviceAccountName": "runtime-account",
                "pods": [{
                    "name": "proxy-pod", "uid": "proxy-pod-uid",
                    "nodeName": "node-a", "nodeUid": "node-uid",
                    "containers": {"regular": [{"name":"envoy","containerId":"containerd://envoy"},{"name":"admin","containerId":"containerd://admin"}],"init":[],"ephemeral":[]}
                }]
            },
            "runtimeTermination": {
                "deploymentUid": "runtime-deployment-uid",
                "deploymentResourceVersion": "12",
                "podResourceVersions": [["runtime-pod-uid", "13"]],
                "containerIds": ["containerd://runtime"]
            },
            "finalProxyLedger": {
                "podUid": "proxy-pod-uid", "podResourceVersion": "14",
                "containerId": "containerd://envoy",
                "qualificationDigest": format!("sha256:{}", "1".repeat(64)),
                "ledgerDigest": format!("sha256:{}", "2".repeat(64)),
                "completedRequests": 7, "rejectedAfterFreeze": 2
            },
            "proxyTermination": {
                "deploymentUid": "proxy-deployment-uid",
                "deploymentResourceVersion": "15",
                "podResourceVersions": [["proxy-pod-uid", "16"]],
                "containerIds": ["containerd://envoy", "containerd://admin"]
            },
            "retiredIdentityUids": {
                "v1/ServiceAccount/control/runtime-account": "account-uid"
            }
        })
    }

    fn assert_legacy_compatibility(legacy: &Value) -> TestResult {
        fn optional(value: &Value) -> Option<&Value> {
            (!value.is_null()).then_some(value)
        }

        let typed: UpgradeDrainState = serde_json::from_value(legacy.clone())?;
        assert_eq!(serde_json::to_value(&typed)?, *legacy);

        let runtime = legacy
            .get("runtimeTermination")
            .ok_or("legacy runtime termination missing")?;
        let ledger = legacy
            .get("finalProxyLedger")
            .ok_or("legacy proxy ledger missing")?;
        let proxy = legacy
            .get("proxyTermination")
            .ok_or("legacy proxy termination missing")?;
        assert_eq!(
            compiler_core::digest(&(
                typed.runtime_termination.as_ref(),
                typed.final_proxy_ledger.as_ref(),
                typed.proxy_termination.as_ref(),
                "17",
            ))?,
            compiler_core::digest(&(optional(runtime), optional(ledger), optional(proxy), "17",))?
        );
        Ok(())
    }

    #[test]
    fn typed_drain_preserves_legacy_wire_and_evidence_digest() -> TestResult {
        let legacy = legacy_drain();
        assert_legacy_compatibility(&legacy)
    }

    #[test]
    fn partial_typed_drain_preserves_null_wire_and_evidence_digest() -> TestResult {
        for absent in [
            &["runtimeTermination", "finalProxyLedger", "proxyTermination"][..],
            &["finalProxyLedger", "proxyTermination"][..],
            &["proxyTermination"][..],
        ] {
            let mut legacy = legacy_drain();
            for field in absent {
                legacy[*field] = Value::Null;
            }
            assert_legacy_compatibility(&legacy)?;
        }
        Ok(())
    }

    #[test]
    fn typed_drain_rejects_unknown_evidence_fields() -> TestResult {
        for field in ["runtimeTermination", "finalProxyLedger", "proxyTermination"] {
            let mut legacy = legacy_drain();
            legacy
                .get_mut(field)
                .and_then(Value::as_object_mut)
                .ok_or("legacy evidence object missing")?
                .insert("privateCanary".to_owned(), json!("must-not-be-retained"));
            assert!(serde_json::from_value::<UpgradeDrainState>(legacy).is_err());
        }
        Ok(())
    }

    #[test]
    fn typed_drain_rejects_missing_or_malformed_required_evidence() -> TestResult {
        let mut cases = Vec::new();
        for (field, required) in [
            ("runtimeTermination", "deploymentUid"),
            ("finalProxyLedger", "ledgerDigest"),
            ("proxyTermination", "containerIds"),
        ] {
            let mut legacy = legacy_drain();
            legacy
                .get_mut(field)
                .and_then(Value::as_object_mut)
                .ok_or("legacy evidence object missing")?
                .remove(required);
            cases.push(legacy);
        }
        for count in [
            json!(-1),
            serde_json::from_str::<Value>("18446744073709551616")?,
        ] {
            let mut legacy = legacy_drain();
            legacy["finalProxyLedger"]["completedRequests"] = count;
            cases.push(legacy);
        }
        for legacy in cases {
            assert!(serde_json::from_value::<UpgradeDrainState>(legacy).is_err());
        }
        Ok(())
    }
}
