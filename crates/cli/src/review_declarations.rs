//! Declaration deltas derived only from authenticated, value-free review metadata.
use coordination::{ReviewMetadataV1, ReviewPrimitive, ReviewSource};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Serialize)]
pub(super) struct Unavailable {
    status: &'static str,
    reason: &'static str,
}
impl Unavailable {
    pub(super) const fn live() -> Self {
        Self {
            status: "unavailable",
            reason: "live-server-side-apply-preview-not-executed",
        }
    }
}

#[derive(Serialize)]
pub(super) struct Changed<'a, T: Serialize> {
    before: &'a T,
    after: &'a T,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Changes<'a, T: Serialize> {
    baseline: &'static str,
    before: &'a [T],
    after: &'a [T],
    added: Vec<&'a T>,
    removed: Vec<&'a T>,
    changed: Vec<Changed<'a, T>>,
    retained: Vec<&'a T>,
}
impl<'a, T: PartialEq + Serialize> Changes<'a, T> {
    pub(super) fn keyed<K: Ord>(
        before: Option<&'a [T]>,
        after: &'a [T],
        key: impl Fn(&T) -> K,
    ) -> crate::Result<Self> {
        let initial = before.is_none();
        let before = before.unwrap_or(&[]);
        let old = index(before, &key)?;
        let new = index(after, &key)?;
        let mut added = Vec::new();
        let mut removed = Vec::new();
        let mut changed = Vec::new();
        let mut retained = Vec::new();
        for (id, value) in &new {
            let value = *value;
            match old.get(id).copied() {
                None => added.push(value),
                Some(previous) if previous == value => retained.push(value),
                Some(previous) => changed.push(Changed {
                    before: previous,
                    after: value,
                }),
            }
        }
        for (id, value) in &old {
            if !new.contains_key(id) {
                removed.push(*value);
            }
        }
        Ok(Self {
            baseline: if initial {
                "empty-initial-declaration"
            } else {
                "authenticated-declaration-metadata"
            },
            before,
            after,
            added,
            removed,
            changed,
            retained,
        })
    }
}

fn index<'a, K: Ord, T>(
    values: &'a [T],
    key: &impl Fn(&T) -> K,
) -> crate::Result<BTreeMap<K, &'a T>> {
    let mut indexed = BTreeMap::new();
    for value in values {
        if indexed.insert(key(value), value).is_some() {
            return Err("review metadata contains a duplicate declaration identity".into());
        }
    }
    Ok(indexed)
}

#[derive(PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Effect<'a> {
    pub id: String,
    target: &'a str,
    operation: &'static str,
    field_paths: &'a [String],
    source: &'a ReviewSource,
}
#[derive(PartialEq, Eq, Serialize)]
pub(super) struct Dataflow<'a> {
    pub reaction: &'a str,
    inputs: &'a [String],
    target: &'a str,
}
#[derive(PartialEq, Eq, Serialize)]
pub(super) struct SourceEntry<'a> {
    pub category: &'static str,
    pub id: &'a str,
    source: &'a ReviewSource,
}

pub(super) struct Declarations<'a> {
    pub effects: Vec<Effect<'a>>,
    pub dataflow: Vec<Dataflow<'a>>,
    pub sources: Vec<SourceEntry<'a>>,
}
impl<'a> Declarations<'a> {
    pub(super) fn new(metadata: &'a ReviewMetadataV1) -> Self {
        let mut effects = Vec::with_capacity(metadata.resources.len() + metadata.reactions.len());
        effects.extend(metadata.resources.iter().map(|resource| Effect {
            id: format!("resource:{}", resource.id),
            target: &resource.id,
            operation: "server-side-apply",
            field_paths: &resource.apply_fields,
            source: &resource.source,
        }));
        effects.extend(metadata.reactions.iter().map(|reaction| Effect {
            id: format!("reaction:{}", reaction.id),
            target: &reaction.target_resource_id,
            operation: match reaction.op {
                ReviewPrimitive::Rollout => "rollout-contribution",
            },
            field_paths: &reaction.effect_fields,
            source: &reaction.source,
        }));
        effects.sort_by(|a, b| a.id.cmp(&b.id));
        let dataflow = metadata
            .reactions
            .iter()
            .map(|reaction| Dataflow {
                reaction: &reaction.id,
                inputs: &reaction.input_ids,
                target: &reaction.target_resource_id,
            })
            .collect();
        let mut sources: Vec<_> = metadata
            .resources
            .iter()
            .map(|resource| SourceEntry {
                category: "resource",
                id: &resource.id,
                source: &resource.source,
            })
            .chain(metadata.inputs.iter().map(|input| SourceEntry {
                category: "input",
                id: &input.id,
                source: &input.source,
            }))
            .chain(metadata.reactions.iter().map(|reaction| SourceEntry {
                category: "reaction",
                id: &reaction.id,
                source: &reaction.source,
            }))
            .collect();
        sources.sort_by_key(|source| (source.category, source.id));
        Self {
            effects,
            dataflow,
            sources,
        }
    }
}
