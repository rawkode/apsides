//! `aps diff`: a pure, structural comparison of two deployment surfaces.
//!
//! [`diff_surfaces`] never re-derives anything from a [`CompiledPlan`]; it
//! only compares two already-derived [`Surface`] values (Phase 1's
//! `surface.rs`), so it stays correct no matter what later phases add to a
//! `Surface` — Phase 3's `kubernetes` values, Phase 4's `handlers`/`egress`/
//! `outputs`, and so on — as long as those additions get their own section
//! here following the same shape: keyed by a stable identity, with
//! `added`/`removed` rows and, where a kept identity can still change
//! internally, a `changed` row describing exactly what moved.
//!
//! Every section is sorted by its key so the result is deterministic
//! regardless of the declaration order in either input `Surface`.

use crate::model::{DeletionPolicy, InputMode};
use crate::surface::{
    Surface, SurfaceDomain, SurfaceImage, SurfaceInput, SurfaceKubernetes, SurfacePermission,
    SurfaceReaction, SurfaceResource, SurfaceSecretRead, SurfaceVerb,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// The `surface-diff.json` schema version this module emits.
pub const SURFACE_DIFF_SCHEMA: &str = "apsides.dev/surface-diff/v1";

/// A field that differs between the base and head surface.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ValueChange<T> {
    pub base: T,
    pub head: T,
}

fn value_change<T: Clone + PartialEq>(base: &T, head: &T) -> Option<ValueChange<T>> {
    (base != head).then(|| ValueChange {
        base: base.clone(),
        head: head.clone(),
    })
}

/// One verb whose scoped names or `allNames` breadth changed on a kept
/// permission target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionVerbChange {
    pub verb: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names_added: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names_removed: Vec<String>,
    /// The verb went from a name-restricted set to every name (`allNames`):
    /// always reported as a widening, never folded into `namesAdded`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub widened_to_all_names: bool,
    /// The verb went from `allNames` back to a name-restricted set.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub narrowed_from_all_names: bool,
}

/// A kept permission target ((namespace, `apiGroup`, resource, subresource))
/// whose verb set changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionChange {
    pub namespace: String,
    pub api_group: String,
    pub resource: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subresource: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verbs_added: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verbs_removed: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verbs_changed: Vec<PermissionVerbChange>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionsDiff {
    pub added: Vec<SurfacePermission>,
    pub removed: Vec<SurfacePermission>,
    pub changed: Vec<PermissionChange>,
}

impl PermissionsDiff {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// A kept secret ((namespace, name)) whose reader changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretReadChange {
    pub namespace: String,
    pub name: String,
    pub reader: ValueChange<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretsReadDiff {
    pub added: Vec<SurfaceSecretRead>,
    pub removed: Vec<SurfaceSecretRead>,
    pub changed: Vec<SecretReadChange>,
}

impl SecretsReadDiff {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// A kept resource ((kind, namespace, name)) whose other fields changed.
///
/// The resource's own `id` is compared too — an author-visible rename of the
/// same live object — but never the resource's key, since `apiVersion` can
/// move (a CRD version bump) while the deployed object stays the same
/// identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceChange {
    pub kind: String,
    pub namespace: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<ValueChange<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_version: Option<ValueChange<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion_policy: Option<ValueChange<DeletionPolicy>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force: Option<ValueChange<bool>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourcesDiff {
    pub added: Vec<SurfaceResource>,
    pub removed: Vec<SurfaceResource>,
    pub changed: Vec<ResourceChange>,
}

impl ResourcesDiff {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// A kept input (by id) whose other fields changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputChange {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ValueChange<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<ValueChange<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<ValueChange<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ValueChange<InputMode>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensitive: Option<ValueChange<bool>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputsDiff {
    pub added: Vec<SurfaceInput>,
    pub removed: Vec<SurfaceInput>,
    pub changed: Vec<InputChange>,
}

impl InputsDiff {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// Reactions are a set of (input, target) edges: an edge either exists or it
/// does not, so there is no `changed` row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReactionsDiff {
    pub added: Vec<SurfaceReaction>,
    pub removed: Vec<SurfaceReaction>,
}

impl ReactionsDiff {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

/// A kept image target ((resource, container)) whose image reference or pin
/// state changed.
///
/// `unpinned` is derived for the policy engine and the markdown renderer:
/// `true` exactly when the head image is unpinned, regardless of whether the
/// base image was pinned, unpinned, or this is the image's first appearance
/// on this target (which is instead reported in `ImagesDiff::added`, already
/// carrying its own `pinned` flag).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageChange {
    pub resource: String,
    pub container: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ValueChange<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<ValueChange<bool>>,
    pub unpinned: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImagesDiff {
    pub added: Vec<SurfaceImage>,
    pub removed: Vec<SurfaceImage>,
    pub changed: Vec<ImageChange>,
}

impl ImagesDiff {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// Risks and enforcement gaps are sets of strings: a change is an addition
/// and a removal, not an in-place edit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StringSetDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

impl StringSetDiff {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DomainChange {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<ValueChange<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<ValueChange<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_namespace: Option<ValueChange<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KubernetesChange {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_version: Option<ValueChange<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualified_on: Option<ValueChange<String>>,
}

/// A structural comparison of two deployment surfaces: what a reviewer needs
/// to approve moving from `base` to `head`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SurfaceDiff {
    pub schema_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<DomainChange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubernetes: Option<KubernetesChange>,
    pub permissions: PermissionsDiff,
    pub secrets_read: SecretsReadDiff,
    pub resources: ResourcesDiff,
    pub inputs: InputsDiff,
    pub reactions: ReactionsDiff,
    pub images: ImagesDiff,
    pub risks: StringSetDiff,
    pub enforcement_gaps: StringSetDiff,
}

/// The union of two `BTreeMap`s' keys, sorted and deduplicated.
fn union_keys<K: Ord + Clone, A, B>(base: &BTreeMap<K, A>, head: &BTreeMap<K, B>) -> Vec<K> {
    let mut keys: BTreeSet<K> = base.keys().cloned().collect();
    keys.extend(head.keys().cloned());
    keys.into_iter().collect()
}

fn domain_change(base: &SurfaceDomain, head: &SurfaceDomain) -> Option<DomainChange> {
    let id = value_change(&base.id, &head.id);
    let namespace = value_change(&base.namespace, &head.namespace);
    let control_namespace = value_change(&base.control_namespace, &head.control_namespace);
    (id.is_some() || namespace.is_some() || control_namespace.is_some()).then_some(DomainChange {
        id,
        namespace,
        control_namespace,
    })
}

fn kubernetes_change(
    base: &SurfaceKubernetes,
    head: &SurfaceKubernetes,
) -> Option<KubernetesChange> {
    let min_version = value_change(&base.min_version, &head.min_version);
    let qualified_on = value_change(&base.qualified_on, &head.qualified_on);
    (min_version.is_some() || qualified_on.is_some()).then_some(KubernetesChange {
        min_version,
        qualified_on,
    })
}

type PermissionKey = (String, String, String, Option<String>);

fn permission_key(permission: &SurfacePermission) -> PermissionKey {
    (
        permission.namespace.clone(),
        permission.api_group.clone(),
        permission.resource.clone(),
        permission.subresource.clone(),
    )
}

fn verb_names(verb: &SurfaceVerb) -> BTreeSet<&str> {
    verb.names
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(String::as_str)
        .collect()
}

fn diff_verb(verb: &str, base: &SurfaceVerb, head: &SurfaceVerb) -> Option<PermissionVerbChange> {
    let widened_to_all_names = !base.all_names && head.all_names;
    let narrowed_from_all_names = base.all_names && !head.all_names;
    // A verb's `names` list is only meaningful when neither side is
    // `allNames`: across a widening or narrowing transition one side has no
    // explicit list at all, and diffing the other side's names against
    // nothing would report every one of them as "added" or "removed" —
    // noise that just restates the transition the two dedicated flags above
    // already report accurately.
    let (names_added, names_removed) = if base.all_names || head.all_names {
        (Vec::new(), Vec::new())
    } else {
        let base_names = verb_names(base);
        let head_names = verb_names(head);
        let names_added = head_names
            .difference(&base_names)
            .map(|name| (*name).to_owned())
            .collect();
        let names_removed = base_names
            .difference(&head_names)
            .map(|name| (*name).to_owned())
            .collect();
        (names_added, names_removed)
    };
    if names_added.is_empty()
        && names_removed.is_empty()
        && !widened_to_all_names
        && !narrowed_from_all_names
    {
        return None;
    }
    Some(PermissionVerbChange {
        verb: verb.to_owned(),
        names_added,
        names_removed,
        widened_to_all_names,
        narrowed_from_all_names,
    })
}

fn diff_permission(base: &SurfacePermission, head: &SurfacePermission) -> Option<PermissionChange> {
    let base_verbs: BTreeMap<&str, &SurfaceVerb> = base
        .verbs
        .iter()
        .map(|verb| (verb.verb.as_str(), verb))
        .collect();
    let head_verbs: BTreeMap<&str, &SurfaceVerb> = head
        .verbs
        .iter()
        .map(|verb| (verb.verb.as_str(), verb))
        .collect();
    let mut verbs_added = Vec::new();
    let mut verbs_removed = Vec::new();
    let mut verbs_changed = Vec::new();
    for verb in union_keys(&base_verbs, &head_verbs) {
        match (base_verbs.get(verb), head_verbs.get(verb)) {
            (None, Some(_)) => verbs_added.push(verb.to_owned()),
            (Some(_), None) => verbs_removed.push(verb.to_owned()),
            (Some(base), Some(head)) => verbs_changed.extend(diff_verb(verb, base, head)),
            (None, None) => {}
        }
    }
    if verbs_added.is_empty() && verbs_removed.is_empty() && verbs_changed.is_empty() {
        return None;
    }
    Some(PermissionChange {
        namespace: head.namespace.clone(),
        api_group: head.api_group.clone(),
        resource: head.resource.clone(),
        subresource: head.subresource.clone(),
        verbs_added,
        verbs_removed,
        verbs_changed,
    })
}

fn diff_permissions(base: &[SurfacePermission], head: &[SurfacePermission]) -> PermissionsDiff {
    let base_map: BTreeMap<PermissionKey, &SurfacePermission> =
        base.iter().map(|row| (permission_key(row), row)).collect();
    let head_map: BTreeMap<PermissionKey, &SurfacePermission> =
        head.iter().map(|row| (permission_key(row), row)).collect();
    let mut diff = PermissionsDiff::default();
    for key in union_keys(&base_map, &head_map) {
        match (base_map.get(&key), head_map.get(&key)) {
            (None, Some(head)) => diff.added.push((*head).clone()),
            (Some(base), None) => diff.removed.push((*base).clone()),
            (Some(base), Some(head)) => diff.changed.extend(diff_permission(base, head)),
            (None, None) => {}
        }
    }
    diff
}

type SecretKey = (String, String);

fn secret_key(secret: &SurfaceSecretRead) -> SecretKey {
    (secret.namespace.clone(), secret.name.clone())
}

fn diff_secrets_read(base: &[SurfaceSecretRead], head: &[SurfaceSecretRead]) -> SecretsReadDiff {
    let base_map: BTreeMap<SecretKey, &SurfaceSecretRead> =
        base.iter().map(|row| (secret_key(row), row)).collect();
    let head_map: BTreeMap<SecretKey, &SurfaceSecretRead> =
        head.iter().map(|row| (secret_key(row), row)).collect();
    let mut diff = SecretsReadDiff::default();
    for key in union_keys(&base_map, &head_map) {
        match (base_map.get(&key), head_map.get(&key)) {
            (None, Some(head)) => diff.added.push((*head).clone()),
            (Some(base), None) => diff.removed.push((*base).clone()),
            (Some(base), Some(head)) => {
                if let Some(reader) = value_change(&base.reader, &head.reader) {
                    diff.changed.push(SecretReadChange {
                        namespace: head.namespace.clone(),
                        name: head.name.clone(),
                        reader,
                    });
                }
            }
            (None, None) => {}
        }
    }
    diff
}

type ResourceKey = (String, String, String);

fn resource_key(resource: &SurfaceResource) -> ResourceKey {
    (
        resource.kind.clone(),
        resource.namespace.clone(),
        resource.name.clone(),
    )
}

fn diff_resource(base: &SurfaceResource, head: &SurfaceResource) -> Option<ResourceChange> {
    let id = value_change(&base.id, &head.id);
    let api_version = value_change(&base.api_version, &head.api_version);
    let deletion_policy = value_change(&base.deletion_policy, &head.deletion_policy);
    let force = value_change(&base.force, &head.force);
    if id.is_none() && api_version.is_none() && deletion_policy.is_none() && force.is_none() {
        return None;
    }
    Some(ResourceChange {
        kind: head.kind.clone(),
        namespace: head.namespace.clone(),
        name: head.name.clone(),
        id,
        api_version,
        deletion_policy,
        force,
    })
}

fn diff_resources(base: &[SurfaceResource], head: &[SurfaceResource]) -> ResourcesDiff {
    let base_map: BTreeMap<ResourceKey, &SurfaceResource> =
        base.iter().map(|row| (resource_key(row), row)).collect();
    let head_map: BTreeMap<ResourceKey, &SurfaceResource> =
        head.iter().map(|row| (resource_key(row), row)).collect();
    let mut diff = ResourcesDiff::default();
    for key in union_keys(&base_map, &head_map) {
        match (base_map.get(&key), head_map.get(&key)) {
            (None, Some(head)) => diff.added.push((*head).clone()),
            (Some(base), None) => diff.removed.push((*base).clone()),
            (Some(base), Some(head)) => diff.changed.extend(diff_resource(base, head)),
            (None, None) => {}
        }
    }
    diff
}

fn diff_input(base: &SurfaceInput, head: &SurfaceInput) -> Option<InputChange> {
    let kind = value_change(&base.kind, &head.kind);
    let namespace = value_change(&base.namespace, &head.namespace);
    let name = value_change(&base.name, &head.name);
    let mode = value_change(&base.mode, &head.mode);
    let sensitive = value_change(&base.sensitive, &head.sensitive);
    if kind.is_none()
        && namespace.is_none()
        && name.is_none()
        && mode.is_none()
        && sensitive.is_none()
    {
        return None;
    }
    Some(InputChange {
        id: head.id.clone(),
        kind,
        namespace,
        name,
        mode,
        sensitive,
    })
}

fn diff_inputs(base: &[SurfaceInput], head: &[SurfaceInput]) -> InputsDiff {
    let base_map: BTreeMap<&str, &SurfaceInput> =
        base.iter().map(|row| (row.id.as_str(), row)).collect();
    let head_map: BTreeMap<&str, &SurfaceInput> =
        head.iter().map(|row| (row.id.as_str(), row)).collect();
    let mut diff = InputsDiff::default();
    for key in union_keys(&base_map, &head_map) {
        match (base_map.get(key), head_map.get(key)) {
            (None, Some(head)) => diff.added.push((*head).clone()),
            (Some(base), None) => diff.removed.push((*base).clone()),
            (Some(base), Some(head)) => diff.changed.extend(diff_input(base, head)),
            (None, None) => {}
        }
    }
    diff
}

fn diff_reactions(base: &[SurfaceReaction], head: &[SurfaceReaction]) -> ReactionsDiff {
    let base_set: BTreeSet<&SurfaceReaction> = base.iter().collect();
    let head_set: BTreeSet<&SurfaceReaction> = head.iter().collect();
    ReactionsDiff {
        added: head_set
            .difference(&base_set)
            .map(|row| (*row).clone())
            .collect(),
        removed: base_set
            .difference(&head_set)
            .map(|row| (*row).clone())
            .collect(),
    }
}

type ImageKey = (String, String);

fn image_key(image: &SurfaceImage) -> ImageKey {
    (image.resource.clone(), image.container.clone())
}

fn diff_image(base: &SurfaceImage, head: &SurfaceImage) -> Option<ImageChange> {
    let image = value_change(&base.image, &head.image);
    let pinned = value_change(&base.pinned, &head.pinned);
    if image.is_none() && pinned.is_none() {
        return None;
    }
    Some(ImageChange {
        resource: head.resource.clone(),
        container: head.container.clone(),
        image,
        pinned,
        unpinned: !head.pinned,
    })
}

fn diff_images(base: &[SurfaceImage], head: &[SurfaceImage]) -> ImagesDiff {
    let base_map: BTreeMap<ImageKey, &SurfaceImage> =
        base.iter().map(|row| (image_key(row), row)).collect();
    let head_map: BTreeMap<ImageKey, &SurfaceImage> =
        head.iter().map(|row| (image_key(row), row)).collect();
    let mut diff = ImagesDiff::default();
    for key in union_keys(&base_map, &head_map) {
        match (base_map.get(&key), head_map.get(&key)) {
            (None, Some(head)) => diff.added.push((*head).clone()),
            (Some(base), None) => diff.removed.push((*base).clone()),
            (Some(base), Some(head)) => diff.changed.extend(diff_image(base, head)),
            (None, None) => {}
        }
    }
    diff
}

fn diff_string_set(base: &[String], head: &[String]) -> StringSetDiff {
    let base_set: BTreeSet<&str> = base.iter().map(String::as_str).collect();
    let head_set: BTreeSet<&str> = head.iter().map(String::as_str).collect();
    StringSetDiff {
        added: head_set
            .difference(&base_set)
            .map(|value| (*value).to_owned())
            .collect(),
        removed: base_set
            .difference(&head_set)
            .map(|value| (*value).to_owned())
            .collect(),
    }
}

/// Compare two deployment surfaces.
///
/// Pure and deterministic: every section is keyed by a stable identity and
/// sorted, so the result depends only on the two surfaces' content, never on
/// either one's declaration order.
#[must_use]
pub fn diff_surfaces(base: &Surface, head: &Surface) -> SurfaceDiff {
    SurfaceDiff {
        schema_version: SURFACE_DIFF_SCHEMA.into(),
        domain: domain_change(&base.domain, &head.domain),
        kubernetes: kubernetes_change(&base.kubernetes, &head.kubernetes),
        permissions: diff_permissions(&base.permissions, &head.permissions),
        secrets_read: diff_secrets_read(&base.secrets_read, &head.secrets_read),
        resources: diff_resources(&base.resources, &head.resources),
        inputs: diff_inputs(&base.inputs, &head.inputs),
        reactions: diff_reactions(&base.reactions, &head.reactions),
        images: diff_images(&base.images, &head.images),
        risks: diff_string_set(&base.risks, &head.risks),
        enforcement_gaps: diff_string_set(&base.enforcement_gaps, &head.enforcement_gaps),
    }
}
