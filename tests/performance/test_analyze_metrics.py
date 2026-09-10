"""Offline regressions for interval semantics and incomplete scrape evidence."""
import json
from pathlib import Path
import tempfile
import unittest

import analyze_metrics as metrics


def scrape(counter=10, watch=3, queue_sum=2, queue_count=1, extra=""):
    return metrics.parse_metrics((
        '# TYPE apiserver_request_total counter\n'
        f'apiserver_request_total{{verb="GET"}} {counter}\n'
        '# TYPE apiserver_longrunning_requests gauge\n'
        f'apiserver_longrunning_requests{{verb="WATCH"}} {watch}\n'
        'apiserver_longrunning_requests{verb="GET"} 99\n'
        '# TYPE workqueue_queue_duration_seconds histogram\n'
        f'workqueue_queue_duration_seconds_sum{{name="queue"}} {queue_sum}\n'
        f'workqueue_queue_duration_seconds_count{{name="queue"}} {queue_count}\n' + extra).encode())


class MetricTests(unittest.TestCase):
    def test_gauge_is_sampled_concurrency_not_counter_delta(self):
        rows = [scrape(watch=8), scrape(watch=2), scrape(watch=5)]
        self.assertEqual(metrics.watches(rows), {"status": "measured", "kind": "sampled concurrent WATCH gauge",
                         "minimum": 2, "mean": 5, "maximum": 8, "samples": 3})

    def test_counter_rate_and_per_label_queue_mean(self):
        rows = [scrape(), scrape(counter=14, queue_sum=8, queue_count=3)]
        self.assertEqual(metrics.counter(rows, "apiserver_request_total", 2)["perSecond"], 2)
        self.assertEqual(metrics.queues(rows)["series"][0]["intervalMeanSeconds"], 3)
        self.assertEqual(metrics.counter(rows, "rest_client_requests_total", 2)["status"], "unavailable")

    def test_middle_reset_missing_and_new_series_are_not_zero(self):
        for rows, reason in [([scrape(10), scrape(1), scrape(20)], "counter reset"),
                             ([scrape(), {}, scrape(20)], "missing metric series"),
                             ([scrape(), scrape(extra='apiserver_request_total{verb="PATCH"} 1\n')], "changing metric series")]:
            result = metrics.counter(rows, "apiserver_request_total", 2)
            self.assertEqual(result, metrics.unavailable(reason))

    def test_queue_missing_and_no_observations_unavailable(self):
        self.assertEqual(metrics.queues([{}, {}]), metrics.unavailable("missing metric series"))
        self.assertEqual(metrics.queues([scrape(), scrape()])["series"][0]["reason"], "no queue observations")
        self.assertEqual(metrics.queues([scrape(), scrape(queue_sum=3)])["series"][0]["reason"], "queue sum changed without observations")

    def test_parser_handles_escaped_labels(self):
        result = metrics.parse_metrics(b'metric{label="quote\\\" and slash\\\\ and newline\\n"} 2\n')
        self.assertIn(("metric", (("label", 'quote" and slash\\ and newline\n'),)), result)

    def test_invalid_metrics_are_static_errors(self):
        for raw in [b'metric 1\nmetric 2\n', b'metric{private="PRIVATE" broken']:
            with self.assertRaisesRegex(metrics.AnalysisError, '^invalid metric file$'):
                metrics.parse_metrics(raw)

    def test_unrelated_nonfinite_sentinel_does_not_hide_selected_metrics(self):
        row = scrape(extra='rest_client_exec_plugin_ttl_seconds +Inf\n')
        self.assertEqual(metrics.watches([row, row])["status"], "measured")
        self.assertEqual(metrics.counter([row, row], "apiserver_request_total", 1)["status"], "measured")
        bad = scrape(counter="NaN")
        self.assertEqual(metrics.counter([row, bad], "apiserver_request_total", 1), metrics.unavailable("invalid metric value"))
        self.assertEqual(metrics.watches([scrape(watch="+Inf")]), metrics.unavailable("invalid metric value"))

    def test_duplicate_json_and_symlink_sample_directory_rejected(self):
        for mode in ["duplicate", "symlink"]:
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory); self.window(root)
                if mode == "duplicate":
                    (root / "sample-0000/sample.json").write_text('{"errors":[],"errors":[]}')
                else:
                    (root / "sample-0000").rename(root / "elsewhere")
                    (root / "sample-0000").symlink_to(root / "elsewhere", target_is_directory=True)
                with self.assertRaises(metrics.AnalysisError):
                    metrics.analyze(root)

    def window(self, root):
        summary = {"schemaVersion": "apsides.dev/performance-samples/v1", "samples": 3,
                   "durationSeconds": 2, "rawMetricPaths": [], "errors": []}
        (root / "summary.json").write_text(json.dumps(summary))
        for index in range(3):
            folder = root / f"sample-{index:04d}"; folder.mkdir()
            (folder / "sample.json").write_text(json.dumps({"elapsedSeconds": index, "collectionSeconds": .1, "errors": []}))
            (folder / "apiserver.prom").write_text(f'apiserver_request_total {index}\n')

    def test_window_binds_every_input_and_cannot_claim_section26(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); self.window(root)
            report = metrics.analyze(root)
            self.assertEqual(len(report["sourceDigests"]), 7)
            self.assertFalse(report["completeSection26"])
            self.assertEqual(report["sources"]["apiserver.prom"]["requests"]["intervalDelta"], 2)
            (root / "sample-0001/apiserver.prom").write_text('metric 1\nmetric 2\n')
            self.assertEqual(metrics.analyze(root)["sources"]["apiserver.prom"], metrics.unavailable("invalid metric file"))

    def test_missing_and_unordered_samples_rejected(self):
        for mode in ["missing", "unordered", "failed"]:
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory); self.window(root)
                path = root / "sample-0001/sample.json"
                if mode == "missing":
                    path.unlink()
                else:
                    path.write_text(json.dumps({"elapsedSeconds": 0 if mode == "unordered" else 1,
                                                "collectionSeconds": .1, "errors": ["failure"] if mode == "failed" else []}))
                with self.assertRaises(metrics.AnalysisError):
                    metrics.analyze(root)


if __name__ == "__main__":
    unittest.main()
