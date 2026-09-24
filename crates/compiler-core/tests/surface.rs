//! `surface.rs`: the reviewable deployment surface derived from a compiled
//! plan.
//!
//! The fixture below mirrors `examples/web-application` (a `ConfigMap`, a
//! `Deployment` reading it plus an external `ConfigMap` and `Secret`, and a
//! `Service`), hand-authored as IR the way `compiler-core`'s other contract
//! tests build fixtures, since this crate compiles finished IR and never
//! runs the TSX/Deno authoring compiler itself.
use compiler_core::*;
use serde_json::{Value, json};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn source() -> Value {
    json!({"file":"controller.ts","line":1,"column":1})
}

const IMAGE: &str = "docker.io/nginxinc/nginx-unprivileged@sha256:9b87ad3dd9f431c733f19dfb278c7eb3dba9dca381942c79818bb42f1a566a83";

/// A web-application-shaped IR: `resources` and `inputs` are supplied by the
/// caller so determinism can be exercised by reordering them.
fn fixture(resources: &Value, inputs: &Value, reaction_inputs: &Value) -> Value {
    json!({
        "schemaVersion":IR_SCHEMA,
        "domain":{"id":"team-api-production","namespace":"production","controlNamespace":"apsides-system","clusterBinding":"local-fixture","policyRef":"platform/team-api","enforcement":"api-bound","budgets":{"maxResources":64,"maxInputs":64,"maxReactions":64,"maxObjectBytes":262_144,"maxQueueDepth":128,"maxConcurrentRequests":4}},
        "resources":resources,
        "inputs":inputs,
        "reactions":[{"id":"restart","op":"rollout","inputs":reaction_inputs,"target":"api","source":source()}],
        "provenance":{"compiler":"apsides/0.1","inputs":[{"path":"controller.ts","sha256":"0000000000000000000000000000000000000000000000000000000000000000"}]}
    })
}

fn deployment() -> Value {
    json!({"id":"api","apiVersion":"apps/v1","kind":"Deployment","name":"api","namespace":"production","desired":{
        "apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"api","namespace":"production"},
        "spec":{"replicas":3,"selector":{"matchLabels":{"app":"api"}},"template":{"metadata":{"labels":{"app":"api"}},"spec":{
            "serviceAccountName":"api-workload",
            "containers":[{"name":"api","image":IMAGE,"ports":[{"containerPort":8080}],
                "resources":{"requests":{"cpu":"10m","memory":"16Mi"},"limits":{"memory":"64Mi"}},
                "envFrom":[{"configMapRef":{"name":"api-config"}},{"configMapRef":{"name":"external-settings"}},{"secretRef":{"name":"database-credentials"}}]
            }]
        }}}
    },"lifecycle":{"deletionPolicy":"orphan"},"source":source()})
}

fn config_map() -> Value {
    json!({"id":"api-config","apiVersion":"v1","kind":"ConfigMap","name":"api-config","namespace":"production","desired":{
        "apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"api-config","namespace":"production"},"data":{"LOG_LEVEL":"info"}
    },"lifecycle":{"deletionPolicy":"orphan"},"source":source()})
}

fn service() -> Value {
    json!({"id":"api-service","apiVersion":"v1","kind":"Service","name":"api","namespace":"production","desired":{
        "apiVersion":"v1","kind":"Service","metadata":{"name":"api","namespace":"production"},
        "spec":{"selector":{"app":"api"},"ports":[{"port":80,"targetPort":8080}]}
    },"lifecycle":{"deletionPolicy":"orphan"},"source":source()})
}

fn settings_input() -> Value {
    json!({"id":"settings","apiVersion":"v1","kind":"ConfigMap","name":"external-settings","namespace":"production","mode":"observed","projection":["data"],"sensitive":false,"missing":"block-dependent-mutations","source":source()})
}

fn credentials_input() -> Value {
    json!({"id":"credentials","apiVersion":"v1","kind":"Secret","name":"database-credentials","namespace":"production","mode":"observed","projection":["data"],"sensitive":true,"missing":"block-dependent-mutations","source":source()})
}

fn web_application() -> Value {
    fixture(
        &json!([deployment(), config_map(), service()]),
        &json!([settings_input(), credentials_input()]),
        &json!(["settings", "credentials"]),
    )
}

fn plan(value: &Value) -> TestResult<CompiledPlan> {
    Ok(parse_and_validate(&value.to_string())?)
}

fn assert_domain_resources_and_inputs(surface: &Surface) {
    assert_eq!(surface.schema_version, SURFACE_SCHEMA);
    assert_eq!(
        surface.domain,
        SurfaceDomain {
            id: "team-api-production".into(),
            namespace: "production".into(),
            control_namespace: "apsides-system".into(),
        }
    );
    assert_eq!(
        surface.kubernetes,
        SurfaceKubernetes {
            min_version: "1.34".into(),
            qualified_on: "v1.34.3".into(),
        }
    );

    // Resources, sorted by id ("api" < "api-config" < "api-service").
    assert_eq!(
        surface.resources,
        vec![
            SurfaceResource {
                id: "api".into(),
                api_version: "apps/v1".into(),
                kind: "Deployment".into(),
                namespace: "production".into(),
                name: "api".into(),
                deletion_policy: DeletionPolicy::Orphan,
                force: false,
            },
            SurfaceResource {
                id: "api-config".into(),
                api_version: "v1".into(),
                kind: "ConfigMap".into(),
                namespace: "production".into(),
                name: "api-config".into(),
                deletion_policy: DeletionPolicy::Orphan,
                force: false,
            },
            SurfaceResource {
                id: "api-service".into(),
                api_version: "v1".into(),
                kind: "Service".into(),
                namespace: "production".into(),
                name: "api".into(),
                deletion_policy: DeletionPolicy::Orphan,
                force: false,
            },
        ]
    );

    // Inputs, sorted by id ("credentials" < "settings").
    assert_eq!(
        surface.inputs,
        vec![
            SurfaceInput {
                id: "credentials".into(),
                kind: "Secret".into(),
                namespace: "production".into(),
                name: "database-credentials".into(),
                mode: InputMode::Observed,
                sensitive: true,
            },
            SurfaceInput {
                id: "settings".into(),
                kind: "ConfigMap".into(),
                namespace: "production".into(),
                name: "external-settings".into(),
                mode: InputMode::Observed,
                sensitive: false,
            },
        ]
    );

    // Both Secrets the reconciler reads: the program's own observed Secret
    // and Apsides's own rollout-key secret, sorted by (namespace, name).
    assert_eq!(
        surface.secrets_read,
        vec![
            SurfaceSecretRead {
                namespace: "apsides-system".into(),
                name: "apsides-rollout-key-team-api-production".into(),
                reader: "reconcile".into(),
            },
            SurfaceSecretRead {
                namespace: "production".into(),
                name: "database-credentials".into(),
                reader: "reconcile".into(),
            },
        ]
    );
}

fn assert_reactions_and_images(surface: &Surface) {
    assert_eq!(
        surface.reactions,
        vec![
            SurfaceReaction {
                input: "credentials".into(),
                target: "api".into(),
            },
            SurfaceReaction {
                input: "settings".into(),
                target: "api".into(),
            },
        ]
    );

    assert_eq!(
        surface.images,
        vec![SurfaceImage {
            resource: "api".into(),
            container: "api".into(),
            image: IMAGE.into(),
            pinned: true,
        }]
    );
}

/// A name-restricted [`SurfaceVerb`]: the union of names every capability
/// requesting this verb against the target was scoped to.
fn verb(name: &str, names: &[&str]) -> SurfaceVerb {
    SurfaceVerb {
        verb: name.into(),
        all_names: false,
        names: Some(names.iter().map(|n| (*n).to_owned()).collect()),
    }
}

/// An unrestricted [`SurfaceVerb`]: some capability requesting this verb
/// against the target had no name (e.g. `create`'s list-before-create).
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

/// The complete, exact permission set for `web_application()`: the
/// reconciler's control-plane bookkeeping (activation check, platform policy
/// check, runtime inventory — merged onto one row since they share
/// namespace/apiGroup/resource), its own rollout-key secret, and every
/// declared or observed object's own read/write authority. `patch` is never
/// claimed on `external-settings` (an external input, never written), and
/// `update` is never claimed on the activation/policy `ConfigMap`s (which the
/// reconciler only ever reads) — each verb keeps only the names capabilities
/// actually requested it for, never every name in the group.
fn expected_permissions() -> Vec<SurfacePermission> {
    vec![
        permission(
            "apsides-system",
            "",
            "configmaps",
            vec![
                verb(
                    "get",
                    &[
                        "apsides-activation-team-api-production",
                        "apsides-inventory-team-api-production",
                        "apsides-policy-team-api-production",
                    ],
                ),
                verb("update", &["apsides-inventory-team-api-production"]),
            ],
        ),
        permission(
            "apsides-system",
            "",
            "secrets",
            vec![verb("get", &["apsides-rollout-key-team-api-production"])],
        ),
        permission(
            "production",
            "",
            "configmaps",
            vec![
                verb_all("create"),
                verb("get", &["external-settings"]),
                verb("list", &["api-config", "external-settings"]),
                verb("patch", &["api-config"]),
                verb("watch", &["external-settings"]),
            ],
        ),
        permission(
            "production",
            "",
            "secrets",
            vec![
                verb("get", &["database-credentials"]),
                verb("list", &["database-credentials"]),
                verb("watch", &["database-credentials"]),
            ],
        ),
        permission(
            "production",
            "",
            "services",
            vec![
                verb_all("create"),
                verb("list", &["api"]),
                verb("patch", &["api"]),
            ],
        ),
        permission(
            "production",
            "apps",
            "deployments",
            vec![
                verb_all("create"),
                verb("list", &["api"]),
                verb("patch", &["api"]),
            ],
        ),
    ]
}

fn assert_permissions(plan: &CompiledPlan, surface: &Surface) {
    assert_eq!(surface.risks, plan.indirect_risks);
    assert_eq!(surface.enforcement_gaps, plan.enforcement_gaps);
    assert_eq!(surface.permissions, expected_permissions());
}

#[test]
fn surface_web_application() -> TestResult {
    let plan = plan(&web_application())?;
    let surface = derive_surface(&plan);
    assert_domain_resources_and_inputs(&surface);
    assert_reactions_and_images(&surface);
    assert_permissions(&plan, &surface);
    Ok(())
}

#[test]
fn surface_is_deterministic() -> TestResult {
    let first = derive_surface(&plan(&web_application())?);
    let second = derive_surface(&plan(&web_application())?);
    assert_eq!(canonical_bytes(&first)?, canonical_bytes(&second)?);

    // Reordering resource and input declarations (and so, separately, the
    // reaction's input list) changes the IR's own bytes and its ir_digest —
    // canonical_bytes preserves array order — but must not change the
    // surface derived from it: every section is sorted by a stable key.
    let shuffled_plan = plan(&fixture(
        &json!([service(), deployment(), config_map()]),
        &json!([credentials_input(), settings_input()]),
        &json!(["credentials", "settings"]),
    ))?;
    assert_ne!(shuffled_plan.ir_digest, plan(&web_application())?.ir_digest);
    let shuffled = derive_surface(&shuffled_plan);
    assert_eq!(canonical_bytes(&first)?, canonical_bytes(&shuffled)?);
    Ok(())
}

#[test]
fn surface_excludes_values() -> TestResult {
    let plan = plan(&web_application())?;
    let surface = derive_surface(&plan);
    let bytes = canonical_bytes(&surface)?;
    let text = String::from_utf8(bytes)?;

    // The ConfigMap's only data value never appears in the surface.
    assert!(!text.contains("LOG_LEVEL"));
    assert!(!text.contains("info"));

    // Nor does the desired document or any source file location. (The
    // rollout-key secret's identity is deliberately NOT excluded: it is
    // real read authority, asserted present in `secrets_read` above.)
    assert!(!text.contains("replicas"));
    assert!(!text.contains("controller.ts"));
    Ok(())
}
