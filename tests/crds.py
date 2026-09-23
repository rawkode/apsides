"""Fixture CRD helpers shared by the Python qualification harnesses.

The reactor cluster suite itself is `aps test --cluster` (crates/test-cluster);
these helpers parse CRD manifests with the compiler's locked YAML parser and
check them against the custom resource declarations an IR records.
"""

import hashlib
import json
import subprocess


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def digest(value):
    return "sha256:" + hashlib.sha256(canonical(value)).hexdigest()


def parse_crd_yaml(raw, root):
    # Reuse the compiler's locked YAML parser, including duplicate-key rejection.
    script = """import { parseAllDocuments } from 'yaml';
import { readFileSync } from 'node:fs';
const documents = parseAllDocuments(readFileSync(0, 'utf8'), { uniqueKeys: true });
if (documents.some(document => document.errors.length)) process.exit(1);
process.stdout.write(JSON.stringify(documents.map(document => document.toJS({ maxAliasCount: 0 }))));"""
    parsed = subprocess.run(["deno", "eval", "--ext=ts", script], input=raw,
                            capture_output=True, cwd=root / "packages/compiler", timeout=30)
    if parsed.returncode or len(parsed.stdout) > 4 * 1024 * 1024:
        raise RuntimeError("fixture CRD YAML is invalid or exceeds its parsing bound")
    return json.loads(parsed.stdout)


def validate_crd_documents(ir, manifests):
    expected = {}
    for resource in ir["resources"]:
        mapping = resource.get("apiResource", {})
        schema = mapping.get("schemaSha256")
        if schema is None:
            continue
        group, version = resource["apiVersion"].split("/", 1)
        identity = (group, resource["kind"], mapping["plural"])
        entry = expected.setdefault(identity, {"digest": schema, "versions": set()})
        if mapping["scope"] != "Namespaced" or entry["digest"] != schema:
            raise RuntimeError("fixture CRD declaration has inconsistent scope or schema")
        entry["versions"].add(version)
    seen = set()
    for source_digest, documents in manifests:
        if not documents:
            raise RuntimeError("fixture CRD manifest is empty")
        for document in documents:
            if (not isinstance(document, dict) or document.get("apiVersion") != "apiextensions.k8s.io/v1"
                    or document.get("kind") != "CustomResourceDefinition"):
                raise RuntimeError("fixture CRD manifest contains a non-CRD resource")
            spec = document.get("spec", {})
            names = spec.get("names", {})
            identity = (spec.get("group"), names.get("kind"), names.get("plural"))
            declaration = expected.get(identity)
            conversion = spec.get("conversion", {})
            versions = spec.get("versions", [])
            if (declaration is None or identity in seen or declaration["digest"] != source_digest
                    or spec.get("scope") != "Namespaced"
                    or document.get("metadata", {}).get("name") != f"{identity[2]}.{identity[0]}"
                    or document.get("metadata", {}).get("namespace") is not None
                    or conversion.get("strategy", "None") != "None" or "webhook" in conversion
                    or not isinstance(versions, list)
                    or not declaration["versions"] <= {item.get("name") for item in versions if item.get("served") is True}):
                raise RuntimeError("fixture CRD differs from the declared schema, identity, scope or version")
            seen.add(identity)
    if seen != set(expected):
        raise RuntimeError("fixture custom resources require their exact local CRD manifests")
