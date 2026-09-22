//! Pinned `CustomResourceDefinition`s for fixture programs that manage custom
//! resources.
//!
//! The IR records, per custom resource type, the SHA-256 of the CRD source
//! the program was compiled against. The runner installs only CRD manifests
//! whose bytes hash to exactly those digests and whose definitions match the
//! declared group, kind, plural, scope and served versions. No CRD is
//! installed that the IR does not declare, and no declared type is left
//! without its definition.

use crate::{Result, input};
use compiler_core::CompiledPlan;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

const MAX_MANIFESTS: usize = 16;
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;

/// A CRD manifest file, read once and checked against the IR.
#[derive(Debug)]
pub struct CrdManifest {
    pub source: PathBuf,
    /// The name the copy in the output directory has.
    pub file: String,
    pub digest: String,
    pub raw: Vec<u8>,
    pub documents: Vec<Value>,
}

/// Read, parse and check every CRD manifest against the plan.
pub fn snapshot(paths: &[PathBuf], plan: &CompiledPlan) -> Result<Vec<CrdManifest>> {
    if paths.len() > MAX_MANIFESTS {
        return input("too many CRD manifests");
    }
    let manifests = paths
        .iter()
        .enumerate()
        .map(|(index, path)| read(index, path))
        .collect::<Result<Vec<_>>>()?;
    validate(plan, &manifests)?;
    Ok(manifests)
}

fn read(index: usize, path: &Path) -> Result<CrdManifest> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_MANIFEST_BYTES {
        return input(format!(
            "CRD manifest {} is not a bounded regular file",
            path.display()
        ));
    }
    let raw = std::fs::read(path)?;
    if raw.len() as u64 > MAX_MANIFEST_BYTES {
        return input("CRD manifest grew beyond its bound");
    }
    Ok(CrdManifest {
        source: path.to_path_buf(),
        file: format!("fixture-crd-{index}.yaml"),
        digest: format!("sha256:{:x}", Sha256::digest(&raw)),
        documents: parse(&raw)?,
        raw,
    })
}

/// Parse a multi-document YAML stream, refusing duplicate keys and aliases.
pub fn parse(raw: &[u8]) -> Result<Vec<Value>> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| crate::Error::Input("CRD manifest is not UTF-8".into()))?;
    let options = serde_saphyr::options! {
        duplicate_keys: serde_saphyr::DuplicateKeyPolicy::Error,
        alias_limits: serde_saphyr::alias_limits! {
            max_total_replayed_events: 0,
            max_alias_expansions_per_anchor: 0,
        },
    };
    let documents: Vec<Value> = serde_saphyr::from_multiple_with_options(text, options)
        .map_err(|error| crate::Error::Input(format!("CRD manifest is invalid YAML: {error}")))?;
    Ok(documents
        .into_iter()
        .filter(|item| !item.is_null())
        .collect())
}

/// Every declared custom resource type must have exactly one matching
/// definition, from a manifest with the declared schema digest.
pub fn validate(plan: &CompiledPlan, manifests: &[CrdManifest]) -> Result<()> {
    let mut expected: BTreeMap<(String, String, String), (String, BTreeSet<String>)> =
        BTreeMap::new();
    for resource in &plan.ir.resources {
        let Some(mapping) = &resource.api_resource else {
            continue;
        };
        let Some((group, version)) = resource.api_version.split_once('/') else {
            return input(format!(
                "{} declares a CRD type without a group",
                resource.id
            ));
        };
        let identity = (
            group.to_owned(),
            resource.kind.clone(),
            mapping.plural.clone(),
        );
        let entry = expected
            .entry(identity)
            .or_insert_with(|| (mapping.schema_sha256.clone(), BTreeSet::new()));
        if entry.0 != mapping.schema_sha256 {
            return input("CRD declarations disagree on their schema digest");
        }
        entry.1.insert(version.to_owned());
    }
    let mut seen = BTreeSet::new();
    for manifest in manifests {
        if manifest.documents.is_empty() {
            return input(format!(
                "CRD manifest {} is empty",
                manifest.source.display()
            ));
        }
        for document in &manifest.documents {
            let identity = check_document(document, &manifest.digest, &expected)?;
            if !seen.insert(identity) {
                return input("a CRD is defined more than once");
            }
        }
    }
    if seen.len() != expected.len() {
        return input("custom resources require their exact CRD manifests");
    }
    Ok(())
}

fn check_document(
    document: &Value,
    digest: &str,
    expected: &BTreeMap<(String, String, String), (String, BTreeSet<String>)>,
) -> Result<(String, String, String)> {
    if document["apiVersion"] != "apiextensions.k8s.io/v1"
        || document["kind"] != "CustomResourceDefinition"
    {
        return input("CRD manifest contains a resource that is not a CRD");
    }
    let spec = &document["spec"];
    let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    let identity = (
        text(&spec["group"]),
        text(&spec["names"]["kind"]),
        text(&spec["names"]["plural"]),
    );
    let served = spec["versions"]
        .as_array()
        .map(|versions| {
            versions
                .iter()
                .filter(|version| version["served"] == true)
                .map(|version| text(&version["name"]))
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let conversion = &spec["conversion"];
    let matches = expected.get(&identity).is_some_and(|(schema, versions)| {
        schema == digest
            && spec["scope"] == "Namespaced"
            && document["metadata"]["name"] == format!("{}.{}", identity.2, identity.0)
            && document["metadata"]["namespace"].is_null()
            && conversion["strategy"].as_str().unwrap_or("None") == "None"
            && conversion.get("webhook").is_none()
            && versions.is_subset(&served)
    });
    if !matches {
        return input(format!(
            "CRD {}.{} differs from the declared schema, identity, scope or versions",
            identity.2, identity.0
        ));
    }
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CRD: &str = "apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: clusters.postgresql.cnpg.io
spec:
  group: postgresql.cnpg.io
  scope: Namespaced
  names:
    kind: Cluster
    plural: clusters
  versions:
    - name: v1
      served: true
      storage: true
      schema:
        openAPIV3Schema:
          type: object
";

    fn plan_with_cluster(schema: &str) -> Result<CompiledPlan> {
        let mut ir = crate::testing::ir();
        let resources = ir["resources"]
            .as_array_mut()
            .ok_or_else(|| crate::Error::Case("resources".into()))?;
        resources.push(json!({
            "id": "cluster-database", "apiVersion": "postgresql.cnpg.io/v1", "kind": "Cluster",
            "apiResource": {"plural": "clusters", "scope": "Namespaced", "schemaSha256": schema},
            "name": "database", "namespace": "work",
            "desired": {"apiVersion": "postgresql.cnpg.io/v1", "kind": "Cluster",
                "metadata": {"name": "database", "namespace": "work"}, "spec": {"instances": 1}},
            "lifecycle": {"deletionPolicy": "orphan"},
            "source": {"file": "program.tsx", "line": 1, "column": 1}}));
        Ok(compiler_core::parse_and_validate(&ir.to_string())?)
    }

    fn manifest(raw: &str) -> Result<CrdManifest> {
        Ok(CrdManifest {
            source: PathBuf::from("crd.yaml"),
            file: "fixture-crd-0.yaml".into(),
            digest: format!("sha256:{:x}", Sha256::digest(raw.as_bytes())),
            raw: raw.as_bytes().to_vec(),
            documents: parse(raw.as_bytes())?,
        })
    }

    #[test]
    fn the_exact_declared_definition_is_accepted() -> Result<()> {
        let digest = format!("sha256:{:x}", Sha256::digest(CRD.as_bytes()));
        let plan = plan_with_cluster(&digest)?;
        validate(&plan, &[manifest(CRD)?])?;
        validate(&crate::testing::plan()?, &[])?;
        Ok(())
    }

    #[test]
    fn missing_extra_and_altered_definitions_are_refused() -> Result<()> {
        let digest = format!("sha256:{:x}", Sha256::digest(CRD.as_bytes()));
        let plan = plan_with_cluster(&digest)?;
        assert!(validate(&plan, &[]).is_err(), "missing definition");
        assert!(
            validate(&crate::testing::plan()?, &[manifest(CRD)?]).is_err(),
            "undeclared definition"
        );
        for (from, to) in [
            ("scope: Namespaced", "scope: Cluster"),
            ("plural: clusters", "plural: backups"),
            ("served: true", "served: false"),
            ("name: clusters.postgresql.cnpg.io", "name: wrong"),
            ("kind: CustomResourceDefinition", "kind: Deployment"),
        ] {
            let altered = CRD.replace(from, to);
            let altered_digest = format!("sha256:{:x}", Sha256::digest(altered.as_bytes()));
            let plan = plan_with_cluster(&altered_digest)?;
            assert!(validate(&plan, &[manifest(&altered)?]).is_err(), "{to}");
        }
        let webhook = format!("{CRD}  conversion:\n    strategy: None\n    webhook: {{}}\n");
        let plan = plan_with_cluster(&format!("sha256:{:x}", Sha256::digest(webhook.as_bytes())))?;
        assert!(validate(&plan, &[manifest(&webhook)?]).is_err(), "webhook");
        let plan = plan_with_cluster(&format!("sha256:{}", "0".repeat(64)))?;
        assert!(validate(&plan, &[manifest(CRD)?]).is_err(), "digest");
        let twice = format!("{CRD}---\n{CRD}");
        let plan = plan_with_cluster(&format!("sha256:{:x}", Sha256::digest(twice.as_bytes())))?;
        assert!(validate(&plan, &[manifest(&twice)?]).is_err(), "duplicate");
        Ok(())
    }

    #[test]
    fn the_parser_refuses_duplicate_keys_and_aliases() -> Result<()> {
        assert_eq!(
            parse(b"kind: CustomResourceDefinition\n")?,
            [json!({"kind": "CustomResourceDefinition"})]
        );
        assert!(parse(b"kind: one\nkind: two\n").is_err());
        assert!(parse(b"value: &a yes\ncopy: *a\n").is_err());
        Ok(())
    }
}
