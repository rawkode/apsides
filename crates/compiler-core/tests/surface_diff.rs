//! `surface_diff.rs`: comparing two already-derived deployment surfaces.
//!
//! Fixtures build `Surface` values directly (every field is `pub`) rather
//! than compiling IR, since `diff_surfaces` is pure over `Surface` and never
//! looks at a `CompiledPlan`.
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

fn verb(name: &str, names: &[&str]) -> SurfaceVerb {
    SurfaceVerb {
        verb: name.into(),
        all_names: false,
        names: Some(names.iter().map(|n| (*n).to_owned()).collect()),
    }
}

fn verb_all(name: &str) -> SurfaceVerb {
    SurfaceVerb {
        verb: name.into(),
        all_names: true,
        names: None,
    }
}

fn permission(
    namespace: &str,
    api_group: &str,
    resource: &str,
    verbs: Vec<SurfaceVerb>,
) -> SurfacePermission {
    SurfacePermission {
        namespace: namespace.into(),
        api_group: api_group.into(),
        resource: resource.into(),
        subresource: None,
        verbs,
    }
}

fn resource(id: &str, kind: &str, name: &str) -> SurfaceResource {
    resource_with_api_version(id, "apps/v1", kind, name)
}

fn resource_with_api_version(
    id: &str,
    api_version: &str,
    kind: &str,
    name: &str,
) -> SurfaceResource {
    SurfaceResource {
        id: id.into(),
        api_version: api_version.into(),
        kind: kind.into(),
        namespace: "production".into(),
        name: name.into(),
        deletion_policy: DeletionPolicy::Orphan,
        force: false,
    }
}

#[test]
fn diff_no_changes_is_empty() {
    let mut surface = empty_surface();
    surface.permissions = vec![permission(
        "production",
        "",
        "configmaps",
        vec![verb_all("create")],
    )];
    surface.resources = vec![resource("api", "Deployment", "api")];
    surface.risks = vec!["opaque-custom-resource-effects".into()];

    let diff = diff_surfaces(&surface, &surface.clone());

    assert!(diff.domain.is_none());
    assert!(diff.kubernetes.is_none());
    assert!(diff.permissions.is_empty());
    assert!(diff.secrets_read.is_empty());
    assert!(diff.resources.is_empty());
    assert!(diff.inputs.is_empty());
    assert!(diff.reactions.is_empty());
    assert!(diff.images.is_empty());
    assert!(diff.risks.is_empty());
    assert!(diff.enforcement_gaps.is_empty());
}

#[test]
fn diff_detects_added_permission() {
    let base = empty_surface();
    let mut head = empty_surface();
    head.permissions = vec![permission(
        "production",
        "",
        "configmaps",
        vec![verb("get", &["api-config"])],
    )];

    let diff = diff_surfaces(&base, &head);

    assert_eq!(diff.permissions.added, head.permissions);
    assert!(diff.permissions.removed.is_empty());
    assert!(diff.permissions.changed.is_empty());
}

#[test]
fn diff_detects_removed_resource() {
    let mut base = empty_surface();
    base.resources = vec![resource("legacy-api", "Deployment", "legacy-api")];
    let head = empty_surface();

    let diff = diff_surfaces(&base, &head);

    assert_eq!(diff.resources.removed, base.resources);
    assert!(diff.resources.added.is_empty());
    assert!(diff.resources.changed.is_empty());
}

#[test]
fn diff_does_not_collide_same_kind_resources_across_api_groups() {
    // Two custom resources can share a Kind and name across different API
    // groups: an Istio `Gateway` and a `gateway.networking.k8s.io` `Gateway`
    // both named "web". Keying resources by (kind, namespace, name) alone
    // would collapse them onto one map entry and drop whichever one lost the
    // collision; apiGroup (parsed from apiVersion) must be part of the key.
    let istio_gateway = resource_with_api_version(
        "istio-web-gateway",
        "networking.istio.io/v1",
        "Gateway",
        "web",
    );
    let k8s_gateway = resource_with_api_version(
        "k8s-web-gateway",
        "gateway.networking.k8s.io/v1",
        "Gateway",
        "web",
    );

    let mut base = empty_surface();
    base.resources = vec![istio_gateway.clone(), k8s_gateway.clone()];
    let mut head = empty_surface();
    head.resources = vec![k8s_gateway];

    let diff = diff_surfaces(&base, &head);

    assert_eq!(diff.resources.removed, vec![istio_gateway]);
    assert!(diff.resources.added.is_empty());
    assert!(diff.resources.changed.is_empty());
}

#[test]
fn diff_reports_a_version_bump_within_the_same_group_as_changed() -> TestResult {
    let mut base = empty_surface();
    base.resources = vec![resource_with_api_version(
        "cluster-db",
        "postgresql.cnpg.io/v1",
        "Cluster",
        "db",
    )];
    let mut head = empty_surface();
    head.resources = vec![resource_with_api_version(
        "cluster-db",
        "postgresql.cnpg.io/v1beta1",
        "Cluster",
        "db",
    )];

    let diff = diff_surfaces(&base, &head);

    assert!(diff.resources.added.is_empty());
    assert!(diff.resources.removed.is_empty());
    assert_eq!(diff.resources.changed.len(), 1);
    let change = &diff.resources.changed[0];
    let Some(api_version) = &change.api_version else {
        return Err("no apiVersion change recorded".into());
    };
    assert_eq!(api_version.base, "postgresql.cnpg.io/v1");
    assert_eq!(api_version.head, "postgresql.cnpg.io/v1beta1");
    Ok(())
}

#[test]
fn diff_changed_verbs() -> TestResult {
    let mut base = empty_surface();
    base.permissions = vec![permission(
        "production",
        "",
        "configmaps",
        vec![
            verb("get", &["a", "b"]),
            verb("list", &["x"]),
            verb("watch", &["x"]),
        ],
    )];
    let mut head = empty_surface();
    head.permissions = vec![permission(
        "production",
        "",
        "configmaps",
        vec![
            // Widened from a name-restricted list to every name.
            verb_all("get"),
            // A name added to an already name-restricted verb.
            verb("list", &["x", "y"]),
            // A brand-new verb on the same, already-present target.
            verb_all("create"),
            // `watch` dropped entirely.
        ],
    )];

    let diff = diff_surfaces(&base, &head);

    assert!(diff.permissions.added.is_empty());
    assert!(diff.permissions.removed.is_empty());
    assert_eq!(diff.permissions.changed.len(), 1);
    let change = &diff.permissions.changed[0];
    assert_eq!(change.resource, "configmaps");
    assert_eq!(change.verbs_added, vec!["create".to_owned()]);
    assert_eq!(change.verbs_removed, vec!["watch".to_owned()]);
    assert_eq!(change.verbs_changed.len(), 2);

    let Some(get) = change.verbs_changed.iter().find(|v| v.verb == "get") else {
        return Err("no verb change recorded for get".into());
    };
    assert!(get.widened_to_all_names);
    assert!(!get.narrowed_from_all_names);
    // A widening never restates the superseded names as "removed": the
    // dedicated flag already says everything a reviewer needs.
    assert!(get.names_added.is_empty());
    assert!(get.names_removed.is_empty());

    let Some(list) = change.verbs_changed.iter().find(|v| v.verb == "list") else {
        return Err("no verb change recorded for list".into());
    };
    assert!(!list.widened_to_all_names);
    assert_eq!(list.names_added, vec!["y".to_owned()]);
    assert!(list.names_removed.is_empty());
    Ok(())
}

#[test]
fn diff_detects_added_secret_and_a_second_reader_on_a_kept_secret() {
    // secretsRead is keyed by (namespace, name, reader): a reader is part of
    // the identity, so a second reader appearing on an already-read secret
    // is a distinct row (`added`), never an in-place `changed` edit a
    // reviewer could miss by only reading a diff of unique targets.
    let mut base = empty_surface();
    base.secrets_read = vec![SurfaceSecretRead {
        namespace: "production".into(),
        name: "database-credentials".into(),
        reader: "reconcile".into(),
    }];
    let mut head = empty_surface();
    head.secrets_read = vec![
        SurfaceSecretRead {
            namespace: "production".into(),
            name: "database-credentials".into(),
            reader: "reconcile".into(),
        },
        SurfaceSecretRead {
            namespace: "production".into(),
            name: "database-credentials".into(),
            reader: "sync-flags".into(),
        },
        SurfaceSecretRead {
            namespace: "production".into(),
            name: "new-credentials".into(),
            reader: "reconcile".into(),
        },
    ];

    let diff = diff_surfaces(&base, &head);

    assert!(diff.secrets_read.removed.is_empty());
    let mut added_names: Vec<&str> = diff
        .secrets_read
        .added
        .iter()
        .map(|row| row.name.as_str())
        .collect();
    added_names.sort_unstable();
    assert_eq!(added_names, vec!["database-credentials", "new-credentials"]);
    assert!(
        diff.secrets_read
            .added
            .iter()
            .any(|row| row.name == "database-credentials" && row.reader == "sync-flags")
    );
}

#[test]
fn diff_treats_a_changed_reader_as_remove_and_add() {
    // Swapping a secret's only reader is not an in-place "changed" edit
    // either: the old (namespace, name, reader) row disappears and a new
    // one appears.
    let mut base = empty_surface();
    base.secrets_read = vec![SurfaceSecretRead {
        namespace: "production".into(),
        name: "database-credentials".into(),
        reader: "reconcile".into(),
    }];
    let mut head = empty_surface();
    head.secrets_read = vec![SurfaceSecretRead {
        namespace: "production".into(),
        name: "database-credentials".into(),
        reader: "sync-flags".into(),
    }];

    let diff = diff_surfaces(&base, &head);

    assert_eq!(diff.secrets_read.added, head.secrets_read);
    assert_eq!(diff.secrets_read.removed, base.secrets_read);
}

#[test]
fn diff_detects_resource_field_changes() -> TestResult {
    let mut base = empty_surface();
    base.resources = vec![resource("api", "Deployment", "api")];
    let mut head = empty_surface();
    let mut changed = resource("api", "Deployment", "api");
    changed.deletion_policy = DeletionPolicy::Delete;
    changed.force = true;
    head.resources = vec![changed];

    let diff = diff_surfaces(&base, &head);

    assert!(diff.resources.added.is_empty());
    assert!(diff.resources.removed.is_empty());
    assert_eq!(diff.resources.changed.len(), 1);
    let change = &diff.resources.changed[0];
    assert!(change.id.is_none());
    assert!(change.api_version.is_none());
    let Some(deletion_policy) = change.deletion_policy.as_ref() else {
        return Err("no deletionPolicy change recorded".into());
    };
    assert_eq!(deletion_policy.base, DeletionPolicy::Orphan);
    assert_eq!(deletion_policy.head, DeletionPolicy::Delete);
    let Some(force) = change.force.as_ref() else {
        return Err("no force change recorded".into());
    };
    assert!(!force.base);
    assert!(force.head);
    Ok(())
}

#[test]
fn diff_images_flags_unpinned_head() -> TestResult {
    let mut base = empty_surface();
    base.images = vec![SurfaceImage {
        resource: "api".into(),
        container: "api".into(),
        image: "nginx@sha256:aaaa".into(),
        pinned: true,
    }];
    let mut head = empty_surface();
    head.images = vec![SurfaceImage {
        resource: "api".into(),
        container: "api".into(),
        image: "nginx:latest".into(),
        pinned: false,
    }];

    let diff = diff_surfaces(&base, &head);

    assert_eq!(diff.images.changed.len(), 1);
    let change = &diff.images.changed[0];
    assert!(change.unpinned);
    let Some(image) = change.image.as_ref() else {
        return Err("no image change recorded".into());
    };
    assert_eq!(image.head, "nginx:latest");
    let Some(pinned) = change.pinned.as_ref() else {
        return Err("no pinned change recorded".into());
    };
    assert!(!pinned.head);
    Ok(())
}

#[test]
fn diff_treats_risks_and_reactions_as_sets() {
    let mut base = empty_surface();
    base.risks = vec!["opaque-custom-resource-effects".into()];
    base.reactions = vec![SurfaceReaction {
        input: "settings".into(),
        target: "api".into(),
    }];
    let mut head = empty_surface();
    head.risks = vec!["broad-permission-ceiling".into()];
    head.reactions = vec![
        SurfaceReaction {
            input: "settings".into(),
            target: "api".into(),
        },
        SurfaceReaction {
            input: "credentials".into(),
            target: "api".into(),
        },
    ];

    let diff = diff_surfaces(&base, &head);

    assert_eq!(
        diff.risks.added,
        vec!["broad-permission-ceiling".to_owned()]
    );
    assert_eq!(
        diff.risks.removed,
        vec!["opaque-custom-resource-effects".to_owned()]
    );
    assert_eq!(
        diff.reactions.added,
        vec![SurfaceReaction {
            input: "credentials".into(),
            target: "api".into(),
        }]
    );
    assert!(diff.reactions.removed.is_empty());
}

#[test]
fn diff_detects_kubernetes_and_domain_changes() -> TestResult {
    let base = empty_surface();
    let mut head = empty_surface();
    head.kubernetes.min_version = "1.35".into();
    head.domain.namespace = "staging".into();

    let diff = diff_surfaces(&base, &head);

    let Some(kubernetes) = diff.kubernetes else {
        return Err("no kubernetes change recorded".into());
    };
    let Some(min_version) = kubernetes.min_version else {
        return Err("no minVersion change recorded".into());
    };
    assert_eq!(min_version.head, "1.35");
    assert!(kubernetes.qualified_on.is_none());
    let Some(domain) = diff.domain else {
        return Err("no domain change recorded".into());
    };
    let Some(namespace) = domain.namespace else {
        return Err("no namespace change recorded".into());
    };
    assert_eq!(namespace.head, "staging");
    assert!(domain.id.is_none());
    Ok(())
}

fn handler(id: &str, trigger: &str, inputs: &[&str], outputs: &[&str]) -> SurfaceHandler {
    SurfaceHandler {
        id: id.into(),
        trigger: trigger.into(),
        inputs: inputs.iter().map(|s| (*s).to_owned()).collect(),
        outputs: outputs.iter().map(|s| (*s).to_owned()).collect(),
    }
}

#[test]
fn diff_detects_added_handler() {
    let base = empty_surface();
    let mut head = empty_surface();
    head.handlers = vec![handler("sync-flags", "settings", &["settings"], &["flags"])];

    let diff = diff_surfaces(&base, &head);

    assert_eq!(diff.handlers.added, head.handlers);
    assert!(diff.handlers.removed.is_empty());
    assert!(diff.handlers.changed.is_empty());
}

#[test]
fn diff_detects_removed_handler() {
    let mut base = empty_surface();
    base.handlers = vec![handler("sync-flags", "settings", &["settings"], &["flags"])];
    let head = empty_surface();

    let diff = diff_surfaces(&base, &head);

    assert_eq!(diff.handlers.removed, base.handlers);
    assert!(diff.handlers.added.is_empty());
}

#[test]
fn diff_detects_changed_handler_trigger_and_edges() -> TestResult {
    let mut base = empty_surface();
    base.handlers = vec![handler("sync-flags", "settings", &["settings"], &["flags"])];
    let mut head = empty_surface();
    head.handlers = vec![handler(
        "sync-flags",
        "other-settings",
        &["settings", "other-settings"],
        &[],
    )];

    let diff = diff_surfaces(&base, &head);

    assert_eq!(diff.handlers.added.len(), 0);
    assert_eq!(diff.handlers.removed.len(), 0);
    let Some(change) = diff.handlers.changed.first() else {
        return Err("no handler change recorded".into());
    };
    let Some(trigger) = change.trigger.as_ref() else {
        return Err("no trigger change recorded".into());
    };
    assert_eq!(trigger.base, "settings");
    assert_eq!(trigger.head, "other-settings");
    assert_eq!(change.inputs_added, vec!["other-settings".to_owned()]);
    assert!(change.inputs_removed.is_empty());
    assert_eq!(change.outputs_removed, vec!["flags".to_owned()]);
    assert!(change.outputs_added.is_empty());
    Ok(())
}

#[test]
fn diff_treats_egress_as_a_set() {
    let mut base = empty_surface();
    base.egress = vec![SurfaceEgress {
        host: "api.example.com".into(),
        port: 443,
        handler: "sync-flags".into(),
    }];
    let mut head = empty_surface();
    head.egress = vec![
        SurfaceEgress {
            host: "api.example.com".into(),
            port: 443,
            handler: "sync-flags".into(),
        },
        SurfaceEgress {
            host: "other.example.com".into(),
            port: 443,
            handler: "sync-flags".into(),
        },
    ];

    let diff = diff_surfaces(&base, &head);

    assert_eq!(
        diff.egress.added,
        vec![SurfaceEgress {
            host: "other.example.com".into(),
            port: 443,
            handler: "sync-flags".into(),
        }]
    );
    assert!(diff.egress.removed.is_empty());
}

#[test]
fn diff_detects_added_and_changed_output() -> TestResult {
    let mut base = empty_surface();
    base.outputs = vec![SurfaceOutput {
        id: "flags".into(),
        name: "flags".into(),
        namespace: "production".into(),
    }];
    let mut head = empty_surface();
    head.outputs = vec![
        SurfaceOutput {
            id: "flags".into(),
            name: "flags-renamed".into(),
            namespace: "production".into(),
        },
        SurfaceOutput {
            id: "other-output".into(),
            name: "other-output".into(),
            namespace: "production".into(),
        },
    ];

    let diff = diff_surfaces(&base, &head);

    assert_eq!(diff.outputs.added.len(), 1);
    assert_eq!(diff.outputs.added[0].id, "other-output");
    let Some(change) = diff.outputs.changed.first() else {
        return Err("no output change recorded".into());
    };
    let Some(name) = change.name.as_ref() else {
        return Err("no name change recorded".into());
    };
    assert_eq!(name.base, "flags");
    assert_eq!(name.head, "flags-renamed");
    Ok(())
}
