#!/usr/bin/env python3
"""Assemble signed, unpromoted lifecycle candidates; never fabricate release coverage."""
import argparse
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[2]
REPOSITORY = "https://github.com/reakt-tests/disposable-fixture"


def run(args):
    subprocess.run([str(arg) for arg in args], check=True, cwd=ROOT)


def prepare(args):
    output = args.output.resolve()
    output.mkdir()
    artifact = args.artifact.resolve()
    ir = json.loads((artifact / "ir.json").read_text())
    domain = ir["domain"]
    if not domain["clusterBinding"].startswith("reakt-disposable-") or any(not domain[key].startswith("reakt-test-") for key in ["namespace", "controlNamespace"]):
        raise ValueError("candidate preparation requires the explicit disposable test scope")
    build = json.loads((artifact / "build-result.json").read_text())
    if build.get("schemaVersion") != "reakt.dev/build-result/v1" or build.get("releaseVerified") is not False:
        raise ValueError("expected a complete, unpromoted Reakt build")
    image_directory = artifact / "image"
    (output / "coverage.json").write_text(json.dumps({"status": "pending", "purpose": "first live deployment acceptance; not release evidence"}))
    (output / "evidence").mkdir()
    image = json.loads((image_directory / "buildkit-metadata.json").read_text())["containerimage.digest"]
    # The repository/commit are explicitly synthetic test identities, not a claim
    # that these uncommitted test inputs exist in a published GitHub repository.
    run([args.cli, "release", "--artifact", artifact, "--binary", artifact / "linux-binary/reakt-controller", "--oci", image_directory / "controller.oci.tar",
         "--image", "localhost/reakt-lifecycle@" + image, "--platform", build["platform"], "--repository", REPOSITORY,
         "--commit", "0" * 40, "--build-inputs", artifact / "build-inputs.json", "--sbom", artifact / "sbom/sbom.json",
         "--coverage", output / "coverage.json", "--evidence", output / "evidence", "--output", output / "candidate"])
    provenance = output / "candidate/provenance.json"
    run(["/usr/bin/ssh-keygen", "-Y", "sign", "-f", args.key, "-n", "reakt.provenance.v1", provenance])
    provenance.with_suffix(".json.sig").rename(output / "candidate/provenance.sig")
    (output / "TEST-ONLY.json").write_text(json.dumps({"releaseVerified": False, "syntheticSourceIdentity": True,
        "coverage": "pending", "purpose": "disposable deployment lifecycle integration"}, indent=2))
    print(output / "candidate")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["artifact", "cli", "key", "output"]:
        parser.add_argument("--" + name, required=True, type=Path)
    prepare(parser.parse_args())
