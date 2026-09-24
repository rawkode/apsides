//! Render a [`SurfaceDiff`] as a concise Markdown PR-comment body.
//!
//! One line per added (`+`), removed (`-`) or changed (`!`) item, grouped by
//! section in the surface's own field order, followed by a `No change: …`
//! line naming every section with no differences. The format is deliberately
//! flat — no per-section subheadings — so the body stays short even for a
//! program with many resources.
//!
//! Every value on a change line ultimately comes from an authored program —
//! a container image reference, a custom resource's namespace or name, a
//! secret's reader — so it is attacker-controlled free text, not a value
//! this renderer can trust. Two defenses apply:
//!
//! - Every change line lives inside one fenced ` ```diff ` block, whose fence
//!   is sized one backtick longer than the longest run of backticks
//!   anywhere in the content, so a value cannot close the fence early and
//!   pull the rest of the document into a false code span. Everything
//!   outside the fence — the title and the `No change: …` line — is built
//!   only from this module's own fixed section names, never from surface
//!   content, so there is nothing outside the fence left to exploit even if
//!   the fence itself were somehow defeated: no bare Markdown `+`/`-`/`~`
//!   prose for GitHub to turn into bullets or fold together, no `<!--`
//!   comment, `@mention` or `[text](url)` markup for it to render, since
//!   none of that is interpreted inside a fenced code block regardless of
//!   its content.
//! - Every value is passed through [`esc`], which escapes CR and LF, so a
//!   value cannot inject what looks like an extra, independent line into
//!   the fenced block.
use crate::model::{DeletionPolicy, InputMode};
use crate::surface::SurfaceVerb;
use crate::surface_diff::{
    DomainChange, ImagesDiff, InputsDiff, KubernetesChange, PermissionsDiff, ReactionsDiff,
    ResourcesDiff, SecretsReadDiff, StringSetDiff, SurfaceDiff,
};

/// Escape a surface value before it goes on a fenced diff line: backslash
/// first (so the escapes this inserts are never themselves ambiguous with a
/// literal backslash in the source value), then CR and LF, so an embedded
/// newline can never read as a second, independent line.
fn esc(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

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
    let escaped_group = esc(api_group);
    let mut target = format!(
        "{}/{}/{}",
        esc(namespace),
        api_group_display(&escaped_group),
        esc(resource)
    );
    if let Some(subresource) = subresource {
        target.push('/');
        target.push_str(&esc(subresource));
    }
    target
}

fn verb_list(verbs: &[SurfaceVerb]) -> String {
    verbs
        .iter()
        .map(|verb| {
            if verb.all_names {
                format!("{}(all)", esc(&verb.verb))
            } else {
                esc(&verb.verb)
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn name_list(names: &[String]) -> String {
    names
        .iter()
        .map(|name| esc(name))
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
        lines.push(format!(
            "! domain.id: \"{}\" -> \"{}\"",
            esc(&id.base),
            esc(&id.head)
        ));
    }
    if let Some(namespace) = &change.namespace {
        lines.push(format!(
            "! domain.namespace: \"{}\" -> \"{}\"",
            esc(&namespace.base),
            esc(&namespace.head)
        ));
    }
    if let Some(control_namespace) = &change.control_namespace {
        lines.push(format!(
            "! domain.controlNamespace: \"{}\" -> \"{}\"",
            esc(&control_namespace.base),
            esc(&control_namespace.head)
        ));
    }
    lines
}

fn kubernetes_lines(change: &KubernetesChange) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(min_version) = &change.min_version {
        lines.push(format!(
            "! kubernetes.minVersion: \"{}\" -> \"{}\"",
            esc(&min_version.base),
            esc(&min_version.head)
        ));
    }
    if let Some(qualified_on) = &change.qualified_on {
        lines.push(format!(
            "! kubernetes.qualifiedOn: \"{}\" -> \"{}\"",
            esc(&qualified_on.base),
            esc(&qualified_on.head)
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
            lines.push(format!("+ permissions {target}: verb {}", esc(verb)));
        }
        for verb in &change.verbs_removed {
            lines.push(format!("- permissions {target}: verb {}", esc(verb)));
        }
        for verb_change in &change.verbs_changed {
            let mut detail = Vec::new();
            if !verb_change.names_added.is_empty() {
                detail.push(format!("+names[{}]", name_list(&verb_change.names_added)));
            }
            if !verb_change.names_removed.is_empty() {
                detail.push(format!("-names[{}]", name_list(&verb_change.names_removed)));
            }
            if verb_change.widened_to_all_names {
                detail.push("widened to allNames".to_owned());
            }
            if verb_change.narrowed_from_all_names {
                detail.push("narrowed from allNames".to_owned());
            }
            lines.push(format!(
                "! permissions {target}: verb {} {}",
                esc(&verb_change.verb),
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
            esc(&row.namespace),
            esc(&row.name),
            esc(&row.reader)
        ));
    }
    for row in &diff.removed {
        lines.push(format!(
            "- secretsRead {}/{}: reader {}",
            esc(&row.namespace),
            esc(&row.name),
            esc(&row.reader)
        ));
    }
    lines
}

fn resource_lines(diff: &ResourcesDiff) -> Vec<String> {
    let mut lines = Vec::new();
    for row in &diff.added {
        lines.push(format!(
            "+ resources {} {}/{}: id={}, deletionPolicy={}",
            esc(&row.kind),
            esc(&row.namespace),
            esc(&row.name),
            esc(&row.id),
            deletion_policy_str(row.deletion_policy)
        ));
    }
    for row in &diff.removed {
        lines.push(format!(
            "- resources {} {}/{}: id={}, deletionPolicy={}",
            esc(&row.kind),
            esc(&row.namespace),
            esc(&row.name),
            esc(&row.id),
            deletion_policy_str(row.deletion_policy)
        ));
    }
    for change in &diff.changed {
        let target = format!(
            "{} {}/{}",
            esc(&change.kind),
            esc(&change.namespace),
            esc(&change.name)
        );
        if let Some(id) = &change.id {
            lines.push(format!(
                "! resources {target}: id \"{}\" -> \"{}\"",
                esc(&id.base),
                esc(&id.head)
            ));
        }
        if let Some(api_version) = &change.api_version {
            lines.push(format!(
                "! resources {target}: apiVersion \"{}\" -> \"{}\"",
                esc(&api_version.base),
                esc(&api_version.head)
            ));
        }
        if let Some(deletion_policy) = &change.deletion_policy {
            lines.push(format!(
                "! resources {target}: deletionPolicy {} -> {}",
                deletion_policy_str(deletion_policy.base),
                deletion_policy_str(deletion_policy.head)
            ));
        }
        if let Some(force) = &change.force {
            lines.push(format!(
                "! resources {target}: force {} -> {}",
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
            esc(&row.id),
            esc(&row.kind),
            esc(&row.namespace),
            esc(&row.name),
            input_mode_str(row.mode),
            row.sensitive
        ));
    }
    for row in &diff.removed {
        lines.push(format!(
            "- inputs {}: {} {}/{}, mode={}, sensitive={}",
            esc(&row.id),
            esc(&row.kind),
            esc(&row.namespace),
            esc(&row.name),
            input_mode_str(row.mode),
            row.sensitive
        ));
    }
    for change in &diff.changed {
        let id = esc(&change.id);
        if let Some(kind) = &change.kind {
            lines.push(format!(
                "! inputs {id}: kind \"{}\" -> \"{}\"",
                esc(&kind.base),
                esc(&kind.head)
            ));
        }
        if let Some(namespace) = &change.namespace {
            lines.push(format!(
                "! inputs {id}: namespace \"{}\" -> \"{}\"",
                esc(&namespace.base),
                esc(&namespace.head)
            ));
        }
        if let Some(name) = &change.name {
            lines.push(format!(
                "! inputs {id}: name \"{}\" -> \"{}\"",
                esc(&name.base),
                esc(&name.head)
            ));
        }
        if let Some(mode) = &change.mode {
            lines.push(format!(
                "! inputs {id}: mode {} -> {}",
                input_mode_str(mode.base),
                input_mode_str(mode.head)
            ));
        }
        if let Some(sensitive) = &change.sensitive {
            lines.push(format!(
                "! inputs {id}: sensitive {} -> {}",
                sensitive.base, sensitive.head
            ));
        }
    }
    lines
}

fn reaction_lines(diff: &ReactionsDiff) -> Vec<String> {
    let mut lines = Vec::new();
    for row in &diff.added {
        lines.push(format!(
            "+ reactions {} -> {}",
            esc(&row.input),
            esc(&row.target)
        ));
    }
    for row in &diff.removed {
        lines.push(format!(
            "- reactions {} -> {}",
            esc(&row.input),
            esc(&row.target)
        ));
    }
    lines
}

fn image_lines(diff: &ImagesDiff) -> Vec<String> {
    let mut lines = Vec::new();
    for row in &diff.added {
        let marker = if row.pinned { "" } else { " (unpinned)" };
        lines.push(format!(
            "+ images {}/{}: {}{marker}",
            esc(&row.resource),
            esc(&row.container),
            esc(&row.image)
        ));
    }
    for row in &diff.removed {
        lines.push(format!(
            "- images {}/{}: {}",
            esc(&row.resource),
            esc(&row.container),
            esc(&row.image)
        ));
    }
    for change in &diff.changed {
        let mut detail = Vec::new();
        if let Some(image) = &change.image {
            detail.push(format!(
                "image \"{}\" -> \"{}\"",
                esc(&image.base),
                esc(&image.head)
            ));
        }
        if let Some(pinned) = &change.pinned {
            detail.push(format!("pinned {} -> {}", pinned.base, pinned.head));
        }
        let marker = if change.unpinned { " (unpinned)" } else { "" };
        lines.push(format!(
            "! images {}/{}: {}{marker}",
            esc(&change.resource),
            esc(&change.container),
            detail.join(", ")
        ));
    }
    lines
}

fn string_set_lines(section: &str, diff: &StringSetDiff) -> Vec<String> {
    let mut lines = Vec::new();
    for value in &diff.added {
        lines.push(format!("+ {section}: {}", esc(value)));
    }
    for value in &diff.removed {
        lines.push(format!("- {section}: {}", esc(value)));
    }
    lines
}

/// The number of backticks a fence needs to safely wrap `content`: one more
/// than the longest run of consecutive backticks anywhere in it, and never
/// fewer than three (Markdown's minimum fence length).
fn fence_length(content: &str) -> usize {
    let mut longest = 0usize;
    let mut current = 0usize;
    for byte in content.bytes() {
        if byte == b'`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    (longest + 1).max(3)
}

/// Render `diff` as a Markdown PR-comment body.
///
/// Every value drawn from `diff` — an image reference, a resource or secret
/// name, anything an authored program controls — lands inside a fenced
/// ` ```diff ` block; see the module documentation for why.
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

    let mut content = String::new();
    let mut unchanged = Vec::new();
    for (name, lines) in &sections {
        if lines.is_empty() {
            unchanged.push(*name);
        } else {
            if !content.is_empty() {
                content.push('\n');
            }
            for line in lines {
                content.push_str(line);
                content.push('\n');
            }
        }
    }

    // Everything from here down is built only from this module's own fixed
    // strings, never from `diff` content: the title, the fence markers and
    // the `No change: …` section-name list.
    let mut body = String::from("# Deployment surface diff\n");
    if !content.is_empty() {
        let fence = "`".repeat(fence_length(&content));
        body.push('\n');
        body.push_str(&fence);
        body.push_str("diff\n");
        body.push_str(&content);
        body.push_str(&fence);
        body.push('\n');
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
