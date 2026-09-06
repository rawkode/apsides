//! Explicit metadata allowlist: no resource bodies, observed values or content commitments.
use compiler_core::{CompiledPlan, SourceLocation};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Serialize)]
pub(super) struct Unavailable {
    status: &'static str,
    reason: &'static str,
}
impl Unavailable {
    pub(super) const fn historical() -> Self {
        Self {
            status: "unavailable",
            reason: "historical-compiled-plan-not-retained",
        }
    }
    pub(super) const fn live() -> Self {
        Self {
            status: "unavailable",
            reason: "live-server-side-apply-preview-not-executed",
        }
    }
}
#[derive(Serialize)]
#[serde(untagged)]
pub(super) enum Detail<T> {
    Available(T),
    Unavailable(Unavailable),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Changes<'a, T: Serialize> {
    baseline: &'static str,
    before: Detail<&'a [T]>,
    after: &'a [T],
    added: Detail<&'a [T]>,
    removed: Detail<&'a [T]>,
    changed: Detail<&'a [T]>,
}
impl<'a, T: Serialize> Changes<'a, T> {
    pub(super) fn new(after: &'a [T], initial: bool) -> Self {
        let detail = |items: &'a [T]| {
            if initial {
                Detail::Available(items)
            } else {
                Detail::Unavailable(Unavailable::historical())
            }
        };
        Self {
            baseline: if initial {
                "empty-initial-declaration"
            } else {
                "historical-declaration-unavailable"
            },
            before: detail(&[]),
            after,
            added: detail(after),
            removed: detail(&[]),
            changed: detail(&[]),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Reaction<'a> {
    id: &'a str,
    op: &'a str,
    inputs: &'a [String],
    target: &'a str,
    source: &'a SourceLocation,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Input<'a> {
    id: &'a str,
    api_version: &'a str,
    kind: &'a str,
    namespace: &'a str,
    name: &'a str,
    mode: compiler_core::InputMode,
    managed_resource_id: Option<&'a str>,
    projection: &'a [String],
    sensitive: bool,
    missing: &'a str,
    source: &'a SourceLocation,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Effect<'a> {
    target: &'a str,
    operation: &'a str,
    field_paths: &'a [String],
    source: &'a SourceLocation,
}
#[derive(Serialize)]
pub(super) struct Dataflow<'a> {
    reaction: &'a str,
    inputs: &'a [String],
    target: &'a str,
}
#[derive(Serialize)]
pub(super) struct SourceEntry<'a> {
    category: &'static str,
    id: &'a str,
    source: &'a SourceLocation,
}

pub(super) struct Declarations<'a> {
    pub reactions: Vec<Reaction<'a>>,
    pub inputs: Vec<Input<'a>>,
    pub effects: Vec<Effect<'a>>,
    pub dataflow: Vec<Dataflow<'a>>,
    pub sources: Vec<SourceEntry<'a>>,
}
impl<'a> Declarations<'a> {
    pub(super) fn new(plan: &'a CompiledPlan) -> crate::Result<Self> {
        validate_effects(plan)?;
        let mut reactions: Vec<_> = plan
            .ir
            .reactions
            .iter()
            .map(|r| Reaction {
                id: &r.id,
                op: &r.op,
                inputs: &r.inputs,
                target: &r.target,
                source: &r.source,
            })
            .collect();
        reactions.sort_by_key(|r| r.id);
        let dataflow = reactions
            .iter()
            .map(|r| Dataflow {
                reaction: r.id,
                inputs: r.inputs,
                target: r.target,
            })
            .collect();
        let mut inputs: Vec<_> = plan
            .ir
            .inputs
            .iter()
            .map(|i| Input {
                id: &i.id,
                api_version: &i.api_version,
                kind: &i.kind,
                namespace: &i.namespace,
                name: &i.name,
                mode: i.mode,
                managed_resource_id: i.managed_resource_id.as_deref(),
                projection: &i.projection,
                sensitive: i.sensitive,
                missing: &i.missing,
                source: &i.source,
            })
            .collect();
        inputs.sort_by_key(|i| i.id);
        let mut effects: Vec<_> = plan
            .effects
            .iter()
            .map(|e| Effect {
                target: &e.target,
                operation: &e.operation,
                field_paths: &e.field_paths,
                source: &e.source,
            })
            .collect();
        effects.sort_by_key(|e| {
            (
                e.target,
                e.operation,
                &e.source.file,
                e.source.line,
                e.source.column,
            )
        });
        let mut sources: Vec<_> = plan
            .ir
            .resources
            .iter()
            .map(|r| SourceEntry {
                category: "resource",
                id: &r.id,
                source: &r.source,
            })
            .chain(inputs.iter().map(|i| SourceEntry {
                category: "input",
                id: i.id,
                source: i.source,
            }))
            .chain(reactions.iter().map(|r| SourceEntry {
                category: "reaction",
                id: r.id,
                source: r.source,
            }))
            .collect();
        sources.sort_by_key(|s| (s.category, s.id));
        Ok(Self {
            reactions,
            inputs,
            effects,
            dataflow,
            sources,
        })
    }
}

fn validate_effects(plan: &CompiledPlan) -> crate::Result<()> {
    let invalid = "review encountered an ambiguous canonical effect catalog";
    let mut expected = BTreeMap::new();
    for r in &plan.ir.resources {
        let found: Vec<_> = plan
            .effects
            .iter()
            .filter(|e| e.operation == "server-side-apply" && e.target == r.id)
            .collect();
        if found.len() != 1 || found.first().is_none_or(|e| e.source != r.source) {
            return Err(invalid.into());
        }
    }
    for r in &plan.ir.reactions {
        *expected
            .entry((&r.target, &r.source.file, r.source.line, r.source.column))
            .or_insert(0usize) += 1;
    }
    let mut actual = BTreeMap::new();
    let mut apply_count = 0;
    for e in &plan.effects {
        match e.operation.as_str() {
            "server-side-apply" => apply_count += 1,
            "rollout-contribution"
                if e.field_paths == ["/spec/template/metadata/annotations/reakt.dev~1revision"] =>
            {
                *actual
                    .entry((&e.target, &e.source.file, e.source.line, e.source.column))
                    .or_insert(0usize) += 1;
            }
            _ => return Err(invalid.into()),
        }
    }
    if apply_count != plan.ir.resources.len() || actual != expected {
        return Err(invalid.into());
    }
    Ok(())
}
