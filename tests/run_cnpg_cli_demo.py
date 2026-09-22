#!/usr/bin/env python3
"""Rehearse initial deployment and upgrade through the public aps CLI on owned kind."""
import argparse
import json
import os
from pathlib import Path
import sys

import run_cnpg_demo as demo


def committed_report(raw, expected):
    report = json.loads(raw)
    if (not isinstance(report, dict) or report.get("status") != "committed"
            or report.get("artifactTrust") != "verified"
            or report.get("releaseQualification") != "not-evaluated"
            or report.get("releaseDigest") != expected["releaseDigest"]
            or report.get("irDigest") != expected["irDigest"]
            or any(not isinstance(report.get(key), str) or not report[key]
                   or len(report[key]) > 256 for key in ["transition", "generation"])
            or not isinstance(report.get("inventory"), dict)
            or not demo.shared.valid_digest(report["inventory"].get("snapshotId", ""))
            or not isinstance(report["inventory"].get("revision"), str)
            or not report["inventory"]["revision"].isdigit()):
        raise demo.shared.QualificationError("CLI did not report the exact committed candidate")
    return {key: report[key] for key in ["status", "artifactTrust", "releaseQualification",
            "releaseDigest", "irDigest", "transition", "generation", "inventory"]}


class CliDemoRunner(demo.CnpgDemoRunner):
    def __init__(self, args):
        if not args.aps.is_absolute() or args.aps.is_symlink() or not args.aps.is_file():
            raise demo.shared.QualificationError("CLI executable must be an absolute regular file")
        args.retain_on_success = False
        args.collect_performance = False
        args.check_input_reactions = True
        super().__init__(args)
        self.aps = args.aps
        # The public CLI opens inventory and coordination tables in one database.
        self.coordination = self.inventory
        self.freeze_file("executable/aps", self.aps)
        self.freeze_file("producer/cnpg-cli-demo.py", Path(__file__).resolve())
        self.reports = []
        self.failure_reasons |= {"CLI did not report the exact committed candidate",
                                 "CLI upgrade reused the initial allocation",
                                 "CLI demo cleanup was not confirmed"}

    def deploy_cli(self, artifact, subject):
        self.refresh_issuer()
        self.require_inputs_unchanged()
        environment = dict(os.environ, KUBECONFIG=str(self.issuer),
                           APSIDES_INVENTORY_DATABASE=str(self.inventory))
        for name in ["APS_PERMISSION_UPGRADE_STEP", "APS_PERMISSION_UPGRADE_COORDINATION_DB",
                     "APS_PERMISSION_UPGRADE_INVENTORY_DB"]:
            environment.pop(name, None)
        raw = self.command([self.aps, "deploy", "--artifact", artifact,
                            "--executor", "cnpg-cli-demo"], environment=environment, timeout=600)
        report = committed_report(raw, subject)
        self.require_inputs_unchanged()
        self.reports.append(report)
        (self.output / "cli-reports.json").write_bytes(demo.shared.canonical(self.reports))
        return report

    def measure(self):
        candidates = self.pair["candidates"]
        def subject(prefix):
            return {key: candidates[prefix + key[0].upper() + key[1:]] for key in ["releaseDigest", "irDigest"]}
        self.phase = "cli-initial-deploy"
        print("Deploying the database with aps deploy…", flush=True)
        initial = self.deploy_cli(self.initial_artifact, subject("initial"))
        self.wait_cnpg()
        self.phase = "initial-database-persistence"
        persistence = self.database_persistence()
        print("Database ready; public persistence row inserted.", flush=True)
        self.create({"apiVersion": "v1", "kind": "ConfigMap",
                     "metadata": {"name": "api-settings", "namespace": self.workload},
                     "data": {"PGRST_DB_MAX_ROWS": "100"}})
        self.replace_policy()
        self.phase = "cli-successor-deploy"
        print("Upgrading to the API with aps deploy…", flush=True)
        successor = self.deploy_cli(self.successor_artifact, subject("successor"))
        if any(initial[key] == successor[key] for key in ["transition", "generation"]):
            raise demo.shared.QualificationError("CLI upgrade reused the initial allocation")
        self.phase = "successor-database-persistence"
        preserved = self.database_persistence(persistence)
        self.checks.append({"check": "public-row-and-volumes-preserved", "passed": True, **preserved})
        self.phase = "successor-http-readiness"
        before = self.workload_identity()
        first_http = self.performance_http(2, before)
        print("Upgrade committed; row and database/PVC identities preserved. API returns 2 rows.", flush=True)
        self.phase = "configmap-reaction"
        self.k("patch", "configmap/api-settings", "-n", self.workload, "--type=merge",
               "-p", json.dumps({"data": {"PGRST_DB_MAX_ROWS": "1"}}))
        after = self.workload_identity(previous_uid=before["podUid"])
        if (after["deploymentUid"] != before["deploymentUid"] or after["revision"] == before["revision"]
                or after["replicaSetUid"] == before["replicaSetUid"]):
            raise demo.shared.QualificationError("ConfigMap change did not produce the expected PostgREST rollout")
        second_http = self.performance_http(1, after)
        print("ConfigMap 100 → 1: new API Pod; HTTP returns 1 row.", flush=True)
        self.checks.extend([
            {"check": "postgrest-messages-live", "passed": True, **first_http},
            {"check": "configmap-driven-pod-turnover", "passed": True, "before": before, "after": after},
            {"check": "postgrest-messages-after-rollout", "passed": True, **second_http},
        ])
        self.check_input_reactions(after)
        print("Equal-data update: no rollout for 30 seconds.", flush=True)
        print("Synthetic Secret change: rollout completed; original Secret data restored.", flush=True)

    def run(self):
        passed = False
        try:
            self.prepare()
            self.phase = "cluster-create"
            self.create_cluster()
            self.phase = "bootstrap"
            self.bootstrap()
            self.measure()
            passed = True
        except Exception as error:
            self.primary_failure = {"phase": self.phase, "reason": demo.base.failure_reason(self, error)}
            raise
        finally:
            self.phase = "cleanup"
            self.finish(passed)
        if self.disposition != "deleted" or not self.cleanup_confirmed or self.admin.exists() or self.issuer.exists():
            raise demo.shared.QualificationError("CLI demo cleanup was not confirmed")
        self.require_inputs_unchanged()
        result = {"passed": True, "scope": "public aps deploy initial and upgrade on owned kind",
                  "releaseVerified": False, "productionEvidence": None,
                  "runnerSourceCommit": self.commit, "sourceArchiveDigest": demo.shared.digest_file(self.source_archive),
                  "frozenInputDigests": self.snapshot, "candidates": self.pair["candidates"],
                  "cliReports": self.reports, "checks": self.checks, "cluster": self.cluster,
                  "clusterDisposition": self.disposition, "issuerCredentialsRemoved": True}
        (self.output / "result.json").write_bytes(demo.shared.canonical(result))
        print("Demo complete; owned cluster and credentials removed.", flush=True)
        print(json.dumps({"passed": True, "scope": result["scope"], "result": str(self.output / "result.json")}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["aps", "initial-artifact", "successor-artifact", "trust-policy", "operator-manifest", "output"]:
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--operator-sha256", required=True)
    parser.add_argument("--image-pull-secret", action="append", default=[])
    runner = None
    try:
        runner = CliDemoRunner(parser.parse_args())
        runner.run()
    except Exception as error:
        if runner is not None:
            result = {"passed": False, "releaseVerified": False, "productionEvidence": None,
                      "phase": runner.phase, "primaryFailure": runner.primary_failure,
                      "failureReason": demo.base.failure_reason(runner, error),
                      "clusterDisposition": runner.disposition, "issuerCredentialsRemoved": not runner.issuer.exists()}
            (runner.output / "result.json").write_bytes(demo.shared.canonical(result))
        print("Public CLI demo failed; see the sanitized result.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
