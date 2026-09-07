#!/usr/bin/env python3
"""Exercise a relocated aps distribution without allowing checkout reads."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parents[2]
CRD_URL = "https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_clusters.yaml"
SDK_FILES = (
    "index.ts",
    "model.ts",
    "runtime.ts",
    "compiler.ts",
    "jsx-runtime.ts",
    "jsx-dev-runtime.ts",
)
RECEIPT_FIELDS = {
    "bundleMapSha256",
    "bundleSha256",
    "compilerClosure",
    "compilerClosureDigest",
    "entry",
    "evaluation",
    "evaluationBundleSha256",
    "evaluationSourceSha256",
    "inputs",
    "lock",
    "reproducibility",
    "schemaVersion",
    "sdk",
    "toolchain",
    "typecheck",
    "virtualRoot",
}
MAX_COMMAND_BYTES = 1024 * 1024
MAX_SCAN_FILES = 512
MAX_SCAN_FILE_BYTES = 16 * 1024 * 1024
MAX_SCAN_TREE_BYTES = 64 * 1024 * 1024

PROGRAM = '''import { Controller, type DomainBudgets } from "@reakt/kubernetes";
import { Cluster } from "./.aps/crds/cnpg.ts";

const budgets: DomainBudgets = {
  maxResources: 8,
  maxInputs: 8,
  maxReactions: 8,
  maxObjectBytes: 262_144,
  maxQueueDepth: 32,
  maxConcurrentRequests: 2,
};

export default (
  <Controller
    id="hello"
    namespace="default"
    controlNamespace="apsides-system"
    clusterBinding="local"
    policyRef="platform/hello"
    enforcement="api-bound"
    budgets={budgets}
  >
    <Cluster
      metadata={{ name: "database", namespace: "default" }}
      spec={{ instances: 1, storage: { size: "1Gi" } }}
    />
  </Controller>
);
'''


class AcceptanceError(Exception):
    """A bounded relocation acceptance failure."""


def sha256(data: bytes) -> str:
    return f"sha256:{hashlib.sha256(data).hexdigest()}"


def regular(path: Path) -> bytes:
    metadata = path.lstat()
    if path.is_symlink() or not stat.S_ISREG(metadata.st_mode):
        raise AcceptanceError("expected a regular non-symlink file")
    if metadata.st_size > MAX_SCAN_FILE_BYTES:
        raise AcceptanceError("acceptance input exceeds the file limit")
    data = path.read_bytes()
    if len(data) != metadata.st_size:
        raise AcceptanceError("acceptance input changed while reading")
    return data


def binary_digest(path: Path) -> str:
    metadata = path.lstat()
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > 256 * 1024 * 1024:
        raise AcceptanceError("invalid distribution binary")
    digest = hashlib.sha256()
    remaining = metadata.st_size
    with path.open("rb") as stream:
        while remaining:
            chunk = stream.read(min(remaining, 1024 * 1024))
            if not chunk:
                raise AcceptanceError("distribution binary changed while reading")
            digest.update(chunk)
            remaining -= len(chunk)
        if stream.read(1):
            raise AcceptanceError("distribution binary grew while reading")
    return f"sha256:{digest.hexdigest()}"


def strict_json(path: Path) -> dict:
    value = json.loads(regular(path))
    if not isinstance(value, dict):
        raise AcceptanceError("expected a JSON object")
    return value


def within(parent: Path, child: Path) -> bool:
    return child == parent or parent in child.parents


def sandbox_profile() -> str:
    checkout = json.dumps(str(ROOT.resolve(strict=True)))
    return f"(version 1)\n(allow default)\n(deny file-read* (subpath {checkout}))\n"


def command(aps: Path, arguments: list[str], cwd: Path, *, success: bool) -> bytes:
    print(f"aps acceptance: {arguments[0]}", flush=True)
    invocation = ["/usr/bin/sandbox-exec", "-p", sandbox_profile(), str(aps), *arguments]
    result = subprocess.run(
        invocation,
        cwd=cwd,
        check=False,
        capture_output=True,
        timeout=300,
    )
    if len(result.stdout) + len(result.stderr) > MAX_COMMAND_BYTES:
        raise AcceptanceError("aps command output exceeds the acceptance limit")
    if success != (result.returncode == 0):
        raise AcceptanceError("aps command returned an unexpected status")
    return result.stdout


def prove_checkout_denied(cwd: Path) -> None:
    result = subprocess.run(
        [
            "/usr/bin/sandbox-exec",
            "-p",
            sandbox_profile(),
            "/bin/cat",
            str(ROOT / "package.json"),
        ],
        cwd=cwd,
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=30,
    )
    if result.returncode == 0:
        raise AcceptanceError("sandbox did not deny checkout reads")


def compare_sdk(prefix: Path, project: Path) -> dict[str, str]:
    digests = {}
    for name in SDK_FILES:
        packaged = regular(prefix / "share/apsides/packages/reakt/src" / name)
        generated = regular(project / ".aps/sdk" / name)
        if generated != packaged:
            raise AcceptanceError("aps init SDK differs from the packaged SDK")
        digests[name] = sha256(packaged)
    return digests


def validate_receipt(path: Path, sdk_digests: dict[str, str]) -> dict:
    value = strict_json(path)
    if set(value) != RECEIPT_FIELDS or value.get("schemaVersion") != "reakt.dev/build-inputs/v1":
        raise AcceptanceError("source compiler receipt is not closed")
    if value.get("entry") != "program.tsx" or value.get("virtualRoot") != "reakt:/source":
        raise AcceptanceError("source compiler receipt has the wrong entry")
    inputs = value.get("inputs")
    if not isinstance(inputs, list) or any(not isinstance(item, dict) or set(item) != {"path", "sha256"} for item in inputs) or [item["path"] for item in inputs] != [
        ".aps/crds/cnpg.ts",
        "program.tsx",
    ]:
        raise AcceptanceError("source compiler receipt has the wrong locked inputs")
    sdk = value.get("sdk")
    expected_sdk = [{"path": name, "sha256": digest.removeprefix("sha256:")} for name, digest in sorted(sdk_digests.items())]
    if not isinstance(sdk, dict) or set(sdk) != {"identity", "version", "files", "sha256"} or sdk.get("identity") != "@reakt/kubernetes" or sdk.get("version") != "0.1.0" or sdk.get("files") != expected_sdk:
        raise AcceptanceError("source compiler receipt does not bind the packaged SDK")
    toolchain = value.get("toolchain")
    closure = value.get("compilerClosure")
    if not isinstance(toolchain, dict) or not isinstance(closure, dict) or set(closure) != {
        "schemaVersion",
        "authoringCompiler",
        "nativeCompiler",
    }:
        raise AcceptanceError("source compiler closure is malformed")
    if closure.get("schemaVersion") != "reakt.dev/compiler-closure/v2" or closure.get("nativeCompiler") is not None:
        raise AcceptanceError("compile acceptance unexpectedly contains a native compiler closure")
    if closure.get("authoringCompiler") != {"sdk": sdk, "toolchain": toolchain}:
        raise AcceptanceError("source compiler closure does not match its public receipt")
    if set(toolchain) != {"node", "deno", "oxc", "rolldown", "ses", "compilerFiles", "installedTools"}:
        raise AcceptanceError("source toolchain receipt is not closed")
    compiler_files = toolchain.get("compilerFiles")
    if not isinstance(compiler_files, list) or any(not isinstance(item, dict) or set(item) != {"path", "sha256"} for item in compiler_files) or [item["path"] for item in compiler_files] != [
        "@apsides/compiler/compiler.ts",
        "@apsides/compiler/fixtures.ts",
    ]:
        raise AcceptanceError("source compiler receipt omits compiler modules")
    installed = toolchain.get("installedTools")
    if not isinstance(installed, list) or len(installed) != 9 or any(not isinstance(item, dict) or set(item) != {"path", "sha256"} for item in installed):
        raise AcceptanceError("source compiler receipt omits installed dependencies")
    installed_paths = [item["path"] for item in installed]
    fixed = {
        "packages/compiler",
        "node_modules/ansis",
        "node_modules/oxc-transform",
        "node_modules/rolldown",
        "node_modules/ses",
        "node_modules/@rolldown/pluginutils",
        "pnpm-lock.yaml",
    }
    if not fixed.issubset(installed_paths) or len([path for path in installed_paths if isinstance(path, str) and path.startswith("node_modules/@oxc-transform/binding-")]) != 1 or len([path for path in installed_paths if isinstance(path, str) and path.startswith("node_modules/@rolldown/binding-")]) != 1:
        raise AcceptanceError("source compiler receipt dependency closure is not exact")
    evaluation = value.get("evaluation")
    permissions = evaluation.get("permissions") if isinstance(evaluation, dict) else None
    if not isinstance(permissions, dict) or set(permissions) != {"read", "write", "net", "env", "run", "ffi", "sys", "import"} or set(permissions.values()) != {"denied"}:
        raise AcceptanceError("source evaluation permissions are not closed")
    if value.get("typecheck") != {
        "engine": "deno",
        "status": "passed",
        "remote": False,
        "npm": False,
        "config": {
            "jsx": "react-jsxdev",
            "jsxImportSource": "@reakt/kubernetes",
            "importMap": "generated-local-sdk-only",
        },
    }:
        raise AcceptanceError("source typecheck receipt is not exact")
    closure_digest = value.get("compilerClosureDigest")
    if not isinstance(closure_digest, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", closure_digest):
        raise AcceptanceError("source compiler receipt contains an invalid closure digest")
    if not isinstance(value.get("bundleSha256"), str) or not re.fullmatch(r"[0-9a-f]{64}", value["bundleSha256"]):
        raise AcceptanceError("source compiler receipt contains an invalid bundle digest")
    return value


def dependency_file(prefix: Path) -> Path:
    manifest = strict_json(prefix / "asset-manifest.json")
    matches = [
        path
        for path, identity in manifest.items()
        if isinstance(path, str)
        and isinstance(identity, str)
        and identity.startswith("sha256:")
        and re.search(r"node_modules/\.pnpm/ses@2\.3\.0/node_modules/ses/package\.json$", path)
    ]
    if len(matches) != 1:
        raise AcceptanceError("cannot identify the packaged SES dependency file")
    return prefix / "share/apsides" / matches[0]


def tamper_checks(prefix: Path, aps: Path, stage: Path) -> list[dict[str, object]]:
    targets = [
        ("compiler", prefix / "share/apsides/packages/compiler/src/compiler.ts", "mutate"),
        ("sdk", prefix / "share/apsides/packages/reakt/src/model.ts", "remove"),
        ("pnpm-lock", prefix / "share/apsides/pnpm-lock.yaml", "mutate"),
        ("dependency", dependency_file(prefix), "remove"),
        ("runtime", prefix / "share/apsides/crates/runtime/src/lib.rs", "mutate"),
        ("native-builder", prefix / "share/apsides/scripts/reakt/build.py", "remove"),
    ]
    observations = []
    for index, (role, path, operation) in enumerate(targets):
        original = regular(path)
        mode = stat.S_IMODE(path.stat().st_mode)
        destination = stage / f"tamper-{index}"
        try:
            if operation == "remove":
                path.unlink()
            else:
                path.write_bytes(original + b"\n")
            command(aps, ["init", "--directory", str(destination)], stage, success=False)
            if destination.exists() or destination.is_symlink():
                raise AcceptanceError("rejected damaged distribution published an init target")
        finally:
            if path.exists() or path.is_symlink():
                path.unlink()
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(original)
            path.chmod(mode)
        if regular(path) != original:
            raise AcceptanceError("damaged distribution asset was not restored")
        observations.append({"role": role, "operation": operation, "rejected": True, "restored": True})
    return observations


def scan_checkout_absence(stage: Path) -> None:
    needle = str(ROOT.resolve(strict=True)).encode()
    count = 0
    total = 0
    for directory, directories, files in os.walk(stage, followlinks=False):
        here = Path(directory)
        if any((here / name).is_symlink() for name in directories):
            raise AcceptanceError("acceptance output contains a symlink directory")
        for name in files:
            path = here / name
            data = regular(path)
            count += 1
            total += len(data)
            if count > MAX_SCAN_FILES or total > MAX_SCAN_TREE_BYTES:
                raise AcceptanceError("acceptance output exceeds the scan budget")
            if needle in data:
                raise AcceptanceError("acceptance output retained a checkout path")


def accept(prefix: Path, output: Path) -> None:
    if sys.platform != "darwin" or not Path("/usr/bin/sandbox-exec").is_file():
        raise AcceptanceError("CLI relocation isolation acceptance requires macOS sandbox-exec")
    prefix = prefix.resolve(strict=True)
    output = output.absolute()
    checkout = ROOT.resolve(strict=True)
    if within(checkout, prefix) or within(checkout, output) or within(prefix, output) or within(output, prefix):
        raise AcceptanceError("prefix and output must be external, unrelated paths")
    if output.exists() or output.is_symlink():
        raise AcceptanceError("output already exists; choose a fresh directory")
    aps = prefix / "bin/aps"
    if aps.is_symlink() or not aps.is_file() or not os.access(aps, os.X_OK):
        raise AcceptanceError("packaged aps executable is unavailable")
    aps_digest = binary_digest(aps)
    manifest_bytes = regular(prefix / "asset-manifest.json")
    manifest_digest = sha256(manifest_bytes)
    output.parent.mkdir(parents=True, exist_ok=True)
    parent = output.parent.resolve(strict=True)
    temporary = Path(tempfile.mkdtemp(prefix=f".{output.name}-", dir=parent))
    published = False
    try:
        prove_checkout_denied(temporary)
        project = temporary / "project"
        command(aps, ["init", "--directory", str(project)], temporary, success=True)
        sdk_digests = compare_sdk(prefix, project)
        command(
            aps,
            [
                "crds",
                "fetch",
                "--url",
                CRD_URL,
                "--kind",
                "Cluster",
                "--output",
                ".aps/crds/cnpg.ts",
                "--version",
                "v1",
            ],
            project,
            success=True,
        )
        (project / "program.tsx").write_text(PROGRAM)
        command(
            aps,
            ["lock", "--source", "program.tsx", "--modules", ".aps/crds/cnpg.ts"],
            project,
            success=True,
        )
        command(
            aps,
            ["compile", "--source", "program.tsx", "--output", "artifact"],
            project,
            success=True,
        )
        verified_raw = command(
            aps,
            ["verify", "--artifact", "artifact"],
            project,
            success=True,
        )
        verified = json.loads(verified_raw)
        if set(verified) != {"status", "irDigest", "stage", "productionReady", "provenanceVerified"} or verified.get("status") != "integrity-verified" or verified.get("stage") != "compiled-source" or verified.get("productionReady") is not False or verified.get("provenanceVerified") is not False:
            raise AcceptanceError("aps verify returned an unexpected boundary")
        receipt = validate_receipt(project / "artifact/source/build-inputs.json", sdk_digests)
        tampered = tamper_checks(prefix, aps, temporary)
        if binary_digest(aps) != aps_digest or regular(prefix / "asset-manifest.json") != manifest_bytes:
            raise AcceptanceError("distribution binary or manifest changed during acceptance")
        (temporary / "asset-manifest.json").write_bytes(manifest_bytes)
        scan_checkout_absence(temporary)
        summary = {
            "schemaVersion": "reakt.dev/cli-distribution-acceptance/v1",
            "passed": True,
            "releaseVerified": False,
            "platform": "darwin",
            "sandbox": {"checkoutReadDenied": True, "allApsCommandsSandboxed": True},
            "apsBinaryDigest": aps_digest,
            "assetManifestDigest": manifest_digest,
            "sdkDigests": sdk_digests,
            "crdSourceDigest": sha256(regular(project / ".aps/crds/cnpg.ts")),
            "compilerClosureDigest": receipt["compilerClosureDigest"],
            "irDigest": verified["irDigest"],
            "artifactStage": verified["stage"],
            "tamperChecks": tampered,
            "limitations": [
                "This proves one relocated macOS CLI authoring and local compile flow; it is not release verification or a native OCI build acceptance.",
                "Linux remains unsupported until an equivalent checkout-read isolation mechanism is qualified.",
            ],
        }
        (temporary / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
        scan_checkout_absence(temporary)
        if output.exists() or output.is_symlink():
            raise AcceptanceError("output appeared during acceptance")
        temporary.rename(output)
        published = True
    finally:
        if not published:
            shutil.rmtree(temporary, ignore_errors=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prefix", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    try:
        accept(arguments.prefix, arguments.output)
    except (AcceptanceError, json.JSONDecodeError, OSError, subprocess.TimeoutExpired, UnicodeError, ValueError):
        raise SystemExit("aps: CLI distribution acceptance failed") from None


if __name__ == "__main__":
    main()
