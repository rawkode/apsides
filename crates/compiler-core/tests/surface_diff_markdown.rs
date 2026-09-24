//! `render_markdown`: an exact-string check of the PR-comment body for a
//! small, hand-built diff (one added permission target, one removed
//! resource, everything else unchanged).
use compiler_core::*;

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
+ permissions production/core/secrets: get(all)\n\
\n\
- resources ConfigMap production/legacy: id=legacy, deletionPolicy=orphan\n\
\n\
No change: domain, kubernetes, secretsRead, inputs, reactions, images, risks, enforcementGaps\n"
    );
}
