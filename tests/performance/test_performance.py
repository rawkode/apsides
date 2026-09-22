import json
from pathlib import Path
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

import collect as collector

from collect import container_usage, cpu_delta


class MeasurementTests(unittest.TestCase):
    def test_command_failures_never_expose_process_output(self):
        failures = [OSError("PRIVATE-SENTINEL"),
                    collector.subprocess.TimeoutExpired("PRIVATE-SENTINEL", 30, output="PRIVATE-SENTINEL")]
        for failure in failures:
            with patch.object(collector.subprocess, "run", side_effect=failure):
                with self.assertRaises(RuntimeError) as observed:
                    collector.command(Path("config"), "get", "nodes")
            self.assertNotIn("PRIVATE-SENTINEL", str(observed.exception))
        with patch.object(collector.subprocess, "run", return_value=SimpleNamespace(returncode=1, stderr="PRIVATE-SENTINEL")):
            with self.assertRaises(RuntimeError) as observed:
                collector.command(Path("config"), "get", "nodes")
        self.assertNotIn("PRIVATE-SENTINEL", str(observed.exception))

    def test_all_metric_failure_sources_are_redacted(self):
        def command(kubeconfig, *args):
            if args == ("get", "nodes", "-o", "json"):
                return '{"items": [{"metadata": {"name": "node"}}]}'
            if args[-1].endswith("/stats/summary"):
                return "PRIVATE-SENTINEL"
            raise RuntimeError("PRIVATE-SENTINEL")
        with tempfile.TemporaryDirectory() as directory:
            args = SimpleNamespace(output=Path(directory) / "out", kubeconfig=None, samples=2, interval=0.1,
                                   namespace=["default"], metrics_path=["/metrics"], runtime_status=[], label="test")
            with patch.object(collector, "command", side_effect=command), patch.object(collector.time, "sleep"):
                collector.collect(args)
            report = json.loads((args.output / "summary.json").read_text())
            self.assertEqual(len(report["errors"]), 6)
            self.assertFalse(report["cpu"]["completePopulation"])
            self.assertTrue(all("PRIVATE-SENTINEL" not in path.read_text()
                                for path in args.output.rglob("*") if path.is_file()))

    def row(self):
        return {"namespace": "work", "pod": "controller", "podUid": "uid", "container": "envoy",
                "containerStartTime": "2026-09-05T00:00:00Z", "cpuCoreNanoSeconds": 100}

    def test_restarts_resets_empty_and_removed_populations_are_not_improvements(self):
        first = self.row()
        for change in [{"cpuCoreNanoSeconds": 99}, {"containerStartTime": "later", "cpuCoreNanoSeconds": 200}, {"podUid": "replacement"}]:
            end = {**first, **change}
            report = cpu_delta([first], [end], 1)
            self.assertFalse(report["completePopulation"])
            self.assertIsNone(report["containers"][0]["meanCpuCores"])
        self.assertFalse(cpu_delta([], [], 1)["completePopulation"])
        self.assertFalse(cpu_delta([first], [], 1)["completePopulation"])

    def test_standard_counter_units_and_all_container_accounting(self):
        first = self.row()
        end = {**first, "cpuCoreNanoSeconds": 1000000100}
        self.assertEqual(cpu_delta([first], [end], 2)["containers"][0]["meanCpuCores"], 0.5)
        summary = {"pods": [{"podRef": {"namespace": "work", "name": "proxy", "uid": "uid"}, "containers": [
            {"name": "envoy", "startTime": "start", "cpu": {"usageCoreNanoSeconds": 1}, "memory": {"rssBytes": 2}},
            {"name": "admin", "startTime": "start", "cpu": {"usageCoreNanoSeconds": 3}, "memory": {"rssBytes": 4}}]}]}
        rows = container_usage(summary, ["work"])
        self.assertEqual({row["container"] for row in rows}, {"envoy", "admin"})
        self.assertEqual(sum(row["rssBytes"] for row in rows), 6)


class RuntimeMeasurements(unittest.TestCase):
    route = "demo=/api/v1/namespaces/control/pods/runtime:9090/proxy/status"

    def status(self, count=2, total=10):
        return {"schemaVersion": "apsides.dev/runtime-status/v1", "domain": "demo", "buildDigest": "sha256:" + "a" * 64, "generation": "g1",
                "phase": "ready", "cache": "synchronized", "blockedTargets": 0, "reconciliations": 3, "errors": 0,
                "applyCacheEntries": 4, "applyCacheSerializedBytes": 100, "pendingCoalescedTriggers": 0,
                "triggerToCycleMicrosCount": count, "triggerToCycleMicrosSum": total, "triggerToCycleMicrosMax": 8,
                "conditions": [{"message": "PRIVATE-SENTINEL"}], "lastReasonCode": "PRIVATE-SENTINEL"}

    def pod(self):
        return {"metadata": {"name": "runtime", "namespace": "control", "uid": "uid"},
                "status": {"containerStatuses": [{"name": "controller", "containerID": "containerd://abc",
                    "restartCount": 0, "state": {"running": {"startedAt": "now"}}}]}}

    def sampled(self, **changes):
        status = self.status()
        status.update(changes)
        with patch.object(collector, "command", side_effect=[json.dumps(self.pod()), json.dumps(status), json.dumps(self.pod())]):
            return collector.runtime_sample(None, collector.runtime_target(self.route))

    def test_projection_excludes_raw_diagnostics(self):
        row = self.sampled()
        self.assertNotIn("PRIVATE-SENTINEL", json.dumps(row))
        self.assertEqual(row["healthReason"], "other")
        self.assertNotIn("conditions", row)
        for changes in [{"schemaVersion": "other"}, {"triggerToCycleMicrosCount": True}, {"applyCacheEntries": -1},
                        {"generation": "bad secret value"}, {"domain": "other"}, {"pendingCoalescedTriggers": 2}]:
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                self.sampled(**changes)

    def test_pod_replacement_during_read_is_rejected(self):
        replacement = self.pod()
        replacement["metadata"]["uid"] = "replacement"
        with patch.object(collector, "command", side_effect=[json.dumps(self.pod()), json.dumps(self.status()), json.dumps(replacement)]):
            with self.assertRaises(ValueError):
                collector.runtime_sample(None, collector.runtime_target(self.route))

    def test_health_projection_is_closed_and_records_degradation(self):
        row = self.sampled(phase="degraded", cache="pending", errors=2, blockedTargets=1)
        self.assertEqual((row["phase"], row["cache"], row["errors"], row["blockedTargets"]),
                         ("degraded", "pending", 2, 1))
        self.assertNotIn("PRIVATE-SENTINEL", json.dumps(row))
        for reason, expected in [(None, None), ("watch-recovery", "watch-recovery"),
                                 ({"private": "PRIVATE-SENTINEL"}, "other")]:
            self.assertEqual(self.sampled(lastReasonCode=reason)["healthReason"], expected)
        for changes in [{"phase": "PRIVATE-SENTINEL"}, {"cache": "PRIVATE-SENTINEL"},
                        {"phase": None}, {"errors": True}, {"blockedTargets": -1},
                        {"reconciliations": 2**64}]:
            with self.subTest(changes=changes), self.assertRaisesRegex(ValueError, "runtime health projection is invalid"):
                self.sampled(**changes)

    def test_interval_counters_and_lifetime_max_are_distinct(self):
        first = self.sampled()
        last = self.sampled(triggerToCycleMicrosCount=4, triggerToCycleMicrosSum=18)
        report = collector.runtime_report([{"runtime": [first]}, {"runtime": [last]}], [collector.runtime_target(self.route)])
        row = report["domains"][0]
        self.assertTrue(report["completePopulation"])
        self.assertEqual((row["triggerSamples"], row["triggerToCycleMicrosMean"], row["processLifetimeTriggerToCycleMicrosMax"]), (2, 4, 8))
        self.assertNotIn("p99", json.dumps(report))

    def test_missing_middle_reset_or_changed_generation_invalidates_interval(self):
        first = self.sampled()
        target = collector.runtime_target(self.route)
        for changed in [None, {**first, "generation": "g2"}, {**first, "podUid": "new"},
                        {**first, "restartCount": 1}, {**first, "triggerToCycleMicrosCount": 1},
                        {**first, "buildDigest": "sha256:" + "b" * 64}]:
            samples = [{"runtime": [first]}, {"runtime": [] if changed is None else [changed]}, {"runtime": [first]}]
            self.assertFalse(collector.runtime_report(samples, [target])["completePopulation"])

    def test_routes_reject_services_and_query_injection(self):
        for route in [self.route + "?secret=x", self.route.replace("/pods/", "/services/"), "demo=https://example/status"]:
            with self.assertRaises(ValueError):
                collector.runtime_target(route)

    def test_runtime_namespace_must_be_in_measured_container_population(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "out"
            args = SimpleNamespace(output=output, namespace=["other"], runtime_status=[self.route])
            with patch.object(collector, "command") as command, self.assertRaises(ValueError):
                collector.collect(args)
            command.assert_not_called()
            self.assertFalse(output.exists())

    def test_collect_integration_saves_selected_values_and_redacts_failures(self):
        for fails in [False, True]:
            status_calls = 0
            def command(kubeconfig, *args):
                nonlocal status_calls
                if args == ("get", "nodes", "-o", "json"):
                    return '{"items": []}'
                if "pod" in args:
                    return json.dumps(self.pod())
                if args[-1].endswith("/status"):
                    status_calls += 1
                    if fails and status_calls == 2:
                        raise RuntimeError("PRIVATE-SENTINEL")
                    return json.dumps(self.status(count=status_calls * 2, total=status_calls * 10))
                return "metric 1\n"
            with tempfile.TemporaryDirectory() as directory:
                args = SimpleNamespace(output=Path(directory) / "out", kubeconfig=None, samples=2, interval=0.1,
                                       namespace=["control"], metrics_path=[], runtime_status=[self.route], label="test")
                with patch.object(collector, "command", side_effect=command), patch.object(collector.time, "sleep"):
                    collector.collect(args)
                report = json.loads((args.output / "summary.json").read_text())
                self.assertEqual(report["runtimeDiagnostics"]["completePopulation"], not fails)
                self.assertFalse(report["completeSection26"])
                self.assertTrue(all("PRIVATE-SENTINEL" not in path.read_text() for path in args.output.rglob("*") if path.is_file()))


if __name__ == "__main__":
    unittest.main()
