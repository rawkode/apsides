"""Analyze saved kubelet resource samples; never produces release evidence."""
import argparse
from datetime import datetime
import hashlib
import json
import math
from pathlib import Path

GROUPS = {
    "native": {("apsides-system", name): "controllerSupport" for name in ("controller", "envoy", "admin")},
    "baseline": {("default", name): "controllerSupport" for name in ("manager", "reloader", "http")},
}
for mapping in GROUPS.values():
    mapping.update({("cnpg-system", "manager"): "sharedInfrastructure", ("default", "postgres"): "sharedInfrastructure", ("default", "api"): "application"})


class AnalysisError(ValueError):
    pass


def require(ok, message):
    if not ok:
        raise AnalysisError(message)


def number(value):
    return type(value) in (int, float) and math.isfinite(value) and value >= 0


def identity(row):
    return tuple(row.get(k) for k in ("namespace", "pod", "podUid", "container", "containerStartTime"))


def unavailable(reason):
    return {"status": "unavailable", "reason": reason}


def statistics(values):
    require(all(number(v) for v in values), "invalid resource value")
    return {"minimumBytes": min(values), "sampleMeanBytes": math.fsum(v / len(values) for v in values), "maximumSampledBytes": max(values)}


def group_report(samples, group):
    rows = [{identity(r): r for r in sample if group == "fullPopulation" or r["group"] == group} for sample in samples]
    memory = {}
    for field in ("rssBytes", "workingSetBytes"):
        memory[field] = (statistics([sum(r[field] for r in s.values()) for s in rows])
                         if all(s and all(number(r.get(field)) for r in s.values()) for s in rows)
                         else unavailable("missing memory sample"))
    cpu = unavailable("missing or changing container population")
    if rows[0] and all(s.keys() == rows[0].keys() for s in rows):
        valid = all(type(s[k].get("cpuCoreNanoSeconds")) is int and s[k]["cpuCoreNanoSeconds"] >= 0 for s in rows for k in rows[0])
        valid = valid and all(b[k]["cpuCoreNanoSeconds"] >= a[k]["cpuCoreNanoSeconds"] for a, b in zip(rows, rows[1:]) for k in rows[0])
        if valid:
            containers = []
            for key in rows[0]:
                times = [s[key].get("cpuTime") for s in rows]
                if any(t is None for t in times) or times[-1] <= times[0] or any(b < a or (b == a and current[key]["cpuCoreNanoSeconds"] != previous[key]["cpuCoreNanoSeconds"]) for a, b, previous, current in zip(times, times[1:], rows, rows[1:])):
                    break
                delta = rows[-1][key]["cpuCoreNanoSeconds"] - rows[0][key]["cpuCoreNanoSeconds"]
                containers.append({"identity": list(key), "cpuSeconds": delta / 1e9,
                                   "kubeletIntervalSeconds": times[-1] - times[0], "meanCpuCores": delta / 1e9 / (times[-1] - times[0])})
            if len(containers) == len(rows[0]):
                cpu = {"status": "measured", "containers": containers,
                       "sumContainerMeanCpuCores": math.fsum(c["meanCpuCores"] for c in containers),
                       "precision": "Each container uses its own kubelet CPU timestamp interval; endpoints may differ between containers."}
            else:
                cpu = unavailable("missing, decreasing, or inconsistent kubelet CPU timestamps")
        else:
            cpu = unavailable("missing or reset CPU counter")
    return {"cpu": cpu, "memory": memory}


def analyze(directory, profile):
    directory = Path(directory)
    require(profile in GROUPS and directory.is_dir() and not directory.is_symlink(), "invalid analysis input")
    digests = {}
    def read(relative):
        path = directory / relative
        require(not path.is_symlink() and not path.parent.is_symlink(), "symlink input")
        require(path.is_file() and 0 < path.stat().st_size <= 32 * 1024 * 1024, "invalid bounded input")
        raw = path.read_bytes()
        require(len(raw) <= 32 * 1024 * 1024, "input exceeded bound")
        digests[relative] = "sha256:" + hashlib.sha256(raw).hexdigest()
        def unique(pairs):
            result = {}
            for k, v in pairs:
                require(k not in result, "duplicate JSON field")
                result[k] = v
            return result
        return json.loads(raw, object_pairs_hook=unique)
    summary = read("summary.json")
    namespaces = summary.get("namespaces")
    require(isinstance(namespaces, list) and bool(namespaces) and all(isinstance(n, str) and n.strip() for n in namespaces) and len(set(namespaces)) == len(namespaces), "invalid namespace population")
    count = summary.get("samples")
    require(type(count) is int and 2 <= count <= 10000 and summary.get("errors") == [], "incomplete window")
    require({p.name for p in directory.glob('sample-*')} == {f'sample-{i:04d}' for i in range(count)}, "sample population mismatch")
    samples, stamps = [], []
    expected_names = set(GROUPS[profile])
    for i in range(count):
        prefix = f"sample-{i:04d}/"
        sample = read(prefix + "sample.json")
        stamp = sample.get("elapsedSeconds")
        require(sample.get("errors") == [] and number(stamp) and (not stamps or stamp > stamps[-1]), "invalid sample interval")
        stamps.append(stamp)
        raw_rows = {}
        for path in sorted((directory / prefix).glob('*.summary.json')):
            node = read(prefix + path.name)
            for pod in node.get("pods", []):
                ref = pod["podRef"]
                if ref["namespace"] not in summary["namespaces"]:
                    continue
                for c in pod.get("containers", []):
                    row = {"namespace": ref["namespace"], "pod": ref["name"], "podUid": ref["uid"], "container": c["name"], "containerStartTime": c.get("startTime")}
                    key = identity(row)
                    require(key not in raw_rows, "duplicate kubelet container")
                    row.update(cpuCoreNanoSeconds=c.get("cpu", {}).get("usageCoreNanoSeconds"), rssBytes=c.get("memory", {}).get("rssBytes"), workingSetBytes=c.get("memory", {}).get("workingSetBytes"))
                    try:
                        parsed = datetime.fromisoformat(c['cpu']['time'].replace('Z', '+00:00'))
                        row['cpuTime'] = parsed.timestamp() if parsed.tzinfo else None
                    except (KeyError, ValueError, TypeError):
                        row['cpuTime'] = None
                    raw_rows[key] = row
        rows, seen, names = [], set(), []
        for projected in sample['usage']:
            key = identity(projected)
            require(all(isinstance(x, str) and x for x in key) and key not in seen, "invalid or duplicate container identity")
            seen.add(key)
            name = (projected['namespace'], projected['container'])
            require(name in expected_names, "unclassified container")
            names.append(name)
            require(key in raw_rows, "missing raw kubelet container")
            row = raw_rows[key]
            require(all(row[k] == projected.get(k) for k in ('cpuCoreNanoSeconds', 'rssBytes', 'workingSetBytes')), "kubelet projection differs")
            rows.append({**row, "group": GROUPS[profile][name]})
        require(set(names) == expected_names and len(names) == len(set(names)), "missing or ambiguous container group")
        require(seen == raw_rows.keys(), "unprojected kubelet container")
        samples.append(rows)
    require(number(summary.get('durationSeconds')) and math.isclose(stamps[-1]-stamps[0], summary['durationSeconds'], abs_tol=1e-6), "window duration mismatch")
    return {"schemaVersion": "apsides.dev/resource-analysis/v1", "profile": profile, "samples": count, "namespaces": namespaces,
            "populationScope": "One CNPG domain: fullPopulation is the sampled namespace/container set, excluding the Kubernetes control plane; not a cluster-wide total.",
            "durationSeconds": stamps[-1]-stamps[0], "groups": {g: group_report(samples, g) for g in ('controllerSupport', 'sharedInfrastructure', 'application', 'fullPopulation')},
            "memorySemantics": "Sampled RSS and working set; no between-sample peak, cache size, or allocator attribution.",
            "sourceDigests": digests, "analyzerDigest": "sha256:" + hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), "completeSection26": False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--window', type=Path, required=True)
    parser.add_argument('--profile', choices=GROUPS, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    try:
        result = analyze(args.window, args.profile)
        with args.output.open('x') as f:
            json.dump(result, f, indent=2, allow_nan=False)
    except (ValueError, KeyError, TypeError, OSError, OverflowError):
        parser.exit(1, 'Resource analysis failed; no comparison acceptance is claimed.\n')


if __name__ == '__main__':
    main()
