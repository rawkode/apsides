"""Offline Prometheus window analysis; this does not produce release evidence."""
import argparse
import hashlib
import json
from importlib.metadata import version
import math
from pathlib import Path

from prometheus_client.parser import text_string_to_metric_families


class AnalysisError(Exception):
    """Only fixed diagnostic messages cross the CLI boundary."""


def unavailable(reason):
    return {"status": "unavailable", "reason": reason}


def parse_metrics(raw):
    result = {}
    try:
        for family in text_string_to_metric_families(raw.decode("utf-8")):
            for sample in family.samples:
                key = (sample.name, tuple(sorted(sample.labels.items())))
                if key in result:
                    raise AnalysisError("invalid metric file")
                result[key] = sample.value
    except (ValueError, UnicodeError, TypeError):
        raise AnalysisError("invalid metric file") from None
    return result


def series(scrapes, name):
    return [{labels: value for (metric, labels), value in scrape.items() if metric == name}
            for scrape in scrapes]


def deltas(values):
    if any(not value for value in values):
        return None, "missing metric series"
    if any(not math.isfinite(value) or value < 0 for row in values for value in row.values()):
        return None, "invalid metric value"
    if any(value.keys() != values[0].keys() for value in values[1:]):
        return None, "changing metric series"
    if any(current[key] < previous[key] for previous, current in zip(values, values[1:]) for key in previous):
        return None, "counter reset"
    return {key: values[-1][key] - values[0][key] for key in values[0]}, None


def counter(scrapes, name, duration):
    changes, reason = deltas(series(scrapes, name))
    if reason:
        return unavailable(reason)
    total = sum(changes.values())
    if not math.isfinite(total):
        return unavailable("numeric overflow")
    rate = total / duration
    if not math.isfinite(rate):
        return unavailable("numeric overflow")
    return {"status": "measured", "intervalDelta": total, "perSecond": rate,
            "seriesCount": len(changes)}


def watches(scrapes):
    values = [{labels: value for labels, value in row.items() if dict(labels).get("verb") == "WATCH"}
              for row in series(scrapes, "apiserver_longrunning_requests")]
    if any(not row for row in values):
        return unavailable("missing metric series")
    if any(not math.isfinite(value) or value < 0 for row in values for value in row.values()):
        return unavailable("invalid metric value")
    if any(row.keys() != values[0].keys() for row in values[1:]):
        return unavailable("changing metric series")
    totals = [sum(row.values()) for row in values]
    try:
        mean = math.fsum(totals) / len(totals)
    except OverflowError:
        return unavailable("numeric overflow")
    if not all(math.isfinite(value) for value in [*totals, mean]):
        return unavailable("numeric overflow")
    return {"status": "measured", "kind": "sampled concurrent WATCH gauge", "minimum": min(totals),
            "mean": mean, "maximum": max(totals), "samples": len(totals)}


def queues(scrapes):
    sums, reason = deltas(series(scrapes, "workqueue_queue_duration_seconds_sum"))
    if reason:
        return unavailable(reason)
    counts, reason = deltas(series(scrapes, "workqueue_queue_duration_seconds_count"))
    if reason:
        return unavailable(reason)
    if sums.keys() != counts.keys():
        return unavailable("queue sum and count labels differ")
    rows = []
    for labels in sorted(sums):
        row = {"labels": dict(labels), "durationSecondsDelta": sums[labels], "observationsDelta": counts[labels]}
        if counts[labels] == 0:
            row.update(unavailable("queue sum changed without observations" if sums[labels] != 0 else "no queue observations"))
        elif not math.isfinite(sums[labels] / counts[labels]):
            row.update(unavailable("numeric overflow"))
        else:
            row.update(status="measured", intervalMeanSeconds=sums[labels] / counts[labels])
        rows.append(row)
    return {"status": "measured", "kind": "workqueue interval observation mean", "series": rows}


def analyze(directory):
    directory = Path(directory).resolve()
    digests = {}
    def read(relative):
        path = directory / relative
        if path.is_symlink() or not path.is_file() or path.stat().st_size > 32 * 1024 * 1024:
            raise AnalysisError("missing or invalid input file")
        raw = path.read_bytes()
        digests[relative] = "sha256:" + hashlib.sha256(raw).hexdigest()
        return raw
    def unique_object(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise AnalysisError("duplicate document key")
            result[key] = value
        return result
    def document(relative):
        try:
            value = json.loads(read(relative), object_pairs_hook=unique_object)
        except (ValueError, UnicodeError):
            raise AnalysisError("invalid sample document") from None
        if not isinstance(value, dict):
            raise AnalysisError("invalid sample document")
        return value
    summary = document("summary.json")
    count = summary.get("samples")
    paths = summary.get("rawMetricPaths")
    if (summary.get("schemaVersion") != "apsides.dev/performance-samples/v1"
            or type(count) is not int or not 2 <= count <= 10000 or summary.get("errors") != []
            or not isinstance(paths, list) or len(paths) > 16
            or any(not isinstance(path, str) for path in paths) or len(set(paths)) != len(paths)):
        raise AnalysisError("incomplete sample window")
    expected = {f"sample-{index:04d}" for index in range(count)}
    if {path.name for path in directory.glob("sample-*")} != expected:
        raise AnalysisError("sample directory population differs")
    filenames = ["apiserver.prom", *[f"controller-{index}.prom" for index in range(len(paths))]]
    sources = {name: [] for name in filenames}
    invalid = set()
    timestamps = []
    for index in range(count):
        prefix = f"sample-{index:04d}/"
        if (directory / prefix).is_symlink():
            raise AnalysisError("invalid sample directory")
        sample = document(prefix + "sample.json")
        stamp, collection = sample.get("elapsedSeconds"), sample.get("collectionSeconds")
        if (sample.get("errors") != [] or any(type(value) not in [int, float] or not math.isfinite(value) or value < 0
                                               for value in [stamp, collection])
                or timestamps and stamp <= timestamps[-1]):
            raise AnalysisError("incomplete or unordered sample window")
        timestamps.append(stamp)
        for name in filenames:
            raw = read(prefix + name)
            try:
                sources[name].append(parse_metrics(raw))
            except AnalysisError:
                invalid.add(name)
    duration = timestamps[-1] - timestamps[0]
    declared = summary.get("durationSeconds")
    if type(declared) not in [int, float] or not math.isfinite(declared) or not math.isclose(duration, declared, rel_tol=1e-9):
        raise AnalysisError("sample duration differs from summary")
    reports = {}
    for name, scrapes in sources.items():
        if name in invalid:
            reports[name] = unavailable("invalid metric file")
        elif name == "apiserver.prom":
            reports[name] = {"concurrentWatches": watches(scrapes),
                             "requests": counter(scrapes, "apiserver_request_total", duration)}
        else:
            reports[name] = {"requests": counter(scrapes, "rest_client_requests_total", duration),
                             "queueDuration": queues(scrapes)}
    return {"schemaVersion": "apsides.dev/offline-metrics/v1", "completeSection26": False,
            "samples": count, "durationSeconds": duration, "sources": reports, "sourceDigests": digests,
            "controllerMetricPaths": paths,
            "method": {"analyzerDigest": "sha256:" + hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                       "parser": "prometheus-client", "parserVersion": version("prometheus-client"),
                       "requirementsDigest": "sha256:" + hashlib.sha256(Path(__file__).with_name("requirements.txt").read_bytes()).hexdigest()},
            "observerWarning": "API request totals include observers and cluster activity; native status and baseline metrics probes differ. WATCH values are concurrent gauges, not watch starts. Workqueue timing is not native queue timing."}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--samples", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        result = analyze(args.samples)
        with args.output.open("x") as output:
            output.write(json.dumps(result, indent=2, allow_nan=False) + "\n")
    except (AnalysisError, OSError, ValueError, TypeError, KeyError):
        print("offline metrics analysis failed")
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
