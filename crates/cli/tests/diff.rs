//! `aps diff` exit codes and output formats.
//!
//! Fixtures are built directly with `compiler_core::Surface` (every field is
//! `pub`) rather than through `aps compile`, since `aps diff` only ever reads
//! an already-derived `surface.json`.
use compiler_core::*;
use std::{fs, path::Path, process::Command};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn empty_surface() -> Surface {
    Surface {
        schema_version: SURFACE_SCHEMA.into(),
        domain: SurfaceDomain {
            id: "team-api-production".into(),
            namespace: "production".into(),
            control_namespace: "apsides-system".into(),
        },
        kubernetes: SurfaceKubernetes {
            min_version: "1.34".into(),
            qualified_on: "v1.34.3".into(),
        },
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

fn write_surface(path: &Path, surface: &Surface) -> Result {
    fs::write(path, canonical_bytes(surface)?)?;
    Ok(())
}

fn aps() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aps"))
}

#[test]
fn diff_exits_zero_with_no_changes_and_no_policy() -> Result {
    let directory = tempfile::tempdir()?;
    let base = directory.path().join("base.json");
    let head = directory.path().join("head.json");
    write_surface(&base, &empty_surface())?;
    write_surface(&head, &empty_surface())?;

    let output = aps()
        .args(["diff", "--base"])
        .arg(&base)
        .arg("--head")
        .arg(&head)
        .output()?;

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.starts_with("# Deployment surface diff\n"));
    assert!(stdout.contains("No change: domain, kubernetes, permissions"));
    Ok(())
}

#[test]
fn diff_reads_surface_json_inside_an_artifact_directory() -> Result {
    let directory = tempfile::tempdir()?;
    let base = directory.path().join("base");
    let head = directory.path().join("head");
    fs::create_dir_all(&base)?;
    fs::create_dir_all(&head)?;
    write_surface(&base.join("surface.json"), &empty_surface())?;
    write_surface(&head.join("surface.json"), &empty_surface())?;

    let output = aps()
        .args(["diff", "--base"])
        .arg(&base)
        .arg("--head")
        .arg(&head)
        .output()?;

    assert_eq!(output.status.code(), Some(0));
    Ok(())
}

#[test]
fn diff_exits_one_when_the_policy_requires_approval() -> Result {
    let directory = tempfile::tempdir()?;
    let base = directory.path().join("base.json");
    let head = directory.path().join("head.json");
    write_surface(&base, &empty_surface())?;
    let mut head_surface = empty_surface();
    head_surface.permissions = vec![SurfacePermission {
        namespace: "production".into(),
        api_group: String::new(),
        resource: "configmaps".into(),
        subresource: None,
        verbs: vec![SurfaceVerb {
            verb: "get".into(),
            all_names: true,
            names: None,
        }],
    }];
    write_surface(&head, &head_surface)?;
    let policy = directory.path().join("policy.json");
    fs::write(
        &policy,
        r#"{"schemaVersion":"apsides.dev/surface-policy/v1","requireApproval":["permissions.added"]}"#,
    )?;

    let output = aps()
        .args(["diff", "--base"])
        .arg(&base)
        .arg("--head")
        .arg(&head)
        .arg("--policy")
        .arg(&policy)
        .output()?;

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("permissions.added"), "{stderr}");
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("+ permissions production/core/configmaps"));
    Ok(())
}

#[test]
fn diff_exits_zero_when_the_policy_does_not_match() -> Result {
    let directory = tempfile::tempdir()?;
    let base = directory.path().join("base.json");
    let head = directory.path().join("head.json");
    write_surface(&base, &empty_surface())?;
    write_surface(&head, &empty_surface())?;
    let policy = directory.path().join("policy.json");
    fs::write(
        &policy,
        r#"{"schemaVersion":"apsides.dev/surface-policy/v1","requireApproval":["permissions.added"]}"#,
    )?;

    let output = aps()
        .args(["diff", "--base"])
        .arg(&base)
        .arg("--head")
        .arg(&head)
        .arg("--policy")
        .arg(&policy)
        .output()?;

    assert_eq!(output.status.code(), Some(0));
    Ok(())
}

#[test]
fn diff_json_format_reports_the_diff_and_triggered_rules() -> Result {
    let directory = tempfile::tempdir()?;
    let base = directory.path().join("base.json");
    let head = directory.path().join("head.json");
    write_surface(&base, &empty_surface())?;
    let mut head_surface = empty_surface();
    head_surface.risks = vec!["opaque-custom-resource-effects".into()];
    write_surface(&head, &head_surface)?;
    let policy = directory.path().join("policy.json");
    fs::write(
        &policy,
        r#"{"schemaVersion":"apsides.dev/surface-policy/v1","requireApproval":["risks.added"]}"#,
    )?;

    let output = aps()
        .args(["diff", "--base"])
        .arg(&base)
        .arg("--head")
        .arg(&head)
        .arg("--policy")
        .arg(&policy)
        .arg("--format")
        .arg("json")
        .output()?;

    assert_eq!(output.status.code(), Some(1));
    let body: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        body["diff"]["risks"]["added"],
        serde_json::json!(["opaque-custom-resource-effects"])
    );
    assert_eq!(body["triggeredRules"], serde_json::json!(["risks.added"]));
    Ok(())
}

#[test]
fn diff_json_format_omits_triggered_rules_without_a_policy() -> Result {
    let directory = tempfile::tempdir()?;
    let base = directory.path().join("base.json");
    let head = directory.path().join("head.json");
    write_surface(&base, &empty_surface())?;
    write_surface(&head, &empty_surface())?;

    let output = aps()
        .args(["diff", "--base"])
        .arg(&base)
        .arg("--head")
        .arg(&head)
        .arg("--format")
        .arg("json")
        .output()?;

    assert_eq!(output.status.code(), Some(0));
    let body: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert!(body["triggeredRules"].is_null());
    Ok(())
}

#[test]
fn diff_exits_two_for_a_missing_base_file() -> Result {
    let directory = tempfile::tempdir()?;
    let base = directory.path().join("missing.json");
    let head = directory.path().join("head.json");
    write_surface(&head, &empty_surface())?;

    let output = aps()
        .args(["diff", "--base"])
        .arg(&base)
        .arg("--head")
        .arg(&head)
        .output()?;

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.starts_with("aps: --base:"), "{stderr}");
    assert!(output.stdout.is_empty());
    Ok(())
}

#[test]
fn diff_exits_two_for_malformed_surface_json() -> Result {
    let directory = tempfile::tempdir()?;
    let base = directory.path().join("base.json");
    let head = directory.path().join("head.json");
    fs::write(&base, b"{ not json")?;
    write_surface(&head, &empty_surface())?;

    let output = aps()
        .args(["diff", "--base"])
        .arg(&base)
        .arg("--head")
        .arg(&head)
        .output()?;

    assert_eq!(output.status.code(), Some(2));
    Ok(())
}

#[test]
fn diff_exits_two_for_a_mismatched_schema_version() -> Result {
    let directory = tempfile::tempdir()?;
    let base = directory.path().join("base.json");
    let head = directory.path().join("head.json");
    fs::write(
        &base,
        br#"{"schemaVersion":"apsides.dev/surface/v0","domain":{"id":"d","namespace":"n","controlNamespace":"c"},"kubernetes":{"minVersion":"1.34","qualifiedOn":"v1.34.3"},"permissions":[],"secretsRead":[],"resources":[],"inputs":[],"reactions":[],"images":[],"risks":[],"enforcementGaps":[]}"#,
    )?;
    write_surface(&head, &empty_surface())?;

    let output = aps()
        .args(["diff", "--base"])
        .arg(&base)
        .arg("--head")
        .arg(&head)
        .output()?;

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.starts_with("aps: --base:"), "{stderr}");
    assert!(stderr.contains("schemaVersion"), "{stderr}");
    assert!(output.stdout.is_empty());
    Ok(())
}

#[test]
fn diff_exits_two_for_an_invalid_policy() -> Result {
    let directory = tempfile::tempdir()?;
    let base = directory.path().join("base.json");
    let head = directory.path().join("head.json");
    write_surface(&base, &empty_surface())?;
    write_surface(&head, &empty_surface())?;
    let policy = directory.path().join("policy.json");
    fs::write(
        &policy,
        r#"{"schemaVersion":"apsides.dev/surface-policy/v1","requireApproval":["not-a-real-rule"]}"#,
    )?;

    let output = aps()
        .args(["diff", "--base"])
        .arg(&base)
        .arg("--head")
        .arg(&head)
        .arg("--policy")
        .arg(&policy)
        .output()?;

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.starts_with("aps: --policy:"), "{stderr}");
    Ok(())
}

#[test]
fn diff_missing_required_arguments_is_a_clap_usage_error() -> Result {
    let output = aps().args(["diff", "--base", "b"]).output()?;
    assert_eq!(output.status.code(), Some(2));
    Ok(())
}
