//! Live inspection of a separately protected Envoy proxy. This module does not
//! turn caller-provided hashes into upgrade authorization.
use crate::{PlatformClient, ProxyDrainObservation, ProxyIdentity, Result, invalid, valid_digest};
use compiler_core::{digest, parse_strict};
use k8s_openapi::api::core::v1::{ConfigMap, Pod};
use kube::{Api, api::AttachParams};
use serde::Deserialize;
use serde_json::Value;

// Envoy's unfiltered JSON includes fractional histogram quantiles. Ask the
// standard admin filter for exactly the four integral counters/gauges used by
// the proof, retaining the strict duplicate-safe integer parser below.
const STATS_URL: &str = "http://localhost/stats?format=json&filter=%5E%28server%5C.live%7Chttp%5C.apsides%5C.downstream_rq_active%7Ccluster%5C.write_api%5C.upstream_rq_%28active%7Ctotal%29%29%24";
use std::{collections::BTreeMap, time::Duration};
use tokio::io::AsyncReadExt;

pub const ENVOY_IMAGE: &str =
    "envoyproxy/envoy@sha256:57e14a549d7bd43c8d3f6d03e8cfa653e037d4b38e133acd9b54f38c524401b4";
pub const ADMIN_IMAGE: &str =
    "curlimages/curl@sha256:d9b4541e214bcd85196d6e92e2753ac6d0ea699f0af5741f8c6cccbfcf00ef4b";
pub const MAX_PROXY_LEDGER_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AccessEntry {
    method: String,
    code: u16,
    details: String,
    flags: String,
    attempts: u32,
    upstream: Option<String>,
}

impl PlatformClient {
    /// Scheduling barrier only: do not terminate clients while their admitted
    /// writes still await upstream responses. Full ledger/epoch proof follows.
    pub(crate) async fn wait_for_frozen_writes(&self, expected: &ProxyIdentity) -> Result<()> {
        self.inspect_proxy(expected).await?;
        frozen_writes_idle(&counter_snapshot(
            &self.proxy_exec(expected, AdminOperation::Stats).await?,
        )?)
    }
    /// Freeze all proxy admission using Envoy's health-check gate, then read a
    /// complete bounded ledger and stable idle counters through its UNIX socket.
    /// # Errors
    /// Any identity change, restart, incomplete ledger, reset, timeout, retry or
    /// unqualified upstream status fails closed. No timeout establishes success.
    pub(crate) async fn freeze_and_inspect_proxy(
        &self,
        expected: &ProxyIdentity,
    ) -> Result<ProxyDrainObservation> {
        self.freeze_proxy_admission(expected).await?;
        let before = counter_snapshot(&self.proxy_exec(expected, AdminOperation::Stats).await?)?;
        let ledger = self.proxy_exec(expected, AdminOperation::Ledger).await?;
        let after = counter_snapshot(&self.proxy_exec(expected, AdminOperation::Stats).await?)?;
        for name in [
            "server.live",
            "http.apsides.downstream_rq_active",
            "cluster.write_api.upstream_rq_active",
            "cluster.write_api.upstream_rq_total",
        ] {
            if before.get(name) != after.get(name) {
                return Err(invalid(
                    "proxy counters changed during frozen ledger inspection",
                ));
            }
        }
        let (completed, rejected) = validate_ledger(&ledger, &after)?;
        let pod = self.inspect_proxy(expected).await?;
        Ok(ProxyDrainObservation {
            pod_uid: expected.pod_uid.clone(),
            pod_resource_version: pod
                .metadata
                .resource_version
                .ok_or_else(|| invalid("proxy Pod omitted resourceVersion"))?,
            container_id: expected.envoy_container_id.clone(),
            qualification_digest: expected.qualification_digest.clone(),
            ledger_digest: digest(
                &String::from_utf8(ledger).map_err(|_| invalid("proxy ledger is not UTF-8"))?,
            )?,
            completed_requests: completed,
            rejected_after_freeze: rejected,
        })
    }

    pub(crate) async fn freeze_proxy_admission(&self, expected: &ProxyIdentity) -> Result<()> {
        self.inspect_proxy(expected).await?;
        if self.proxy_exec(expected, AdminOperation::Freeze).await? != b"OK\n" {
            return Err(invalid("proxy freeze acknowledgement is invalid"));
        }
        Ok(())
    }

    pub(crate) async fn inspect_proxy(&self, expected: &ProxyIdentity) -> Result<Pod> {
        if ![
            &expected.pod_spec_digest,
            &expected.config_digest,
            &expected.qualification_digest,
        ]
        .iter()
        .all(|s| valid_digest(s))
        {
            return Err(invalid(
                "proxy record lacks approved configuration bindings",
            ));
        }
        self.verify_proxy_node(expected).await?;
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let pod = pods.get(&expected.pod_name).await?;
        if pod.metadata.uid.as_deref() != Some(&expected.pod_uid)
            || pod.metadata.deletion_timestamp.is_some()
            || digest(&pod.spec)? != expected.pod_spec_digest
        {
            return Err(invalid("proxy Pod identity/spec changed"));
        }
        let spec = pod
            .spec
            .as_ref()
            .ok_or_else(|| invalid("proxy Pod lacks spec"))?;
        let status = pod
            .status
            .as_ref()
            .ok_or_else(|| invalid("proxy Pod lacks live status"))?;
        if spec.containers.len() != 2
            || spec.init_containers.as_ref().is_some_and(|v| !v.is_empty())
            || spec
                .ephemeral_containers
                .as_ref()
                .is_some_and(|v| !v.is_empty())
            || status.phase.as_deref() != Some("Running")
        {
            return Err(invalid("proxy Pod has unqualified containers or phase"));
        }
        for (name, image, id) in [
            ("envoy", ENVOY_IMAGE, &expected.envoy_container_id),
            ("admin", ADMIN_IMAGE, &expected.admin_container_id),
        ] {
            let container = spec
                .containers
                .iter()
                .find(|c| c.name == name)
                .ok_or_else(|| invalid("proxy container missing"))?;
            let live = status
                .container_statuses
                .as_ref()
                .and_then(|v| v.iter().find(|c| c.name == name))
                .ok_or_else(|| invalid("proxy container status missing"))?;
            if container.image.as_deref() != Some(image)
                || live.container_id.as_ref() != Some(id)
                || id.is_empty()
                || live.restart_count != 0
                || !live.ready
                || live
                    .state
                    .as_ref()
                    .and_then(|s| s.running.as_ref())
                    .is_none()
            {
                return Err(invalid(
                    "proxy container restarted or differs from qualified image/epoch",
                ));
            }
        }
        let maps: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let config = maps.get(&expected.config_map_name).await?;
        if config.metadata.uid.as_deref() != Some(&expected.config_map_uid)
            || config.metadata.deletion_timestamp.is_some()
            || config.immutable != Some(true)
            || digest(&config.data)? != expected.config_digest
        {
            return Err(invalid("proxy configuration changed or is not immutable"));
        }
        Ok(pod)
    }

    async fn proxy_exec(
        &self,
        expected: &ProxyIdentity,
        operation: AdminOperation,
    ) -> Result<Vec<u8>> {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let mut args = vec![
            "curl",
            "--silent",
            "--show-error",
            "--fail",
            "--max-time",
            "5",
            "--max-filesize",
            "16777216",
        ];
        match operation {
            AdminOperation::Freeze => args.extend([
                "--unix-socket",
                "/evidence/admin.sock",
                "-X",
                "POST",
                "http://localhost/healthcheck/fail",
            ]),
            AdminOperation::Stats => {
                args.extend(["--unix-socket", "/evidence/admin.sock", STATS_URL]);
            }
            AdminOperation::Ledger => args.push("file:///evidence/access.jsonl"),
        }
        let params = AttachParams::default()
            .container("admin")
            .stderr(false)
            .stdin(false)
            .stdout(true)
            .tty(false);
        let work = async {
            let mut attached = pods
                .exec(&expected.pod_name, args, &params)
                .await
                .map_err(|_| invalid("proxy helper execution unavailable"))?;
            let status = attached
                .take_status()
                .ok_or_else(|| invalid("proxy helper status unavailable"))?;
            let mut output = Vec::new();
            attached
                .stdout()
                .ok_or_else(|| invalid("proxy helper stdout unavailable"))?
                .take((MAX_PROXY_LEDGER_BYTES + 1) as u64)
                .read_to_end(&mut output)
                .await
                .map_err(|_| invalid("proxy helper output failed"))?;
            if output.len() > MAX_PROXY_LEDGER_BYTES {
                attached.abort();
                return Err(invalid("proxy ledger exceeds protected evidence budget"));
            }
            let status = status
                .await
                .ok_or_else(|| invalid("proxy helper did not return exit status"))?;
            attached
                .join()
                .await
                .map_err(|_| invalid("proxy helper transport did not complete"))?;
            if status.status.as_deref() != Some("Success") {
                return Err(invalid("proxy helper command failed"));
            }
            Ok(output)
        };
        tokio::time::timeout(Duration::from_secs(10), work)
            .await
            .map_err(|_| invalid("proxy helper execution timed out; request state unknown"))?
    }
}

enum AdminOperation {
    Freeze,
    Stats,
    Ledger,
}

fn counter_snapshot(bytes: &[u8]) -> Result<BTreeMap<String, u64>> {
    let source =
        std::str::from_utf8(bytes).map_err(|_| invalid("invalid proxy statistics encoding"))?;
    let parsed = parse_strict(source)?;
    let values = parsed
        .get("stats")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("proxy statistics absent"))?;
    let mut counters = BTreeMap::new();
    for value in values {
        if let (Some(name), Some(number)) = (
            value.get("name").and_then(Value::as_str),
            value.get("value").and_then(Value::as_u64),
        ) && counters.insert(name.into(), number).is_some()
        {
            return Err(invalid("duplicate proxy statistic"));
        }
    }
    Ok(counters)
}

fn frozen_writes_idle(stats: &BTreeMap<String, u64>) -> Result<()> {
    if stats.get("server.live") != Some(&0) {
        return Err(invalid("proxy admission is not frozen before client stop"));
    }
    match stats.get("cluster.write_api.upstream_rq_active") {
        Some(0) => Ok(()),
        Some(_) => Err(crate::pending(
            "admitted proxy writes are still awaiting upstream responses",
        )),
        None => Err(invalid("proxy write activity counter missing")),
    }
}

fn validate_ledger(bytes: &[u8], stats: &BTreeMap<String, u64>) -> Result<(u64, u64)> {
    for name in [
        "server.live",
        "http.apsides.downstream_rq_active",
        "cluster.write_api.upstream_rq_active",
    ] {
        if stats.get(name) != Some(&0) {
            return Err(invalid("proxy is not frozen and fully idle"));
        }
    }
    let total = stats
        .get("cluster.write_api.upstream_rq_total")
        .ok_or_else(|| invalid("proxy request total absent"))?;
    if bytes.len() > MAX_PROXY_LEDGER_BYTES || (!bytes.is_empty() && !bytes.ends_with(b"\n")) {
        return Err(invalid("proxy ledger truncated or oversized"));
    }
    let source = std::str::from_utf8(bytes).map_err(|_| invalid("proxy ledger is not UTF-8"))?;
    let mut completed = 0;
    let mut rejected = 0;
    for line in source.lines() {
        let entry: AccessEntry = serde_json::from_value(parse_strict(line)?)?;
        if !matches!(entry.method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE") {
            return Err(invalid("proxy ledger contains unsupported request method"));
        }
        if entry.upstream.is_some() {
            if entry.attempts != 1
                || entry.details != "via_upstream"
                || entry.flags != "-"
                || !(matches!(
                    entry.code,
                    200..=299
                        | 400
                        | 401
                        | 403
                        | 404
                        | 405
                        | 406
                        | 409
                        | 410
                        | 413
                        | 415
                        | 422
                        | 429
                ))
            {
                return Err(invalid(
                    "forwarded request has ambiguous or unqualified completion",
                ));
            }
            completed += 1;
        } else if entry.attempts == 0
            && entry.code == 503
            && entry.details == "health_check_failed"
            && entry.flags == "LH"
        {
            rejected += 1;
        } else {
            return Err(invalid(
                "proxy ledger contains an unqualified local outcome",
            ));
        }
    }
    if &completed != total {
        return Err(invalid(
            "proxy ledger does not account for every upstream attempt",
        ));
    }
    Ok((completed, rejected))
}

#[cfg(test)]
mod tests {
    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
    use super::*;
    use serde_json::json;
    #[test]
    fn filtered_live_envoy_statistics_remain_strict_integral_evidence() -> TestResult {
        let captured = br#"{"stats":[{"name":"cluster.write_api.upstream_rq_active","value":0},{"name":"cluster.write_api.upstream_rq_total","value":3},{"name":"http.apsides.downstream_rq_active","value":0},{"name":"server.live","value":0}]}"#;
        let snapshot = super::counter_snapshot(captured)?;
        assert_eq!(snapshot.len(), 4);
        assert_eq!(snapshot["cluster.write_api.upstream_rq_total"], 3);
        assert!(
            super::counter_snapshot(br#"{"stats":[{"name":"server.live","value":0.0}]}"#).is_err()
        );
        assert!(
            super::counter_snapshot(br#"{"stats":[{"name":"server.live","value":0,"value":1}]}"#)
                .is_err()
        );
        assert!(
            super::counter_snapshot(
                br#"{"stats":[{"name":"server.live","value":0},{"name":"server.live","value":1}]}"#
            )
            .is_err()
        );
        Ok(())
    }
    fn stats() -> BTreeMap<String, u64> {
        [
            ("server.live".into(), 0),
            ("http.apsides.downstream_rq_active".into(), 0),
            ("cluster.write_api.upstream_rq_active".into(), 0),
            ("cluster.write_api.upstream_rq_total".into(), 1),
        ]
        .into()
    }
    fn entry() -> Value {
        json!({"method":"POST","code":200,"details":"via_upstream","flags":"-","attempts":1,"upstream":"api:443"})
    }
    fn line(entry: &Value) -> Vec<u8> {
        format!("{entry}\n").into_bytes()
    }
    #[test]
    fn idle_does_not_hide_timeouts_resets_retries_or_missing_records() -> TestResult {
        assert_eq!(validate_ledger(&line(&entry()), &stats())?, (1, 0));
        for (field, value) in [
            ("code", json!(504)),
            ("flags", json!("DC")),
            ("attempts", json!(2)),
            ("details", json!("upstream_reset_before_response_started")),
        ] {
            let mut bad = entry();
            bad[field] = value;
            assert!(validate_ledger(&line(&bad), &stats()).is_err());
        }
        assert!(validate_ledger(b"", &stats()).is_err());
        let mut incomplete = line(&entry());
        incomplete.pop();
        assert!(validate_ledger(&incomplete, &stats()).is_err());
        let mut running = stats();
        running.insert("server.live".into(), 1);
        assert!(validate_ledger(&line(&entry()), &running).is_err());
        Ok(())
    }

    #[test]
    fn admitted_write_delays_client_stop_without_establishing_drain_proof() {
        let mut values = stats();
        values.insert("cluster.write_api.upstream_rq_active".into(), 1);
        assert!(matches!(
            frozen_writes_idle(&values),
            Err(crate::Error::Pending(_))
        ));
        values.insert("cluster.write_api.upstream_rq_active".into(), 0);
        assert!(frozen_writes_idle(&values).is_ok());
        // A scheduling pass never replaces complete request accounting.
        assert!(validate_ledger(b"", &values).is_err());
        let mut reset = entry();
        reset["flags"] = json!("DC");
        assert!(validate_ledger(&line(&reset), &values).is_err());
        values.insert("server.live".into(), 1);
        assert!(matches!(
            frozen_writes_idle(&values),
            Err(crate::Error::Invalid(_))
        ));
    }
}
