#!/usr/bin/env python3
"""Exercise issued credentials in an owned disposable cluster; never use ambient context."""
import json
import os
from pathlib import Path
import subprocess
import sys
import uuid

ROOT = Path(__file__).resolve().parents[2]
NODE = "kindest/node:v1.34.3@sha256:08497ee19eace7b4b5348db5c6a1591d7752b164530a36f855cb0f2bdcbadd48"


def run(arguments, environment, output, name, timeout):
    with (output / name).open("w") as log:
        result = subprocess.run(arguments, cwd=ROOT, env=environment, stdout=log,
                                stderr=subprocess.STDOUT, timeout=timeout, check=False)
    if result.returncode:
        raise RuntimeError(f"{name} failed with exit code {result.returncode}; inspect its local log")


def main():
    os.umask(0o077)
    root = ROOT
    cluster = "aps-preview-" + uuid.uuid4().hex[:16]
    output = root / "work" / cluster
    output.mkdir(parents=True, exist_ok=False)
    environment = {**os.environ, "KUBECONFIG": str(output / "admin.kubeconfig"),
                   "APS_PREVIEW_LIVE_CONTEXT": "kind-" + cluster}
    owned = False
    result = {"cluster": cluster, "nodeImage": NODE,
              "scope": "issued-client-conditional-ssa-and-rbac-only", "passed": False}
    try:
        existing = subprocess.run(["kind", "get", "clusters"], env=environment,
                                  capture_output=True, text=True, check=True, timeout=30)
        if cluster in existing.stdout.splitlines():
            raise RuntimeError("disposable cluster name already exists")
        owned = True
        run(["kind", "create", "cluster", "--name", cluster, "--image", NODE,
             "--kubeconfig", environment["KUBECONFIG"], "--wait", "120s"],
            environment, output, "create.log", 240)
        run(["cuenv", "task", "previewIssuedLive"], environment, output, "test.log", 900)
        test_log = (output / "test.log").read_text()
        expected = "test preview_issued_live::issued_credentials_enforce_conditional_ssa_and_rbac ... ok"
        if expected not in test_log or "test result: ok. 1 passed; 0 failed; 0 ignored" not in test_log:
            raise RuntimeError("the expected live qualification test did not execute successfully")
        result["passed"] = True
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        result["error"] = str(error)
    finally:
        if owned:
            try:
                run(["kind", "delete", "cluster", "--name", cluster],
                    environment, output, "cleanup.log", 120)
                result["clusterRemoved"] = True
            except (OSError, RuntimeError, subprocess.SubprocessError) as error:
                result["clusterRemoved"] = False
                result["cleanupError"] = str(error)
                result["passed"] = False
        # The admin kubeconfig contains disposable credentials, not evidence.
        (output / "admin.kubeconfig").unlink(missing_ok=True)
        (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"passed": result["passed"], "evidence": str(output / "result.json")}))
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
