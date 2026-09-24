//! `render_markdown`: an exact-string check of the PR-comment body for a
//! small, hand-built diff (one added permission target, one removed
//! resource, everything else unchanged).
use compiler_core::*;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn domain() -> SurfaceDomain {
    SurfaceDomain {
        id: "team-api-production".into(),
        namespace: "production".into(),
        control_namespace: "apsides-system".into(),
    }
}

fn kubernetes() -> SurfaceKubernetes {
    SurfaceKubernetes {
        min_version: "1.34".into(),
        qualified_on: "v1.34.3".into(),
    }
}

fn empty_surface() -> Surface {
    Surface {
        schema_version: SURFACE_SCHEMA.into(),
        domain: domain(),
        kubernetes: kubernetes(),
        permissions: Vec::new(),
        secrets_read: Vec::new(),
        resources: Vec::new(),
        inputs: Vec::new(),
        reactions: Vec::new(),
        images: Vec::new(),
        handlers: Vec::new(),
        egress: Vec::new(),
        outputs: Vec::new(),
        risks: Vec::new(),
        enforcement_gaps: Vec::new(),
    }
}

#[test]
fn render_markdown_matches_a_small_diff_exactly() {
    let mut base = empty_surface();
    base.resources = vec![SurfaceResource {
        id: "legacy".into(),
        api_version: "v1".into(),
        kind: "ConfigMap".into(),
        namespace: "production".into(),
        name: "legacy".into(),
        deletion_policy: DeletionPolicy::Orphan,
        force: false,
    }];

    let mut head = empty_surface();
    head.permissions = vec![SurfacePermission {
        namespace: "production".into(),
        api_group: String::new(),
        resource: "secrets".into(),
        subresource: None,
        verbs: vec![SurfaceVerb {
            verb: "get".into(),
            all_names: true,
            names: None,
        }],
    }];

    let diff = diff_surfaces(&base, &head);
    let markdown = render_markdown(&diff);

    assert_eq!(
        markdown,
        "# Deployment surface diff\n\
\n\
```diff\n\
+ permissions production/core/secrets: get(all)\n\
\n\
- resources ConfigMap production/legacy: id=legacy, deletionPolicy=orphan\n\
```\n\
\n\
No change: domain, kubernetes, secretsRead, inputs, reactions, images, handlers, egress, outputs, risks, enforcementGaps\n"
    );
}

/// A container image reference is attacker-controlled free text: an HTML
/// comment that would hide everything after it outside a fence, an
/// `@mention` that would ping someone, a triple-backtick run that would
/// close a naively-sized fence early, an embedded newline that would
/// otherwise read as an extra, independent line, and a right-to-left
/// override that would visually reorder the characters after it without
/// changing a single byte. All five must stay inside the fence, verbatim
/// (the newline and the override escaped), without breaking it — and every
/// section after the hostile one must still render.
#[test]
fn render_markdown_keeps_a_hostile_image_inside_the_fence() -> TestResult {
    let mut base = empty_surface();
    base.images = vec![SurfaceImage {
        resource: "api".into(),
        container: "api".into(),
        image: "nginx@sha256:aaaa".into(),
        pinned: true,
    }];

    let mut head = empty_surface();
    let hostile = "nginx:<!-- hidden --> @team ```evil``` \u{202e}reversed and\ninjected";
    head.images = vec![SurfaceImage {
        resource: "api".into(),
        container: "api".into(),
        image: hostile.into(),
        pinned: false,
    }];
    head.risks = vec!["opaque-custom-resource-effects".into()];

    let diff = diff_surfaces(&base, &head);
    let markdown = render_markdown(&diff);

    // The longest backtick run in the hostile content is 3 (from
    // "```evil```"), so the fence must be 4 backticks — long enough that
    // nothing inside the content can close it early.
    let fence_open = "````diff\n";
    let fence_close = "\n````\n";
    let Some(open_at) = markdown.find(fence_open) else {
        return Err(format!("fence did not open with {fence_open:?}: {markdown}").into());
    };
    let content_start = open_at + fence_open.len();
    let Some(close_at) = markdown[content_start..].find(fence_close) else {
        return Err(format!("fence did not close with {fence_close:?}: {markdown}").into());
    };
    let close_at = content_start + close_at;
    let (before, content, after) = (
        &markdown[..open_at],
        &markdown[content_start..close_at],
        &markdown[close_at + fence_close.len()..],
    );

    // The hostile substrings and the section after images are inside the
    // fence, verbatim (bar the escaped newline).
    assert!(content.contains("<!-- hidden -->"));
    assert!(content.contains("@team"));
    assert!(content.contains("```evil```"));
    assert!(content.contains("and\\ninjected"), "{content}");
    assert!(!content.contains("and\ninjected"), "{content}");
    assert!(content.contains("\\u{202e}reversed"), "{content}");
    assert!(!content.contains('\u{202e}'), "{content}");
    assert!(content.contains("+ risks: opaque-custom-resource-effects"));

    // Nothing outside the fence — the heading and the "No change" line —
    // carries any surface-controlled text.
    assert_eq!(before, "# Deployment surface diff\n\n");
    assert!(!before.contains("@team") && !before.contains("<!--"));
    assert!(!after.contains("@team") && !after.contains("<!--"));
    assert_eq!(
        after,
        "\nNo change: domain, kubernetes, permissions, secretsRead, resources, inputs, reactions, handlers, egress, outputs, enforcementGaps\n"
    );
    Ok(())
}
