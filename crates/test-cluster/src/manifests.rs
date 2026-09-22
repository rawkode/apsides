//! Every object the fixture driver creates, rendered without a cluster.

use crate::{
    Result,
    contract::{FixtureRelease, ROLLOUT_KEY, ROLLOUT_KEY_ID},
    input,
};
use base64::Engine as _;
use compiler_core::{CompiledPlan, InputMode};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// Name of the controller's `ServiceAccount`, `Role`s, `RoleBinding`s and
/// `Deployment`.
pub const CONTROLLER: &str = "apsides-fixture-controller";
/// A namespace the controller has no rights in: the out-of-scope target and
/// home of the egress sentinel.
pub const OUTSIDE: &str = "apsides-outside";
pub const SENTINEL: &str = "egress-sentinel";
pub const SENTINEL_PREFLIGHT: &str = "sentinel-preflight";
const RUNTIME_SECRET: &str = "apsides-runtime-fixture";
const PROXY_SCRIPT: &str = "apsides-loopback-proxy";
/// The field manager the fixture driver writes as.
pub const DRIVER_MANAGER: &str = "aps-test-fixture-driver";
/// A competing field manager for conflict cases.
pub const FOREIGN_MANAGER: &str = "fixture-foreign-manager";
/// Where the controller reaches the API server: its loopback proxy sidecar.
const LOOPBACK_SERVER: &str = "https://127.0.0.1:8443";

/// Checks the loopback proxy runs before it starts forwarding: the sentinel in
/// another namespace and an external address must both be unreachable.
const PROXY_SH: &str = r#"#!/bin/sh
if nc -z -w 2 "$DENIED_SENTINEL_HOST" 8080; then
  echo APSIDES_NETWORK_POLICY_CANARY_UNEXPECTEDLY_CONNECTED
  exit 41
fi
echo APSIDES_NETWORK_POLICY_CANARY_DENIED
if nc -z -w 2 1.1.1.1 443; then
  echo APSIDES_EGRESS_CANARY_UNEXPECTEDLY_CONNECTED
  exit 42
fi
echo APSIDES_EGRESS_CANARY_DENIED
cat >/tmp/apsides-nginx.conf <<EOF
pid /tmp/apsides-nginx.pid;
error_log stderr notice;
events {}
stream {
  server {
    listen 127.0.0.1:8443;
    proxy_pass ${KUBERNETES_SERVICE_HOST}:${KUBERNETES_SERVICE_PORT};
  }
}
EOF
exec nginx -c /tmp/apsides-nginx.conf -g 'daemon off;'
"#;

/// Holds the controller Pod until egress is denied.
const PREFLIGHT_SH: &str = r#"until ! nc -z -w 2 "$DENIED_SENTINEL_HOST" 8080 && ! nc -z -w 2 1.1.1.1 443; do
  echo APSIDES_INIT_EGRESS_NOT_YET_DENIED
  sleep 1
done
echo APSIDES_INIT_EGRESS_DENIED
"#;

pub fn namespace(name: &str) -> Value {
    json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": name}})
}

pub fn service_account(namespace: &str, name: &str) -> Value {
    json!({"apiVersion": "v1", "kind": "ServiceAccount",
        "metadata": {"name": name, "namespace": namespace}})
}

/// The controller's generated capabilities as RBAC rules, one rule per
/// capability, grouped by namespace. A capability outside the domain's two
/// namespaces is refused.
pub fn capability_rules(plan: &CompiledPlan) -> Result<BTreeMap<String, Vec<Value>>> {
    let domain = &plan.ir.domain;
    let mut rules: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for capability in &plan.capabilities {
        if capability.namespace != domain.namespace
            && capability.namespace != domain.control_namespace
        {
            return input(format!(
                "capability in {} escapes the fixture namespaces",
                capability.namespace
            ));
        }
        let resource = capability.subresource.as_ref().map_or_else(
            || capability.resource.clone(),
            |sub| format!("{}/{sub}", capability.resource),
        );
        let mut rule = json!({"apiGroups": [capability.api_group], "resources": [resource],
            "verbs": [capability.verb]});
        if !capability.names.is_empty() {
            rule["resourceNames"] = json!(capability.names);
        }
        rules
            .entry(capability.namespace.clone())
            .or_default()
            .push(rule);
    }
    Ok(rules)
}

pub fn role(namespace: &str, rules: &[Value]) -> Value {
    json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role",
        "metadata": {"name": CONTROLLER, "namespace": namespace}, "rules": rules})
}

pub fn role_binding(namespace: &str, control: &str) -> Value {
    json!({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding",
        "metadata": {"name": CONTROLLER, "namespace": namespace},
        "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": CONTROLLER},
        "subjects": [{"kind": "ServiceAccount", "name": CONTROLLER, "namespace": control}]})
}

/// `ServiceAccount`s the workloads name, which the controller does not create.
pub fn workload_service_accounts(plan: &CompiledPlan) -> Vec<Value> {
    plan.ir
        .resources
        .iter()
        .filter(|resource| resource.kind == "Deployment")
        .filter_map(|resource| {
            let name = resource.desired["spec"]["template"]["spec"]["serviceAccountName"]
                .as_str()
                .unwrap_or("default");
            (name != "default").then(|| service_account(&resource.namespace, name))
        })
        .collect()
}

/// The generated fixture for every observed input, as the driver seeds it.
///
/// # Errors
/// Reports `FixtureRequired` when an observed input has no usable fixture.
pub fn seed_inputs(release: &FixtureRelease) -> Result<Vec<Value>> {
    let mut objects = Vec::new();
    for item in release
        .plan
        .ir
        .inputs
        .iter()
        .filter(|item| item.mode == InputMode::Observed)
    {
        let Some(fixture) = release.fixture(&item.id) else {
            return input(format!(
                "FixtureRequired: no generated fixture for {}",
                item.id
            ));
        };
        if fixture["state"] != "present" {
            return input(format!(
                "FixtureRequired: initial live input {} is {}",
                item.id, fixture["state"]
            ));
        }
        let data = fixture["data"].as_object().cloned().unwrap_or_default();
        if data.is_empty() {
            return input(format!(
                "FixtureRequired: empty generated fixture for {}",
                item.id
            ));
        }
        let data = data
            .into_iter()
            .map(|(key, value)| (key, value.as_str().unwrap_or_default().to_owned()))
            .collect();
        objects.push(input_object(&item.kind, &item.name, &item.namespace, &data));
    }
    Ok(objects)
}

/// A `ConfigMap` or `Secret` with `data`, given as plain strings.
pub fn input_object(
    kind: &str,
    name: &str,
    namespace: &str,
    data: &BTreeMap<String, String>,
) -> Value {
    let data = data
        .iter()
        .map(|(key, value)| (key.clone(), Value::String(encode(kind, value))))
        .collect::<Map<_, _>>();
    json!({"apiVersion": "v1", "kind": kind,
        "metadata": {"name": name, "namespace": namespace}, "data": data})
}

/// How `value` is stored in a `kind`'s `data` field.
pub fn encode(kind: &str, value: &str) -> String {
    if kind == "Secret" {
        base64::engine::general_purpose::STANDARD.encode(value)
    } else {
        value.to_owned()
    }
}

/// The fixture-mode startup document the controller reads.
pub fn runtime_config(plan: &CompiledPlan, context: &str) -> Value {
    json!({
        "mode": "fixture",
        "acknowledgement": runtime::FIXTURE_ACKNOWLEDGEMENT,
        "irDigest": plan.ir_digest,
        "clusterBinding": plan.ir.domain.cluster_binding,
        "kubeconfig": "/run/apsides/kubeconfig",
        "context": context,
        "pollIntervalMillis": 250,
        "maxWatchCycles": 1_000_000,
        "rolloutKey": {"keyId": ROLLOUT_KEY_ID, "value": ROLLOUT_KEY},
    })
}

/// The controller's kubeconfig: its short-lived token, reaching the API server
/// through the loopback proxy under the API server's own TLS name.
pub fn pod_kubeconfig(authority: &str, context: &str, token: &str) -> Value {
    crate::cluster::token_kubeconfig(
        LOOPBACK_SERVER,
        authority,
        context,
        token,
        Some("kubernetes.default.svc"),
    )
}

pub fn runtime_secret(control: &str, config: &Value, kubeconfig: &Value) -> Value {
    let data = [("runtime.json", config), ("kubeconfig", kubeconfig)]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), json!(encode("Secret", &value.to_string()))))
        .collect::<Map<_, _>>();
    json!({"apiVersion": "v1", "kind": "Secret",
        "metadata": {"name": RUNTIME_SECRET, "namespace": control}, "data": data})
}

/// A server in the outside namespace that a correctly isolated Pod can never
/// reach.
pub fn sentinel(image: &str) -> [Value; 2] {
    let labels = json!({"app": SENTINEL});
    [
        json!({"apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": SENTINEL, "namespace": OUTSIDE},
            "spec": {"replicas": 1, "selector": {"matchLabels": labels},
                "template": {"metadata": {"labels": labels},
                    "spec": {"containers": [{"name": "server", "image": image,
                        "imagePullPolicy": "Never"}]}}}}),
        json!({"apiVersion": "v1", "kind": "Service",
            "metadata": {"name": SENTINEL, "namespace": OUTSIDE},
            "spec": {"selector": labels, "ports": [{"port": 8080, "targetPort": 8080}]}}),
    ]
}

/// A Pod beside the sentinel that proves it is reachable at all, so a denied
/// connection later means the policy denied it.
pub fn sentinel_preflight(image: &str, sentinel_ip: &str) -> Value {
    json!({"apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": SENTINEL_PREFLIGHT, "namespace": OUTSIDE},
        "spec": {"restartPolicy": "Never", "containers": [{"name": "probe", "image": image,
            "imagePullPolicy": "Never", "command": ["sh", "-c", format!("nc -z -w 5 {sentinel_ip} 8080")]}]}})
}

pub fn deny_all_egress(namespace: &str) -> Value {
    json!({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
        "metadata": {"name": "deny-all-egress", "namespace": namespace},
        "spec": {"podSelector": {}, "policyTypes": ["Egress"]}})
}

/// The controller's single egress exception: the API server's Service
/// address. `enabled: false` withdraws it to simulate an unavailable API.
pub fn controller_api_egress(control: &str, api_cidr: &str, enabled: bool) -> Value {
    let egress = if enabled {
        json!([{"to": [{"ipBlock": {"cidr": api_cidr}}], "ports": [{"protocol": "TCP", "port": 443}]}])
    } else {
        json!([])
    };
    json!({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy",
        "metadata": {"name": "controller-api-only", "namespace": control},
        "spec": {"podSelector": {"matchLabels": {"app": CONTROLLER}},
            "policyTypes": ["Egress"], "egress": egress}})
}

pub fn proxy_script(control: &str) -> Value {
    json!({"apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {"name": PROXY_SCRIPT, "namespace": control}, "data": {"proxy.sh": PROXY_SH}})
}

/// The release's controller in fixture mode, with no automounted token, an
/// init container that waits for egress to be denied, and the loopback proxy
/// sidecar.
pub fn controller_deployment(
    control: &str,
    controller_image: &str,
    proxy_image: &str,
    sentinel_ip: &str,
) -> Value {
    let labels = json!({"app": CONTROLLER});
    let sentinel = json!([{"name": "DENIED_SENTINEL_HOST", "value": sentinel_ip}]);
    json!({"apiVersion": "apps/v1", "kind": "Deployment",
    "metadata": {"name": CONTROLLER, "namespace": control},
    "spec": {"replicas": 1, "strategy": {"type": "Recreate"},
        "selector": {"matchLabels": labels},
        "template": {"metadata": {"labels": labels},
            "spec": {"serviceAccountName": CONTROLLER, "automountServiceAccountToken": false,
                "terminationGracePeriodSeconds": 30,
                "initContainers": [{"name": "egress-preflight", "image": proxy_image,
                    "imagePullPolicy": "Never", "command": ["sh", "-c", PREFLIGHT_SH],
                    "env": sentinel}],
                "containers": [
                    {"name": "controller", "image": controller_image, "imagePullPolicy": "Never",
                     "env": [{"name": runtime::CONFIG_PATH_ENV, "value": "/run/apsides/runtime.json"}],
                     "volumeMounts": [{"name": "runtime", "mountPath": "/run/apsides", "readOnly": true}]},
                    {"name": "loopback-proxy", "image": proxy_image, "imagePullPolicy": "Never",
                     "command": ["sh", "/proxy/proxy.sh"], "env": sentinel,
                     "volumeMounts": [{"name": "proxy", "mountPath": "/proxy", "readOnly": true}]},
                ],
                "volumes": [
                    {"name": "runtime", "secret": {"secretName": RUNTIME_SECRET, "defaultMode": 0o444}},
                    {"name": "proxy", "configMap": {"name": PROXY_SCRIPT, "defaultMode": 0o444}},
                ]}}}})
}

pub fn plural(kind: &str) -> &'static str {
    if kind == "Secret" {
        "secrets"
    } else {
        "configmaps"
    }
}

/// `rules` with every grant naming the input `name` of `kind` removed, so the
/// controller can no longer read it. Rules without resource names are kept.
pub fn without_input(rules: &[Value], kind: &str, name: &str) -> Vec<Value> {
    let plural = plural(kind);
    rules
        .iter()
        .filter_map(|rule| {
            let names = rule["resourceNames"].as_array();
            let covers = mentions(rule, plural)
                && names.is_some_and(|names| names.iter().any(|item| item == name));
            if !covers {
                return Some(rule.clone());
            }
            let remaining = names
                .into_iter()
                .flatten()
                .filter(|item| *item != name)
                .cloned()
                .collect::<Vec<_>>();
            if remaining.is_empty() {
                return None;
            }
            let mut rule = rule.clone();
            rule["resourceNames"] = json!(remaining);
            Some(rule)
        })
        .collect()
}

/// `rules` without any grant to create objects of `kind`, so the controller
/// cannot repair a deleted managed input.
pub fn without_create(rules: &[Value], kind: &str) -> Vec<Value> {
    let plural = plural(kind);
    rules
        .iter()
        .filter(|rule| !(mentions(rule, plural) && has_verb(rule, "create")))
        .cloned()
        .collect()
}

/// Whether `rules` still let the controller `get` the input `name` of `kind`.
pub fn grants_read(rules: &[Value], kind: &str, name: &str) -> bool {
    let plural = plural(kind);
    rules.iter().any(|rule| {
        mentions(rule, plural)
            && has_verb(rule, "get")
            && rule["resourceNames"]
                .as_array()
                .is_none_or(|names| names.is_empty() || names.iter().any(|item| item == name))
    })
}

fn mentions(rule: &Value, plural: &str) -> bool {
    rule["resources"]
        .as_array()
        .is_some_and(|resources| resources.iter().any(|item| item == plural))
}

fn has_verb(rule: &Value, verb: &str) -> bool {
    rule["verbs"]
        .as_array()
        .is_some_and(|verbs| verbs.iter().any(|item| item == verb))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    #[test]
    fn capabilities_become_per_namespace_rules() -> Result<()> {
        let plan = testing::plan()?;
        let rules = capability_rules(&plan)?;
        assert!(
            rules
                .keys()
                .all(|namespace| namespace == "work" || namespace == "control")
        );
        let work = rules.get("work").map_or(&[][..], Vec::as_slice);
        assert_eq!(
            work.len(),
            plan.capabilities
                .iter()
                .filter(|capability| capability.namespace == "work")
                .count()
        );
        assert!(grants_read(work, "Secret", "credentials"));
        Ok(())
    }

    #[test]
    fn denying_an_input_removes_only_that_name() {
        let rules = vec![
            json!({"apiGroups": [""], "resources": ["secrets"], "verbs": ["get"],
                "resourceNames": ["credentials", "other"]}),
            json!({"apiGroups": [""], "resources": ["secrets"], "verbs": ["watch"],
                "resourceNames": ["credentials"]}),
            json!({"apiGroups": [""], "resources": ["configmaps"], "verbs": ["create"]}),
        ];
        let denied = without_input(&rules, "Secret", "credentials");
        assert_eq!(denied.len(), 2);
        assert_eq!(denied[0]["resourceNames"], json!(["other"]));
        assert!(!grants_read(&denied, "Secret", "credentials"));
        assert!(grants_read(&denied, "Secret", "other"));
        assert_eq!(without_create(&rules, "ConfigMap").len(), 2);
    }

    #[test]
    fn seeded_secrets_are_base64_and_configmaps_are_plain() {
        let data = BTreeMap::from([("value".to_owned(), "b".to_owned())]);
        assert_eq!(
            input_object("Secret", "s", "n", &data)["data"]["value"],
            "Yg=="
        );
        assert_eq!(
            input_object("ConfigMap", "c", "n", &data)["data"]["value"],
            "b"
        );
    }

    #[test]
    fn the_controller_runs_isolated_with_its_own_credential() {
        let deployment = controller_deployment(
            "control",
            "controller@sha256:1",
            "proxy@sha256:2",
            "10.0.0.9",
        );
        let spec = &deployment["spec"]["template"]["spec"];
        assert_eq!(spec["automountServiceAccountToken"], false);
        assert_eq!(spec["serviceAccountName"], CONTROLLER);
        for container in spec["containers"].as_array().into_iter().flatten() {
            assert_eq!(container["imagePullPolicy"], "Never");
        }
        assert_eq!(spec["initContainers"][0]["env"][0]["value"], "10.0.0.9");
        assert_eq!(deny_all_egress("work")["spec"]["podSelector"], json!({}));
        let open = controller_api_egress("control", "10.96.0.1/32", true);
        assert_eq!(
            open["spec"]["egress"][0]["to"][0]["ipBlock"]["cidr"],
            "10.96.0.1/32"
        );
        let closed = controller_api_egress("control", "10.96.0.1/32", false);
        assert_eq!(closed["spec"]["egress"], json!([]));
    }

    #[test]
    fn the_runtime_config_is_fixture_mode_with_the_synthetic_key() -> Result<()> {
        let plan = testing::plan()?;
        let mut config = runtime_config(&plan, "kind-aps-test-1");
        assert_eq!(config["mode"], "fixture");
        // The runtime's own fixture schema accepts everything but the mode tag.
        if let Some(fields) = config.as_object_mut() {
            let mode = fields.remove("mode");
            let _: runtime::FixtureConfig = serde_json::from_value(Value::Object(fields.clone()))?;
            fields.insert("mode".into(), mode.unwrap_or_default());
        }
        assert_eq!(config["irDigest"], plan.ir_digest);
        assert_eq!(config["rolloutKey"]["keyId"], ROLLOUT_KEY_ID);
        let kubeconfig = pod_kubeconfig("Q0E=", "kind-aps-test-1", "token");
        assert_eq!(
            kubeconfig["clusters"][0]["cluster"]["server"],
            LOOPBACK_SERVER
        );
        let secret = runtime_secret("control", &config, &kubeconfig);
        assert!(secret["data"]["runtime.json"].is_string());
        assert_eq!(workload_service_accounts(&plan).len(), 1);
        Ok(())
    }
}
