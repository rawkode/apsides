//! Pure, deterministic planning for a single `events.k8s.io/v1` Event.
//!
//! Timestamps are parsed as RFC3339 calendar times and normalized to UTC when
//! materialized. This crate deliberately has no Kubernetes client dependency.

use std::{collections::BTreeMap, fmt::Write as _};

use oxideloop_renderer_abi::{
    ActionInputV0, ActionOutputV0, Validated, canonical_bytes, sha256_hex,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub const EVENT_API_VERSION: &str = "events.k8s.io/v1";
pub const EVENT_KIND: &str = "Event";
pub const REPORTING_CONTROLLER: &str = "runtime.oxideloop.dev/event-planner";
pub const ROOT_UID_LABEL: &str = "runtime.oxideloop.dev/root-uid";
pub const OCCURRENCE_LABEL: &str = "runtime.oxideloop.dev/occurrence-id";
pub const LOGICAL_ID_LABEL: &str = "runtime.oxideloop.dev/action-logical-id";
pub const SEMANTIC_DIGEST_ANNOTATION: &str = "runtime.oxideloop.dev/semantic-digest";
const DOMAIN: &[u8] = b"oxideloop:event-planner:v0\0";
const MAX_FIELD: usize = 253;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObjectIdentity {
    pub api_version: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObjectRef {
    pub identity: ObjectIdentity,
    pub uid: String,
    pub resource_version: String,
}

/// Immutable indexed fields from `AutomationRunSpec`; a wire-independent
/// mirror that keeps the planner free of Kubernetes and controller crates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunCorrelationV0 {
    pub occurrence_id: String,
    pub root: ObjectRef,
    pub run: ObjectRef,
    pub source: ObjectIdentity,
    pub source_uid: String,
    pub source_resource_version: String,
    pub source_transition_id: String,
    pub automation_identity: String,
    pub automation_version: String,
    pub bundle_digest: String,
    pub action_module_identity: String,
    pub action_module_digest: String,
    pub renderer_abi_version: String,
    pub action_input_hash: String,
    pub event_metadata_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventMetadata {
    pub name: String,
    pub namespace: String,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    pub owner_references: Vec<OwnerReference>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OwnerReference {
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub uid: String,
    pub controller: bool,
    pub block_owner_deletion: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventRegarding {
    pub api_version: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub uid: String,
    pub resource_version: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlannedEventV0 {
    pub api_version: String,
    pub kind: String,
    pub metadata: EventMetadata,
    pub action: String,
    pub reason: String,
    pub note: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub reporting_controller: String,
    pub reporting_instance: String,
    pub regarding: EventRegarding,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PlannerError {
    #[error("{0}")]
    Invalid(String),
}
fn bad(s: impl Into<String>) -> PlannerError {
    PlannerError::Invalid(s.into())
}
fn bound(v: &str, field: &str, max: usize) -> Result<(), PlannerError> {
    if v.is_empty() || v.len() > max {
        Err(bad(format!("{field} must be 1..={max} UTF-8 bytes")))
    } else {
        Ok(())
    }
}
fn dns(v: &str, field: &str) -> Result<(), PlannerError> {
    bound(v, field, 63)?;
    if !v
        .bytes()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        || v.starts_with('-')
        || v.ends_with('-')
    {
        return Err(bad(format!("{field} must be DNS-1123")));
    }
    Ok(())
}
fn token(v: &str, field: &str, max: usize) -> Result<(), PlannerError> {
    bound(v, field, max)?;
    if !v
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || b"./_-".contains(&c))
    {
        return Err(bad(format!("{field} has unsupported syntax")));
    }
    Ok(())
}
fn object_name(v: &str, field: &str) -> Result<(), PlannerError> {
    bound(v, field, 253)?;
    if v.split('.').any(|label| dns(label, field).is_err()) {
        return Err(bad(format!("{field} must be a DNS-1123 subdomain")));
    }
    Ok(())
}
fn identity(x: &ObjectIdentity) -> Result<(), PlannerError> {
    token(&x.api_version, "apiVersion", 253)?;
    token(&x.kind, "kind", 253)?;
    dns(&x.namespace, "namespace")?;
    object_name(&x.name, "name")
}
fn reference(x: &ObjectRef) -> Result<(), PlannerError> {
    identity(&x.identity)?;
    bound(&x.uid, "uid", MAX_FIELD)?;
    bound(&x.resource_version, "resourceVersion", MAX_FIELD)
}
fn hex(bytes: &[u8]) -> String {
    let mut o = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(&mut o, "{b:02x}").expect("String");
    }
    o
}
fn hashed(tag: &str, value: &str) -> String {
    let mut h = Sha256::new();
    h.update(DOMAIN);
    h.update(tag.as_bytes());
    h.update([0]);
    let length = u64::try_from(value.len()).expect("usize always fits in u64 on supported targets");
    h.update(length.to_be_bytes());
    h.update(value.as_bytes());
    hex(&h.finalize())
}
fn name(occurrence: &str, logical: &str) -> String {
    format!(
        "oxideloop-event-{}",
        &hashed("name", &format!("{occurrence}\0{logical}"))[..40]
    )
}
fn run_name(root_uid: &str, occurrence: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"oxideloop-automation-run/v1\0");
    h.update(root_uid.as_bytes());
    h.update(b"\0");
    h.update(occurrence.as_bytes());
    format!("ol-run-{}", &hex(&h.finalize())[..56])
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AuthoredEvent {
    api_version: String,
    kind: String,
    metadata: AuthoredMetadata,
    regarding: AuthoredRegarding,
    action: String,
    reason: String,
    note: String,
    #[serde(rename = "type")]
    event_type: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AuthoredMetadata {
    namespace: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AuthoredRegarding {
    api_version: String,
    kind: String,
    namespace: String,
    name: String,
}
fn authored(v: &Value) -> Result<AuthoredEvent, PlannerError> {
    let e: AuthoredEvent = serde_json::from_value(v.clone())
        .map_err(|x| bad(format!("malformed authored Event: {x}")))?;
    if e.api_version != EVENT_API_VERSION || e.kind != EVENT_KIND {
        return Err(bad("Event apiVersion/kind must be events.k8s.io/v1/Event"));
    }
    dns(&e.metadata.namespace, "metadata.namespace")?;
    token(&e.regarding.api_version, "regarding.apiVersion", 253)?;
    token(&e.regarding.kind, "regarding.kind", 253)?;
    dns(&e.regarding.namespace, "regarding.namespace")?;
    object_name(&e.regarding.name, "regarding.name")?;
    token(&e.action, "action", 128)?;
    token(&e.reason, "reason", 128)?;
    bound(&e.note, "note", 1024)?;
    if !matches!(e.event_type.as_str(), "Normal" | "Warning") {
        return Err(bad("type must be Normal or Warning"));
    }
    Ok(e)
}

fn check_input(
    input: &Validated<ActionInputV0>,
    corr: &RunCorrelationV0,
) -> Result<(), PlannerError> {
    let i = &input.document;
    reference(&corr.root)?;
    reference(&corr.run)?;
    identity(&corr.source)?;
    for (field, value, maximum) in [
        ("occurrenceId", corr.occurrence_id.as_str(), 253),
        ("sourceUid", corr.source_uid.as_str(), 253),
        (
            "sourceResourceVersion",
            corr.source_resource_version.as_str(),
            253,
        ),
        (
            "sourceTransitionId",
            corr.source_transition_id.as_str(),
            253,
        ),
        ("automationIdentity", corr.automation_identity.as_str(), 253),
        ("automationVersion", corr.automation_version.as_str(), 128),
        (
            "actionModuleIdentity",
            corr.action_module_identity.as_str(),
            128,
        ),
        (
            "rendererAbiVersion",
            corr.renderer_abi_version.as_str(),
            128,
        ),
    ] {
        bound(value, field, maximum)?;
    }
    for (field, value) in [
        ("bundleDigest", &corr.bundle_digest),
        ("actionModuleDigest", &corr.action_module_digest),
        ("actionInputHash", &corr.action_input_hash),
        ("eventMetadataHash", &corr.event_metadata_hash),
    ] {
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(bad(format!("{field} must be lowercase SHA-256")));
        }
    }
    let meta = sha256_hex(&canonical_bytes(&i.event_metadata).map_err(|e| bad(e.to_string()))?);
    if input.sha256 != corr.action_input_hash
        || meta != corr.event_metadata_hash
        || i.abi_version != corr.renderer_abi_version
        || i.bundle_digest != corr.bundle_digest
        || i.automation_identity != corr.automation_identity
        || i.automation_version != corr.automation_version
        || i.action_module_identity != corr.action_module_identity
        || i.action_module_digest != corr.action_module_digest
        || i.occurrence_id != corr.occurrence_id
        || i.source_transition_id != corr.source_transition_id
        || i.source_uid != corr.source_uid
        || i.source_resource_version != corr.source_resource_version
    {
        return Err(bad(
            "ActionInputV0 does not match immutable run correlation",
        ));
    }
    if let Some(context) = oxideloop_renderer_abi::action_context_v1(&i.event_metadata)
        .map_err(|error| bad(error.to_string()))?
        && !oxideloop_renderer_abi::action_context_source_matches(
            &context,
            &corr.source.api_version,
            &corr.source.kind,
            &corr.source.namespace,
            &corr.source.name,
            &corr.source_uid,
            &corr.source_resource_version,
        )
    {
        return Err(bad(
            "v1 action-context source does not match immutable run correlation",
        ));
    }
    Ok(())
}

/// Plan from exact, separate root/source/run references. The source is never
/// inferred from the root, even if both happen to share an identity.
///
/// # Errors
///
/// Returns an error if a replay pin, object identity, action shape, or Event
/// contract requirement is violated.
#[allow(clippy::too_many_lines)] // Contract validation is deliberately co-located for auditability.
pub fn plan_event(
    input: &Validated<ActionInputV0>,
    output: &Validated<ActionOutputV0>,
    root: &ObjectRef,
    source: &ObjectRef,
    run: &ObjectRef,
    corr: &RunCorrelationV0,
) -> Result<PlannedEventV0, PlannerError> {
    reference(root)?;
    reference(source)?;
    reference(run)?;
    check_input(input, corr)?;
    if root != &corr.root
        || run != &corr.run
        || source.identity != corr.source
        || source.uid != corr.source_uid
        || source.resource_version != corr.source_resource_version
    {
        return Err(bad("root/source reference does not match run correlation"));
    }
    if root.identity.api_version != "demo.oxideloop.dev/v1alpha1"
        || root.identity.kind != "WebApplication"
    {
        return Err(bad("root must be an exact WebApplication"));
    }
    if run.identity.api_version != "runtime.oxideloop.dev/v1alpha1"
        || run.identity.kind != "AutomationRun"
    {
        return Err(bad("run must be an exact AutomationRun"));
    }
    if run.identity.name != run_name(&root.uid, &corr.occurrence_id) {
        return Err(bad("AutomationRun name is not deterministic"));
    }
    if root.identity.namespace != source.identity.namespace
        || source.identity.namespace != run.identity.namespace
    {
        return Err(bad("root, source, and run namespaces must match"));
    }
    if output.document.actions.len() != 1 {
        return Err(bad("ActionOutputV0 must contain exactly one action"));
    }
    let a = &output.document.actions[0];
    if a.kind != EVENT_KIND {
        return Err(bad("action kind must be Event"));
    }
    token(&a.logical_id, "logicalId", 253)?;
    let e = authored(&a.specification)?;
    if e.metadata.namespace != source.identity.namespace
        || e.regarding.api_version != source.identity.api_version
        || e.regarding.kind != source.identity.kind
        || e.regarding.namespace != source.identity.namespace
        || e.regarding.name != source.identity.name
    {
        return Err(bad(
            "authored regarding must match the exact source identity",
        ));
    }
    let mut labels = BTreeMap::new();
    labels.insert(
        ROOT_UID_LABEL.into(),
        hashed("root-uid", &root.uid)[..63].into(),
    );
    labels.insert(
        OCCURRENCE_LABEL.into(),
        hashed("occurrence", &corr.occurrence_id)[..63].into(),
    );
    labels.insert(
        LOGICAL_ID_LABEL.into(),
        hashed("logical-id", &a.logical_id)[..63].into(),
    );
    let mut annotations = BTreeMap::new();
    annotations.insert("runtime.oxideloop.dev/root-uid".into(), root.uid.clone());
    annotations.insert(
        "runtime.oxideloop.dev/occurrence-id".into(),
        corr.occurrence_id.clone(),
    );
    annotations.insert(
        "runtime.oxideloop.dev/action-logical-id".into(),
        a.logical_id.clone(),
    );
    let mut plan = PlannedEventV0 {
        api_version: EVENT_API_VERSION.into(),
        kind: EVENT_KIND.into(),
        metadata: EventMetadata {
            name: name(&corr.occurrence_id, &a.logical_id),
            namespace: source.identity.namespace.clone(),
            labels,
            annotations,
            owner_references: vec![OwnerReference {
                api_version: run.identity.api_version.clone(),
                kind: run.identity.kind.clone(),
                name: run.identity.name.clone(),
                uid: run.uid.clone(),
                controller: true,
                block_owner_deletion: false,
            }],
        },
        action: e.action,
        reason: e.reason,
        note: e.note,
        event_type: e.event_type,
        reporting_controller: REPORTING_CONTROLLER.into(),
        reporting_instance: format!("root-{}", &hashed("reporting-root", &root.uid)[..32]),
        regarding: EventRegarding {
            api_version: source.identity.api_version.clone(),
            kind: source.identity.kind.clone(),
            namespace: source.identity.namespace.clone(),
            name: source.identity.name.clone(),
            uid: source.uid.clone(),
            resource_version: source.resource_version.clone(),
        },
    };
    let digest = semantic_digest(&plan)?;
    plan.metadata
        .annotations
        .insert(SEMANTIC_DIGEST_ANNOTATION.into(), digest);
    Ok(plan)
}
fn semantic_digest(plan: &PlannedEventV0) -> Result<String, PlannerError> {
    let mut p = plan.clone();
    p.metadata.annotations.remove(SEMANTIC_DIGEST_ANNOTATION);
    let value = serde_json::to_value(p).map_err(|e| bad(e.to_string()))?;
    canonical_bytes(&value)
        .map(|b| sha256_hex(&b))
        .map_err(|e| bad(e.to_string()))
}
/// Materialize a plan with a normalized UTC RFC3339 timestamp.
///
/// # Errors
///
/// Returns an error for impossible calendar times or malformed RFC3339 input.
pub fn materialize_event(plan: &PlannedEventV0, event_time: &str) -> Result<Value, PlannerError> {
    let time = OffsetDateTime::parse(event_time, &Rfc3339)
        .map_err(|_| bad("eventTime must be valid RFC3339"))?;
    let normalized = time
        .to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .map_err(|_| bad("eventTime cannot be normalized"))?;
    let mut v = serde_json::to_value(plan).map_err(|e| bad(e.to_string()))?;
    v.as_object_mut()
        .ok_or_else(|| bad("plan serialization"))?
        .insert("eventTime".into(), Value::String(normalized));
    Ok(v)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecoveredEvent {
    api_version: String,
    kind: String,
    metadata: RecoveredMetadata,
    action: String,
    reason: String,
    note: String,
    #[serde(rename = "type")]
    event_type: String,
    reporting_controller: String,
    reporting_instance: String,
    regarding: EventRegarding,
    event_time: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecoveredMetadata {
    name: String,
    namespace: String,
    labels: BTreeMap<String, String>,
    annotations: BTreeMap<String, String>,
    owner_references: Vec<OwnerReference>,
    uid: Option<String>,
    resource_version: Option<String>,
    generation: Option<Value>,
    creation_timestamp: Option<String>,
    managed_fields: Option<Value>,
}
fn decode(v: &Value) -> Result<PlannedEventV0, PlannerError> {
    let e: RecoveredEvent = serde_json::from_value(v.clone())
        .map_err(|x| bad(format!("malformed recovered Event: {x}")))?;
    // These are the *only* server-owned fields the recovery comparator drops.
    let _ = (
        &e.metadata.uid,
        &e.metadata.resource_version,
        &e.metadata.generation,
        &e.metadata.creation_timestamp,
        &e.metadata.managed_fields,
    );
    if let Some(t) = &e.event_time {
        OffsetDateTime::parse(t, &Rfc3339).map_err(|_| bad("eventTime must be valid RFC3339"))?;
    }
    let p = PlannedEventV0 {
        api_version: e.api_version,
        kind: e.kind,
        metadata: EventMetadata {
            name: e.metadata.name,
            namespace: e.metadata.namespace,
            labels: e.metadata.labels,
            annotations: e.metadata.annotations,
            owner_references: e.metadata.owner_references,
        },
        action: e.action,
        reason: e.reason,
        note: e.note,
        event_type: e.event_type,
        reporting_controller: e.reporting_controller,
        reporting_instance: e.reporting_instance,
        regarding: e.regarding,
    };
    let Some(d) = p.metadata.annotations.get(SEMANTIC_DIGEST_ANNOTATION) else {
        return Err(bad("missing semantic digest"));
    };
    if d != &semantic_digest(&p)? {
        return Err(bad("semantic digest mismatch"));
    }
    Ok(p)
}
/// Strictly compare recovered Events. Only `eventTime` and Kubernetes server
/// metadata fields are ignored; top-level lookalikes are rejected by decoding.
///
/// # Errors
///
/// Returns an error if either value is not a strict Event or its semantic
/// digest does not authenticate every non-server field.
pub fn semantically_equal(a: &Value, b: &Value) -> Result<bool, PlannerError> {
    Ok(decode(a)? == decode(b)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideloop_renderer_abi::{parse_validate_action_input, parse_validate_action_output};
    use serde_json::json;

    fn source() -> ObjectRef {
        ObjectRef {
            identity: ObjectIdentity {
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "demo".into(),
                name: "input".into(),
            },
            uid: "source-uid".into(),
            resource_version: "7".into(),
        }
    }
    fn root() -> ObjectRef {
        ObjectRef {
            identity: ObjectIdentity {
                api_version: "demo.oxideloop.dev/v1alpha1".into(),
                kind: "WebApplication".into(),
                namespace: "demo".into(),
                name: "portal".into(),
            },
            uid: "root-uid".into(),
            resource_version: "4".into(),
        }
    }
    fn run() -> ObjectRef {
        ObjectRef {
            identity: ObjectIdentity {
                api_version: "runtime.oxideloop.dev/v1alpha1".into(),
                kind: "AutomationRun".into(),
                namespace: "demo".into(),
                name: run_name("root-uid", "occurrence-1"),
            },
            uid: "run-uid".into(),
            resource_version: "8".into(),
        }
    }
    fn input() -> Validated<ActionInputV0> {
        parse_validate_action_input(&json!({"abiVersion":"renderer.oxideloop.dev/v0","bundleDigest":"a".repeat(64),"automationIdentity":"changed","automationVersion":"v1","actionModuleIdentity":"demo.action","actionModuleDigest":"b".repeat(64),"occurrenceId":"occurrence-1","sourceTransitionId":"transition-1","sourceUid":"source-uid","sourceResourceVersion":"7","eventMetadata":{"note":"changed"}}).to_string()).unwrap()
    }

    fn action_context_metadata() -> Value {
        json!({
            "$oxideloop": {
                "kind": "action-context",
                "version": "v1",
                "source": {
                    "apiVersion": "v1",
                    "kind": "ConfigMap",
                    "namespace": "demo",
                    "name": "input",
                    "uid": "source-uid",
                    "resourceVersion": "7"
                },
                "metadata": {"$oxideloop": {"user": true}, "note": "changed"}
            }
        })
    }

    fn input_with_action_context(metadata: Value) -> Validated<ActionInputV0> {
        let mut document = serde_json::to_value(input().document).unwrap();
        document["eventMetadata"] = metadata;
        parse_validate_action_input(&document.to_string()).unwrap()
    }
    fn corr(i: &Validated<ActionInputV0>) -> RunCorrelationV0 {
        let x = &i.document;
        RunCorrelationV0 {
            occurrence_id: x.occurrence_id.clone(),
            root: root(),
            run: run(),
            source: source().identity,
            source_uid: x.source_uid.clone(),
            source_resource_version: x.source_resource_version.clone(),
            source_transition_id: x.source_transition_id.clone(),
            automation_identity: x.automation_identity.clone(),
            automation_version: x.automation_version.clone(),
            bundle_digest: x.bundle_digest.clone(),
            action_module_identity: x.action_module_identity.clone(),
            action_module_digest: x.action_module_digest.clone(),
            renderer_abi_version: x.abi_version.clone(),
            action_input_hash: i.sha256.clone(),
            event_metadata_hash: sha256_hex(&canonical_bytes(&x.event_metadata).unwrap()),
        }
    }
    fn output() -> Validated<ActionOutputV0> {
        parse_validate_action_output(include_str!(
            "../../../packages/fixtures/golden/v0/action-output.valid.json"
        ))
        .unwrap()
    }
    fn plan() -> PlannedEventV0 {
        let i = input();
        let c = corr(&i);
        plan_event(&i, &output(), &root(), &source(), &c.run, &c).unwrap()
    }

    #[test]
    fn source_is_not_root_and_planning_is_deterministic() {
        let a = plan();
        let b = plan();
        assert_eq!(a, b);
        assert_eq!(a.regarding.name, "input");
        assert_ne!(a.regarding.name, root().identity.name);
        for value in a.metadata.labels.values() {
            assert_eq!(value.len(), 63);
            assert!(
                value
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            );
        }
        assert!(a.metadata.name.len() <= 63);
        assert!(!a.reporting_instance.contains("root-uid"));
        assert_eq!(
            a.metadata.name,
            "oxideloop-event-4267e5ee61bdf065b23bf2b4e5ebff4dbb27d3dd"
        );
        assert_eq!(
            a.metadata.labels[ROOT_UID_LABEL],
            "2003da55e422029d0661568ca7c5963bac70fc970355ee38fcb34035f54da53"
        );
        assert_eq!(
            a.metadata.labels[OCCURRENCE_LABEL],
            "811b54ad79376fd8448a49c958af531c79b37de38a9c480c04edb0e6811c887"
        );
        assert_eq!(
            a.metadata.labels[LOGICAL_ID_LABEL],
            "641fa24a86b1ead51ac8877b850df5fe98455ae0e4f4d15e292a85ef5e5b16d"
        );
        assert_eq!(
            a.reporting_instance,
            "root-0ae10a2304ea4346d3ba8c2880fc533e"
        );
        assert_eq!(
            a.metadata.annotations[SEMANTIC_DIGEST_ANNOTATION],
            "91032f5fae2a7cbbb7490d801d1291246822b82690ff46fdbb3f4cfeac71d88c"
        );
    }
    #[test]
    fn every_correlation_pin_is_checked() {
        let i = input();
        let fields: Vec<fn(&mut RunCorrelationV0)> = vec![
            |c| c.occurrence_id.push('x'),
            |c| c.root.uid.push('x'),
            |c| c.source_uid.push('x'),
            |c| c.source_resource_version.push('x'),
            |c| c.source_transition_id.push('x'),
            |c| c.automation_identity.push('x'),
            |c| c.automation_version.push('x'),
            |c| c.bundle_digest.push('x'),
            |c| c.action_module_identity.push('x'),
            |c| c.action_module_digest.push('x'),
            |c| c.renderer_abi_version.push('x'),
            |c| c.action_input_hash.push('x'),
            |c| c.event_metadata_hash.push('x'),
        ];
        for change in fields {
            let mut c = corr(&i);
            change(&mut c);
            assert!(plan_event(&i, &output(), &root(), &source(), &run(), &c).is_err());
        }
    }

    #[test]
    fn tagged_action_context_cannot_retarget_the_event_source() {
        let valid = input_with_action_context(action_context_metadata());
        let valid_corr = corr(&valid);
        assert!(plan_event(&valid, &output(), &root(), &source(), &run(), &valid_corr).is_ok());

        let mutations: [fn(&mut Value); 6] = [
            |source| source["apiVersion"] = json!("v2"),
            |source| source["kind"] = json!("Secret"),
            |source| source["namespace"] = json!("other"),
            |source| source["name"] = json!("other"),
            |source| source["uid"] = json!("other"),
            |source| source["resourceVersion"] = json!("8"),
        ];
        for mutate in mutations {
            let mut metadata = action_context_metadata();
            mutate(&mut metadata["$oxideloop"]["source"]);
            let retargeted = input_with_action_context(metadata);
            let correlation = corr(&retargeted);
            assert!(
                plan_event(
                    &retargeted,
                    &output(),
                    &root(),
                    &source(),
                    &run(),
                    &correlation
                )
                .is_err()
            );
        }
    }
    #[test]
    fn exact_root_and_run_references_and_generated_name_are_pinned() {
        let i = input();
        for mutate in [
            |c: &mut RunCorrelationV0| c.root.uid.push('x'),
            |c: &mut RunCorrelationV0| c.root.resource_version.push('x'),
            |c: &mut RunCorrelationV0| c.root.identity.name.push('x'),
            |c: &mut RunCorrelationV0| c.root.identity.api_version = "v1".into(),
            |c: &mut RunCorrelationV0| c.run.uid.push('x'),
            |c: &mut RunCorrelationV0| c.run.resource_version.push('x'),
            |c: &mut RunCorrelationV0| c.run.identity.name = "wrong".into(),
        ] {
            let mut c = corr(&i);
            mutate(&mut c);
            assert!(plan_event(&i, &output(), &root(), &source(), &run(), &c).is_err());
        }
        assert_eq!(
            run().identity.name,
            "ol-run-c9c330b81f6a82450cba7ad5b2d6782b52a82054dcd4487f2b635180"
        );
    }
    #[test]
    fn source_object_names_are_subdomains() {
        let mut s = source();
        s.identity.name = "a.very.long.but-valid.source-name.example".into();
        assert!(reference(&s).is_ok());
        for name in ["Upper", "a..b", &"a".repeat(254)] {
            let mut s = source();
            s.identity.name = name.into();
            assert!(reference(&s).is_err());
        }
    }
    #[test]
    fn authored_regarding_accepts_and_rejects_subdomain_names_through_planning() {
        let i = input();
        let valid = format!("{}.example", "a".repeat(60));
        let mut s = source();
        s.identity.name = valid.clone();
        let mut c = corr(&i);
        c.source = s.identity.clone();
        let mut document = serde_json::to_value(output().document).unwrap();
        document["actions"][0]["specification"]["regarding"]["name"] = json!(valid);
        let o = parse_validate_action_output(&document.to_string()).unwrap();
        assert!(plan_event(&i, &o, &root(), &s, &c.run, &c).is_ok());
        for invalid in ["Upper", "a..b", &"a".repeat(254)] {
            let mut document = serde_json::to_value(output().document).unwrap();
            document["actions"][0]["specification"]["regarding"]["name"] = json!(invalid);
            let o = parse_validate_action_output(&document.to_string()).unwrap();
            let normal = corr(&i);
            assert!(plan_event(&i, &o, &root(), &source(), &normal.run, &normal).is_err());
        }
    }
    #[test]
    fn strict_authored_shape_and_timestamps() {
        let i = input();
        let mut o = serde_json::to_value(output().document).unwrap();
        o["actions"][0]["specification"]["metadata"]["generateName"] = json!("bad-");
        let o = parse_validate_action_output(&o.to_string()).unwrap();
        let c = corr(&i);
        assert!(plan_event(&i, &o, &root(), &source(), &c.run, &c).is_err());
        let p = plan();
        assert!(materialize_event(&p, "2026-02-29T12:00:00Z").is_err());
        assert!(materialize_event(&p, "2024-02-29T25:00:00+24:00").is_err());
        assert_eq!(
            materialize_event(&p, "2024-02-29T12:00:00+01:00").unwrap()["eventTime"],
            "2024-02-29T11:00:00Z"
        );
    }
    #[test]
    fn recovery_only_ignores_documented_server_fields() {
        let p = plan();
        let a = materialize_event(&p, "2024-01-01T00:00:00Z").unwrap();
        let mut b = a.clone();
        b["eventTime"] = json!("2024-01-01T01:00:00Z");
        b["metadata"]["uid"] = json!("server");
        b["metadata"]["managedFields"] = json!([]);
        assert!(semantically_equal(&a, &b).unwrap());
        b["uid"] = json!("smuggle");
        assert!(semantically_equal(&a, &b).is_err());
    }
    #[test]
    fn semantic_digest_authenticates_every_planned_category() {
        let event = materialize_event(&plan(), "2024-01-01T00:00:00Z").unwrap();
        let mutations: Vec<fn(&mut Value)> = vec![
            |v| v["metadata"]["name"] = json!("other"),
            |v| v["metadata"]["namespace"] = json!("other"),
            |v| v["metadata"]["labels"][ROOT_UID_LABEL] = json!("x"),
            |v| v["metadata"]["labels"][OCCURRENCE_LABEL] = json!("x"),
            |v| v["metadata"]["labels"][LOGICAL_ID_LABEL] = json!("x"),
            |v| v["metadata"]["annotations"]["runtime.oxideloop.dev/root-uid"] = json!("x"),
            |v| v["metadata"]["annotations"]["runtime.oxideloop.dev/occurrence-id"] = json!("x"),
            |v| {
                v["metadata"]["annotations"]["runtime.oxideloop.dev/action-logical-id"] =
                    json!("x");
            },
            |v| v["metadata"]["ownerReferences"][0]["apiVersion"] = json!("v1"),
            |v| v["metadata"]["ownerReferences"][0]["kind"] = json!("Other"),
            |v| v["metadata"]["ownerReferences"][0]["name"] = json!("other"),
            |v| v["metadata"]["ownerReferences"][0]["uid"] = json!("other"),
            |v| v["metadata"]["ownerReferences"][0]["controller"] = json!(false),
            |v| v["metadata"]["ownerReferences"][0]["blockOwnerDeletion"] = json!(true),
            |v| v["action"] = json!("other"),
            |v| v["reason"] = json!("other"),
            |v| v["note"] = json!("other"),
            |v| v["type"] = json!("Warning"),
            |v| v["reportingController"] = json!("other"),
            |v| v["reportingInstance"] = json!("other"),
            |v| v["regarding"]["apiVersion"] = json!("v2"),
            |v| v["regarding"]["kind"] = json!("Other"),
            |v| v["regarding"]["namespace"] = json!("other"),
            |v| v["regarding"]["name"] = json!("other"),
            |v| v["regarding"]["uid"] = json!("other"),
            |v| v["regarding"]["resourceVersion"] = json!("other"),
        ];
        for mutate in mutations {
            let mut altered = event.clone();
            mutate(&mut altered);
            assert!(semantically_equal(&event, &altered).is_err());
        }
    }
    #[test]
    fn manifest_stays_pure() {
        let manifest = include_str!("../Cargo.toml");
        for forbidden in ["kube", "k8s-openapi", "rand", "chrono"] {
            assert!(
                !manifest.contains(forbidden),
                "forbidden dependency: {forbidden}"
            );
        }
    }
}
