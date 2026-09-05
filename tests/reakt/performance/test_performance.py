import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest

from collect import container_usage, cpu_delta
from generate import archive, prepare
from stimulate import sequence_value

IMAGE = "example.invalid/image@sha256:" + "1" * 64


class PreparationTests(unittest.TestCase):
    def test_equivalent_populations_and_bounded_flux_scope(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "profile"
            profile = prepare(output, 4, 3, IMAGE, IMAGE, 5)
            self.assertEqual(profile["domains"], 4)
            inputs = json.loads((output / "inputs.json").read_text())["items"]
            self.assertEqual(len(inputs), 4)
            objects = json.loads((output / "flux.json").read_text())["items"]
            manager = next(obj for obj in objects if obj["kind"] == "Deployment" and obj["metadata"]["name"] == "benchmark-flux")
            args = manager["spec"]["template"]["spec"]["containers"][0]["args"]
            self.assertIn("--watch-all-namespaces=false", args)
            self.assertIn("--concurrent=1", args)
            self.assertNotIn("ClusterRole", [obj["kind"] for obj in objects])
            with tarfile.open(output / "bench-0/artifact.tar.gz") as payload:
                desired = json.load(payload.extractfile("resources.json"))["items"]
            self.assertEqual([obj["kind"] for obj in desired], ["Deployment", "ConfigMap", "ConfigMap", "ConfigMap"])
            source = (output / "bench-0/program.ts").read_text()
            self.assertEqual(source.count("new ConfigMap("), 3)
            self.assertIn('"maxConcurrentRequests": 1', source)

    def test_artifacts_are_stable_and_have_no_machine_metadata(self):
        first = archive([{"kind": "ConfigMap"}])
        self.assertEqual(first, archive([{"kind": "ConfigMap"}]))
        with tarfile.open(fileobj=io.BytesIO(first)) as payload:
            self.assertTrue(all(member.mtime == 0 and member.uid == 0 and member.gid == 0 for member in payload))

    def test_mutable_images_and_unbounded_population_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            for domains, image in [(33, IMAGE), (1, "example:latest")]:
                with self.assertRaises(ValueError):
                    prepare(Path(directory) / "profile", domains, 1, image, IMAGE, 5)


    def test_shared_sequence_is_identical_and_ambiguous_entries_fail(self):
        with tempfile.TemporaryDirectory() as directory:
            first = prepare(Path(directory) / "flux", 4, 1, IMAGE, IMAGE, 5)
            second = prepare(Path(directory) / "reakt", 4, 1, IMAGE, IMAGE, 5)
            self.assertEqual(first["inputSequence"], second["inputSequence"])
            values = [sequence_value(first, iteration, "bench-0") for iteration in range(100)]
            self.assertEqual(len(set(values)), 100)
            first["inputSequence"].append(first["inputSequence"][0])
            with self.assertRaises(ValueError):
                sequence_value(first, 0, "bench-0")
            with self.assertRaises(ValueError):
                sequence_value(second, 0, "not-in-profile")


class MeasurementTests(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
