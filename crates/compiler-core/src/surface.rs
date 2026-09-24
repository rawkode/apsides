//! The deployment surface: a reviewable projection of a [`CompiledPlan`].
//!
//! `derive_surface` is pure and derives its output ONLY from `CompiledPlan`,
//! so the plan's own IR digest already binds the surface's content: an
//! independently recomputed surface either matches byte for byte or the
//! plan differs. Every section is sorted by a stable key — never by the IR's
//! own array order, which `canonical_bytes` preserves — so shuffling the
//! declaration order of resources or inputs in the source IR never changes
//! the canonical bytes.
//!
//! The surface deliberately excludes desired values, `ConfigMap` data, and
//! source file locations: it is meant to be shared and diffed (a future
//! `aps diff`), not to reproduce the program. It does include every secret
//! the reconciler reads, including its own rollout-key secret — an internal
//! mechanism, but still real read authority a reviewer should see.

use crate::model::{CompiledPlan, DeletionPolicy, InputMode};
use serde::{Deserialize, Serialize};

/// The `surface.json` schema version this module emits.
pub const SURFACE_SCHEMA: &str = "apsides.dev/surface/v1";

/// The exact server version the absent-only conditional-create contract is
/// qualified on today (see `apply_absent_with_list_version`). The minimum
/// version itself (`kubernetes.minVersion`) is author-declared per domain
/// (`domain.minKubernetesVersion`); this is the fixed version that
/// declared floor is qualified against.
const KUBERNETES_QUALIFIED_ON: &str = "v1.34.3";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SurfaceDomain {
    pub id: String,
    pub namespace: String,
    pub control_namespace: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SurfaceKubernetes {
    pub min_version: String,
    pub qualified_on: String,
}

/// Runtime capabilities collapsed onto one row per target.
///
/// Grouped by (namespace, `apiGroup`, resource, subresource), with one
/// [`SurfaceVerb`] per verb requested against it, sorted and deduplicated so
/// grouping order cannot change the canonical bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SurfacePermission {
    pub namespace: String,
    pub api_group: String,
    pub resource: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subresource: Option<String>,
    pub verbs: Vec<SurfaceVerb>,
}

/// One verb requested against a [`SurfacePermission`]'s target, with the
/// object names it is scoped to — the union of names across every
/// capability that requested this verb against this target.
///
/// When any such capability was unrestricted (an empty name list, e.g. the
/// `create` verb's list-before-create authorization), `allNames` is `true`
/// and `names` is omitted: a verb is never shown as name-restricted when
/// some capability actually requested it without a name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SurfaceVerb {
    pub verb: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all_names: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub names: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SurfaceSecretRead {
    pub namespace: String,
    pub name: String,
    /// `"reconcile"` for the controller's own reconcile loop; a handler id
    /// once Phase 4/5 add handlers that read secrets.
    pub reader: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SurfaceResource {
    pub id: String,
    pub api_version: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub deletion_policy: DeletionPolicy,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SurfaceInput {
    pub id: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub mode: InputMode,
    pub sensitive: bool,
}

/// One input-to-target dataflow edge, deduplicated: an input id feeding the
/// same resource target through more than one reaction still appears once.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SurfaceReaction {
    pub input: String,
    pub target: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SurfaceImage {
    pub resource: String,
    pub container: String,
    pub image: String,
    pub pinned: bool,
}

/// A declared event handler: what triggers it and what it reads and writes.
/// Never the handler's own source or bundle contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SurfaceHandler {
    pub id: String,
    /// The input id whose revision change triggers this handler.
    pub trigger: String,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
}

/// One egress host a handler's `fetch` op may reach, always over HTTPS.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SurfaceEgress {
    pub host: String,
    pub port: u16,
    pub handler: String,
}

/// A handler-written managed `ConfigMap` output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SurfaceOutput {
    pub id: String,
    pub name: String,
    pub namespace: String,
}

/// A reviewable projection of a compiled domain: what it can reach, what it
/// reads and writes, and what it runs — never the values it reads or writes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Surface {
    pub schema_version: String,
    pub domain: SurfaceDomain,
    pub kubernetes: SurfaceKubernetes,
    pub permissions: Vec<SurfacePermission>,
    pub secrets_read: Vec<SurfaceSecretRead>,
    pub resources: Vec<SurfaceResource>,
    pub inputs: Vec<SurfaceInput>,
    pub reactions: Vec<SurfaceReaction>,
    pub images: Vec<SurfaceImage>,
    pub handlers: Vec<SurfaceHandler>,
    pub egress: Vec<SurfaceEgress>,
    pub outputs: Vec<SurfaceOutput>,
    pub risks: Vec<String>,
    pub enforcement_gaps: Vec<String>,
}

/// Insert `value` into a sorted, deduplicated `Vec<String>`.
fn insert_sorted(values: &mut Vec<String>, value: String) {
    if let Err(index) = values.binary_search(&value) {
        values.insert(index, value);
    }
}

/// Merge one capability's verb and names into a target's verb list: find or
/// start that verb's entry, then union in the names, or mark it `allNames`
/// if this or any earlier capability for the same verb had none.
fn merge_verb(verbs: &mut Vec<SurfaceVerb>, verb: &str, names: &[String]) {
    let index = verbs
        .iter()
        .position(|v| v.verb == verb)
        .unwrap_or_else(|| {
            verbs.push(SurfaceVerb {
                verb: verb.to_owned(),
                all_names: false,
                names: Some(Vec::new()),
            });
            verbs.len() - 1
        });
    let entry = &mut verbs[index];
    if names.is_empty() {
        entry.all_names = true;
        entry.names = None;
    } else if !entry.all_names {
        let scoped = entry.names.get_or_insert_with(Vec::new);
        for name in names {
            insert_sorted(scoped, name.clone());
        }
    }
}

fn permissions(plan: &CompiledPlan) -> Vec<SurfacePermission> {
    // `plan.capabilities` is already sorted by (namespace, apiGroup, resource,
    // subresource, verb, names) — folding adjacent rows sharing the first
    // four fields groups each target without needing a hash map, and each
    // target's verbs arrive in sorted order too.
    let mut groups: Vec<SurfacePermission> = Vec::new();
    for capability in &plan.capabilities {
        let same_group = groups.last().is_some_and(|group| {
            group.namespace == capability.namespace
                && group.api_group == capability.api_group
                && group.resource == capability.resource
                && group.subresource == capability.subresource
        });
        if !same_group {
            groups.push(SurfacePermission {
                namespace: capability.namespace.clone(),
                api_group: capability.api_group.clone(),
                resource: capability.resource.clone(),
                subresource: capability.subresource.clone(),
                verbs: Vec::new(),
            });
        }
        if let Some(group) = groups.last_mut() {
            merge_verb(&mut group.verbs, &capability.verb, &capability.names);
        }
    }
    groups
}

/// Every secret the reconciler reads: every `get` capability on the `secrets`
/// resource, one row per (namespace, name). This covers both a program's own
/// observed `Secret` inputs and Apsides's own rollout-key secret uniformly —
/// both are real read authority a reviewer should see.
fn secrets_read(plan: &CompiledPlan) -> Vec<SurfaceSecretRead> {
    let mut rows: Vec<SurfaceSecretRead> = plan
        .capabilities
        .iter()
        .filter(|capability| {
            capability.api_group.is_empty()
                && capability.resource == "secrets"
                && capability.verb == "get"
        })
        .flat_map(|capability| {
            capability.names.iter().map(|name| SurfaceSecretRead {
                namespace: capability.namespace.clone(),
                name: name.clone(),
                reader: "reconcile".into(),
            })
        })
        .collect();
    rows.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    rows.dedup();
    rows
}

fn resources(plan: &CompiledPlan) -> Vec<SurfaceResource> {
    let mut rows: Vec<SurfaceResource> = plan
        .ir
        .resources
        .iter()
        .map(|resource| SurfaceResource {
            id: resource.id.clone(),
            api_version: resource.api_version.clone(),
            kind: resource.kind.clone(),
            namespace: resource.namespace.clone(),
            name: resource.name.clone(),
            deletion_policy: resource.lifecycle.deletion_policy,
            force: resource.force,
        })
        .collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows
}

fn inputs(plan: &CompiledPlan) -> Vec<SurfaceInput> {
    let mut rows: Vec<SurfaceInput> = plan
        .ir
        .inputs
        .iter()
        .map(|input| SurfaceInput {
            id: input.id.clone(),
            kind: input.kind.clone(),
            namespace: input.namespace.clone(),
            name: input.name.clone(),
            mode: input.mode,
            sensitive: input.sensitive,
        })
        .collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows
}

fn reactions(plan: &CompiledPlan) -> Vec<SurfaceReaction> {
    let mut rows: Vec<SurfaceReaction> = plan
        .ir
        .reactions
        .iter()
        .flat_map(|reaction| {
            reaction.inputs.iter().map(|input| SurfaceReaction {
                input: input.clone(),
                target: reaction.target.clone(),
            })
        })
        .collect();
    rows.sort();
    rows.dedup();
    rows
}

fn images(plan: &CompiledPlan) -> Vec<SurfaceImage> {
    let mut rows: Vec<SurfaceImage> = plan
        .ir
        .resources
        .iter()
        .filter(|resource| resource.kind == "Deployment")
        .flat_map(|resource| {
            resource
                .desired
                .pointer("/spec/template/spec/containers")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|container| {
                    let name = container.get("name")?.as_str()?.to_owned();
                    let image = container.get("image")?.as_str()?.to_owned();
                    let pinned = image.contains("@sha256:");
                    Some(SurfaceImage {
                        resource: resource.id.clone(),
                        container: name,
                        image,
                        pinned,
                    })
                })
        })
        .collect();
    rows.sort_by(|a, b| (&a.resource, &a.container).cmp(&(&b.resource, &b.container)));
    rows
}

fn handlers(plan: &CompiledPlan) -> Vec<SurfaceHandler> {
    let mut rows: Vec<SurfaceHandler> = plan
        .ir
        .handlers
        .iter()
        .map(|handler| {
            let mut inputs = handler.inputs.clone();
            inputs.sort();
            let mut outputs = handler.outputs.clone();
            outputs.sort();
            SurfaceHandler {
                id: handler.id.clone(),
                trigger: handler.trigger.input.clone(),
                inputs,
                outputs,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows
}

fn egress(plan: &CompiledPlan) -> Vec<SurfaceEgress> {
    let mut rows: Vec<SurfaceEgress> = plan
        .ir
        .handlers
        .iter()
        .flat_map(|handler| {
            handler.egress.iter().map(|host| SurfaceEgress {
                host: host.host.clone(),
                port: host.port,
                handler: handler.id.clone(),
            })
        })
        .collect();
    rows.sort();
    rows.dedup();
    rows
}

fn outputs(plan: &CompiledPlan) -> Vec<SurfaceOutput> {
    let mut rows: Vec<SurfaceOutput> = plan
        .ir
        .outputs
        .iter()
        .map(|output| SurfaceOutput {
            id: output.id.clone(),
            name: output.name.clone(),
            namespace: output.namespace.clone(),
        })
        .collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows
}

/// Derive the reviewable deployment surface from a validated, compiled plan.
///
/// Pure and deterministic: the result depends only on `plan`, and every
/// section is sorted so the input and resource declaration order in the
/// source program never changes the canonical bytes.
#[must_use]
pub fn derive_surface(plan: &CompiledPlan) -> Surface {
    Surface {
        schema_version: SURFACE_SCHEMA.into(),
        domain: SurfaceDomain {
            id: plan.ir.domain.id.clone(),
            namespace: plan.ir.domain.namespace.clone(),
            control_namespace: plan.ir.domain.control_namespace.clone(),
        },
        kubernetes: SurfaceKubernetes {
            min_version: plan.ir.domain.min_kubernetes_version.clone(),
            qualified_on: KUBERNETES_QUALIFIED_ON.into(),
        },
        permissions: permissions(plan),
        secrets_read: secrets_read(plan),
        resources: resources(plan),
        inputs: inputs(plan),
        reactions: reactions(plan),
        images: images(plan),
        handlers: handlers(plan),
        egress: egress(plan),
        outputs: outputs(plan),
        risks: plan.indirect_risks.clone(),
        enforcement_gaps: plan.enforcement_gaps.clone(),
    }
}
