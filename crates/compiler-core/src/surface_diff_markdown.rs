//! Render a [`SurfaceDiff`] as a concise Markdown PR-comment body.
//!
//! One line per added (`+`), removed (`-`) or changed (`~`) item, grouped by
//! section in the surface's own field order, followed by a `No change: …`
//! line naming every section with no differences. The format is deliberately
//! flat — no per-section subheadings — so the body stays short even for a
//! program with many resources.

use crate::model::{DeletionPolicy, InputMode};
use crate::surface::SurfaceVerb;
use crate::surface_diff::{
    DomainChange, ImagesDiff, InputsDiff, KubernetesChange, PermissionsDiff, ReactionsDiff,
    ResourcesDiff, SecretsReadDiff, StringSetDiff, SurfaceDiff,
};

const fn api_group_display(api_group: &str) -> &str {
    if api_group.is_empty() {
        "core"
    } else {
        api_group
    }
}

fn permission_target(
    namespace: &str,
    api_group: &str,
    resource: &str,
    subresource: Option<&str>,
) -> String {
    let mut target = format!("{namespace}/{}/{resource}", api_group_display(api_group));
    if let Some(subresource) = subresource {
        target.push('/');
        target.push_str(subresource);
    }
    target
}

fn verb_list(verbs: &[SurfaceVerb]) -> String {
    verbs
        .iter()
        .map(|verb| {
            if verb.all_names {
                format!("{}(all)", verb.verb)
            } else {
                verb.verb.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

const fn deletion_policy_str(policy: DeletionPolicy) -> &'static str {
    match policy {
        DeletionPolicy::Orphan => "orphan",
        DeletionPolicy::Delete => "delete",
    }
}

const fn input_mode_str(mode: InputMode) -> &'static str {
    match mode {
        InputMode::Managed => "managed",
        InputMode::Observed => "observed",
    }
}

fn domain_lines(change: &DomainChange) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(id) = &change.id {
        lines.push(format!("~ domain.id: \"{}\" -> \"{}\"", id.base, id.head));
    }
    if let Some(namespace) = &change.namespace {
        lines.push(format!(
            "~ domain.namespace: \"{}\" -> \"{}\"",
            namespace.base, namespace.head
        ));
    }
    if let Some(control_namespace) = &change.control_namespace {
        lines.push(format!(
            "~ domain.controlNamespace: \"{}\" -> \"{}\"",
            control_namespace.base, control_namespace.head
        ));
    }
    lines
}

fn kubernetes_lines(change: &KubernetesChange) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(min_version) = &change.min_version {
        lines.push(format!(
            "~ kubernetes.minVersion: \"{}\" -> \"{}\"",
            min_version.base, min_version.head
        ));
    }
    if let Some(qualified_on) = &change.qualified_on {
        lines.push(format!(
            "~ kubernetes.qualifiedOn: \"{}\" -> \"{}\"",
            qualified_on.base, qualified_on.head
        ));
    }
    lines
}

fn permission_lines(diff: &PermissionsDiff) -> Vec<String> {
    let mut lines = Vec::new();
    for row in &diff.added {
        let target = permission_target(
            &row.namespace,
            &row.api_group,
            &row.resource,
            row.subresource.as_deref(),
        );
        lines.push(format!("+ permissions {target}: {}", verb_list(&row.verbs)));
    }
    for row in &diff.removed {
        let target = permission_target(
            &row.namespace,
            &row.api_group,
            &row.resource,
            row.subresource.as_deref(),
        );
        lines.push(format!("- permissions {target}: {}", verb_list(&row.verbs)));
    }
    for change in &diff.changed {
        let target = permission_target(
            &change.namespace,
            &change.api_group,
            &change.resource,
            change.subresource.as_deref(),
        );
        for verb in &change.verbs_added {
            lines.push(format!("+ permissions {target}: verb {verb}"));
        }
        for verb in &change.verbs_removed {
            lines.push(format!("- permissions {target}: verb {verb}"));
        }
        for verb_change in &change.verbs_changed {
            let mut detail = Vec::new();
            if !verb_change.names_added.is_empty() {
                detail.push(format!("+names[{}]", verb_change.names_added.join(",")));
            }
            if !verb_change.names_removed.is_empty() {
                detail.push(format!("-names[{}]", verb_change.names_removed.join(",")));
            }
            if verb_change.widened_to_all_names {
                detail.push("widened to allNames".to_owned());
            }
            if verb_change.narrowed_from_all_names {
                detail.push("narrowed from allNames".to_owned());
            }
            lines.push(format!(
                "~ permissions {target}: verb {} {}",
                verb_change.verb,
                detail.join(" ")
            ));
        }
    }
    lines
}

fn secret_lines(diff: &SecretsReadDiff) -> Vec<String> {
    let mut lines = Vec::new();
    for row in &diff.added {
        lines.push(format!(
            "+ secretsRead {}/{}: reader {}",
            row.namespace, row.name, row.reader
        ));
    }
    for row in &diff.removed {
        lines.push(format!(
            "- secretsRead {}/{}: reader {}",
            row.namespace, row.name, row.reader
        ));
    }
    for change in &diff.changed {
        lines.push(format!(
            "~ secretsRead {}/{}: reader \"{}\" -> \"{}\"",
            change.namespace, change.name, change.reader.base, change.reader.head
        ));
    }
    lines
}

fn resource_lines(diff: &ResourcesDiff) -> Vec<String> {
    let mut lines = Vec::new();
    for row in &diff.added {
        lines.push(format!(
            "+ resources {} {}/{}: id={}, deletionPolicy={}",
            row.kind,
            row.namespace,
            row.name,
            row.id,
            deletion_policy_str(row.deletion_policy)
        ));
    }
    for row in &diff.removed {
        lines.push(format!(
            "- resources {} {}/{}: id={}, deletionPolicy={}",
            row.kind,
            row.namespace,
            row.name,
            row.id,
            deletion_policy_str(row.deletion_policy)
        ));
    }
    for change in &diff.changed {
        let target = format!("{} {}/{}", change.kind, change.namespace, change.name);
        if let Some(id) = &change.id {
            lines.push(format!(
                "~ resources {target}: id \"{}\" -> \"{}\"",
                id.base, id.head
            ));
        }
        if let Some(api_version) = &change.api_version {
            lines.push(format!(
                "~ resources {target}: apiVersion \"{}\" -> \"{}\"",
                api_version.base, api_version.head
            ));
        }
        if let Some(deletion_policy) = &change.deletion_policy {
            lines.push(format!(
                "~ resources {target}: deletionPolicy {} -> {}",
                deletion_policy_str(deletion_policy.base),
                deletion_policy_str(deletion_policy.head)
            ));
        }
        if let Some(force) = &change.force {
            lines.push(format!(
                "~ resources {target}: force {} -> {}",
                force.base, force.head
            ));
        }
    }
    lines
}

fn input_lines(diff: &InputsDiff) -> Vec<String> {
    let mut lines = Vec::new();
    for row in &diff.added {
        lines.push(format!(
            "+ inputs {}: {} {}/{}, mode={}, sensitive={}",
            row.id,
            row.kind,
            row.namespace,
            row.name,
            input_mode_str(row.mode),
            row.sensitive
        ));
    }
    for row in &diff.removed {
        lines.push(format!(
            "- inputs {}: {} {}/{}, mode={}, sensitive={}",
            row.id,
            row.kind,
            row.namespace,
            row.name,
            input_mode_str(row.mode),
            row.sensitive
        ));
    }
    for change in &diff.changed {
        if let Some(kind) = &change.kind {
            lines.push(format!(
                "~ inputs {}: kind \"{}\" -> \"{}\"",
                change.id, kind.base, kind.head
            ));
        }
        if let Some(namespace) = &change.namespace {
            lines.push(format!(
                "~ inputs {}: namespace \"{}\" -> \"{}\"",
                change.id, namespace.base, namespace.head
            ));
        }
        if let Some(name) = &change.name {
            lines.push(format!(
                "~ inputs {}: name \"{}\" -> \"{}\"",
                change.id, name.base, name.head
            ));
        }
        if let Some(mode) = &change.mode {
            lines.push(format!(
                "~ inputs {}: mode {} -> {}",
                change.id,
                input_mode_str(mode.base),
                input_mode_str(mode.head)
            ));
        }
        if let Some(sensitive) = &change.sensitive {
            lines.push(format!(
                "~ inputs {}: sensitive {} -> {}",
                change.id, sensitive.base, sensitive.head
            ));
        }
    }
    lines
}

fn reaction_lines(diff: &ReactionsDiff) -> Vec<String> {
    let mut lines = Vec::new();
    for row in &diff.added {
        lines.push(format!("+ reactions {} -> {}", row.input, row.target));
    }
    for row in &diff.removed {
        lines.push(format!("- reactions {} -> {}", row.input, row.target));
    }
    lines
}

fn image_lines(diff: &ImagesDiff) -> Vec<String> {
    let mut lines = Vec::new();
    for row in &diff.added {
        let marker = if row.pinned { "" } else { " (unpinned)" };
        lines.push(format!(
            "+ images {}/{}: {}{marker}",
            row.resource, row.container, row.image
        ));
    }
    for row in &diff.removed {
        lines.push(format!(
            "- images {}/{}: {}",
            row.resource, row.container, row.image
        ));
    }
    for change in &diff.changed {
        let mut detail = Vec::new();
        if let Some(image) = &change.image {
            detail.push(format!("image \"{}\" -> \"{}\"", image.base, image.head));
        }
        if let Some(pinned) = &change.pinned {
            detail.push(format!("pinned {} -> {}", pinned.base, pinned.head));
        }
        let marker = if change.unpinned { " (unpinned)" } else { "" };
        lines.push(format!(
            "~ images {}/{}: {}{marker}",
            change.resource,
            change.container,
            detail.join(", ")
        ));
    }
    lines
}

fn string_set_lines(section: &str, diff: &StringSetDiff) -> Vec<String> {
    let mut lines = Vec::new();
    for value in &diff.added {
        lines.push(format!("+ {section}: {value}"));
    }
    for value in &diff.removed {
        lines.push(format!("- {section}: {value}"));
    }
    lines
}

/// Render `diff` as a Markdown PR-comment body.
#[must_use]
pub fn render_markdown(diff: &SurfaceDiff) -> String {
    let sections: [(&str, Vec<String>); 10] = [
        (
            "domain",
            diff.domain.as_ref().map_or_else(Vec::new, domain_lines),
        ),
        (
            "kubernetes",
            diff.kubernetes
                .as_ref()
                .map_or_else(Vec::new, kubernetes_lines),
        ),
        ("permissions", permission_lines(&diff.permissions)),
        ("secretsRead", secret_lines(&diff.secrets_read)),
        ("resources", resource_lines(&diff.resources)),
        ("inputs", input_lines(&diff.inputs)),
        ("reactions", reaction_lines(&diff.reactions)),
        ("images", image_lines(&diff.images)),
        ("risks", string_set_lines("risks", &diff.risks)),
        (
            "enforcementGaps",
            string_set_lines("enforcementGaps", &diff.enforcement_gaps),
        ),
    ];

    let mut body = String::from("# Deployment surface diff\n");
    let mut unchanged = Vec::new();
    for (name, lines) in &sections {
        if lines.is_empty() {
            unchanged.push(*name);
        } else {
            body.push('\n');
            for line in lines {
                body.push_str(line);
                body.push('\n');
            }
        }
    }
    body.push('\n');
    if unchanged.is_empty() {
        body.push_str("No change: (none)\n");
    } else {
        body.push_str("No change: ");
        body.push_str(&unchanged.join(", "));
        body.push('\n');
    }
    body
}
