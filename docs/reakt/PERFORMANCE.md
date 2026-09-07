# Performance comparison contract

`tests/reakt/performance` prepares and measures a scoped general-purpose Flux
baseline. It currently provides preparation, installation, raw collection, and
input-change stimulation. It has **not completed a live comparative benchmark**.
No speed, memory, scalability, or complete §26 claim follows from its unit tests.

## Equivalent work

Run each controller on a separate fresh cluster made from the same pinned kind
node image, with identical CPU/memory allocation and no other test activity.
Use one and four domains, then increase domain count only after both runs pass.
Each domain has one observed ConfigMap, one managed Deployment, and the same
number of additional managed ConfigMaps. All domains occupy the same workload
namespace. Flux has one manager and one Kustomization per domain. Apsides has one
native controller and one credential-bearing proxy per domain. Both must use
the profile's reconciliation interval, one concurrent apply, identical object
data, and the same input mutations. Apsides's production staging configuration
must be checked against this profile; generating the source does not set its
runtime polling configuration.

The initial profile uses zero workload replicas. It measures controller work and
template changes without application startup noise. It does not measure Pod
rollout duration. A separate one-replica workload profile must use the same
locally imported immutable fixture image and readiness behavior on both sides.

Flux substitutes the input value into a template annotation. Apsides computes its
standard revision from the input. The revision bytes and annotation key differ;
the intended effect is one template change per changed input value. Equal-value
replays must produce no template change. Input-to-template timing includes API
and observation overhead and is not queue latency.

Flux watches only its runtime namespace. Its ConfigMap/Secret label predicate
filters delivered events; the metadata informer cache may also retain unlabeled
objects in that namespace. Apsides watches exact names. These are equivalent
declared object populations, not identical watch implementations or equivalent
RBAC strength. Measure the additional watch connections and control resources
instead of removing them. Flux's namespace permissions are broader than Apsides's
published exact-name plans. Both boundaries are reported with the results.

## Pinned baseline

The checked-in `pins.json` binds Flux kustomize-controller v1.9.4, source commit
`1324b615da01038e2859cbcde604bd5434f21933`, its OCI index, and the SHA-256 of the
official CRD artifacts. The source-controller API CRDs match the v1.9.0 API
dependency in that Flux release. No source-controller process is installed.
The installer checks downloaded CRD bytes before applying them.

This follows the official [controller options](https://fluxcd.io/flux/components/kustomize/options/),
[substitution contract](https://fluxcd.io/flux/components/kustomize/kustomizations/#post-build-variable-substitution),
and [v1.9.4 release](https://github.com/fluxcd/kustomize-controller/releases/tag/v1.9.4).
The image and source pins identify the measured software; this harness does not
perform an independent signature verification of Flux images.

A standard Python HTTP server serves deterministic tar/gzip artifacts from a
ConfigMap. GitRepository objects have explicitly seeded synthetic status; they
are **not signed Git sources**, and no Git polling or source-controller cost is
claimed. Include the artifact server's CPU, memory, requests, image size, and
startup in baseline totals. Also publish controller-only totals separately.
An end-to-end Git distribution comparison requires a separate source-controller
profile with its costs included.

## Preparation and operation

### CNPG production workload

`cnpg_baseline.py` prepares a single-domain Flux and Reloader comparison from
the production CNPG plan, preserving its four desired objects, replicas and
application images. It adds only static named ConfigMap/Secret reload annotations
to the Deployment. Reloader is pinned to v1.4.21, source commit
`392be733e05b8e52c94199263abfd0f7632faab1`, and an immutable OCI image in
`pins.json`. This does not authenticate the upstream image publisher.

```sh
cuenv task cnpgBaselineTest
cuenv exec -- python3 tests/reakt/performance/cnpg_baseline.py \
  --plan "$CNPG_RELEASE/plan.json" --plan-sha256 "$CNPG_PLAN_SHA256" \
  --output work/cnpg-baseline --artifact-server-image "$PYTHON_IMAGE"
```

Use the plan hash from the verified release's file manifest and a digest-pinned
Python image. Output must be fresh and Git-ignored. The profile records source,
IR and archive digests and fixes reconciliation to one second, matching current
platform staging. Runtime configuration must be checked before measurement.
Flux applies resources sequentially; its concurrency setting does not imply
the same global request budget as Apsides's named watches.

The output is preparation only: it does not install controllers, validate
readiness, verify signatures, or produce release evidence. The fixture installer
below is not a CNPG installer. CNPG and Flux CRDs, the operator, named inputs and
healthy workloads must be established before the steady-state comparison.
Startup and missing-input equivalence remain outside this prepared profile.

`run_cnpg_baseline.py` checks the workload prerequisites in an owned disposable
kind cluster. Run it from a clean source revision with the prepared bundle and
the pinned operator and Flux CRD files:

```sh
cuenv task cnpgBaselineLiveTest
cuenv exec -- python3 tests/reakt/performance/run_cnpg_baseline.py \
  --prepared work/cnpg-baseline --plan "$CNPG_RELEASE/plan.json" \
  --operator-manifest "$CNPG_OPERATOR_MANIFEST" \
  --flux-crd "$FLUX_KUSTOMIZE_CRDS" --flux-crd "$FLUX_SOURCE_CRDS" \
  --output work/cnpg-baseline-live
```

The runner regenerates and compares the prepared inputs before installation,
checks workload readiness, changes `api-settings`, observes an actual equal-data
update without a rollout for at least 30 seconds, and probes Secret reactions
with a temporary key. It verifies restoration of the original Secret data and
cluster/container inventory, then removes its kubeconfigs. This is a prerequisite
check, not a benchmark receipt or proof of startup/missing-input equivalence.

Reloader publishes an unkeyed Secret hash in its runtime rollout annotation;
Apsides uses an aggregate keyed revision. Do not retain that annotation, live
Deployment templates, Secret values or Secret hashes in benchmark artifacts.
ConfigMap `binaryData` must remain empty on both sides, and input changes must
target `data`. These differences and the namespace-wide baseline read authority
are explicit comparison boundaries, not security-equivalence claims.

### Generic fixture profile

Preparation takes immutable fixture and Python server image references. It does
not pull images, build Apsides controllers, or create clusters:

```sh
python3 tests/reakt/performance/generate.py \
  --output work/benchmark-4 --domains 4 --managed 8 --interval 5 \
  --fixture-image "$FIXTURE_IMAGE" --artifact-server-image "$PYTHON_IMAGE"
```

Each `bench-N/program.tsx` passes through the normal source-lock, source-build,
native packaging, and candidate signing pipeline. Apsides provisioning must use
the test-support candidate authorization and real platform proxy staging, with
the benchmark namespace, domain, and cluster bindings. An in-process fixture
runtime or a direct-token runtime is not an equivalent replacement.

The baseline installer requires an existing disposable cluster whose observed
`kube-system` UID equals `--cluster-uid` and whose protected `reakt-test-support`
marker binds `reakt-disposable-benchmark`, `reakt-test-benchmark`, and
`reakt-test-benchmark-control`. It checks the marker before mutations. Create
this marker only as part of fresh-cluster bootstrap, using the same explicit
test-target protocol as the platform lifecycle suite. Import the pinned images
before installation and record CRI image identities.

```sh
python3 tests/reakt/performance/install_flux.py \
  --profile work/benchmark-4 --kubeconfig "$BENCHMARK_KUBECONFIG" \
  --cluster-uid "$BENCHMARK_CLUSTER_UID"
python3 tests/reakt/performance/collect.py \
  --kubeconfig "$BENCHMARK_KUBECONFIG" --output work/flux-idle \
  --label flux-4-idle --namespace reakt-test-benchmark \
  --namespace reakt-test-benchmark-control --samples 61 --interval 5
python3 tests/reakt/performance/stimulate.py \
  --profile work/benchmark-4 --kubeconfig "$BENCHMARK_KUBECONFIG" \
  --cluster-uid "$BENCHMARK_CLUSTER_UID" --controller flux \
  --output work/flux-change-latency
```

Use `--metrics-path` with the exact Kubernetes Pod proxy path for Flux's metrics
endpoint to preserve workqueue histograms. Record the actual Pod name and UID.
For Apsides, repeat `--runtime-status` for each measured domain:

```sh
# Add to collect.py, using the actual controller Pod and diagnostics port:
--runtime-status "$DOMAIN=/api/v1/namespaces/$CONTROL_NAMESPACE/pods/$CONTROLLER_POD:$DIAGNOSTICS_PORT/proxy/status"
```

Include that control namespace in `--namespace`. The collector checks Pod and
container identity before and after each status read, records only selected
numeric metrics and public identities, and rejects missing samples, identity
changes or counter resets across the interval. Cache measurements are retained
apply-cache entries and serialized bytes. Scheduling reports interval count,
sum and mean, with a separately labelled process-lifetime maximum. These are
neither allocator heap measurements nor latency percentiles. Unit and mocked
collection tests pass; a live paired measurement remains pending.

Run collection concurrently with stimulation in the active phase; retain an
idle phase before and after. The stimulus currently measures convergence from
fresh unequal inputs; equal-value and relist-storm phases remain integration
work rather than implicitly passing cases.

The profile contains one deterministic sequence indexed by iteration and domain;
both controllers consume those exact values. Apsides stimulation requires the
actual compiled IR for each domain and the existing core evaluator helper:

```sh
cargo build -p compiler-core --example benchmark_revision --locked --offline
# Add these options to stimulate.py when --controller is reakt:
# --revision-helper target/debug/examples/benchmark_revision
# --compiled-ir-root PATH_CONTAINING_BENCH_N_IR_JSON_DIRECTORIES
```

The helper accepts only the fixture's single observed public ConfigMap and
returns the precise expected revision from `compiler_core::evaluate_rollout`.
Unrelated annotation churn does not complete a sample. Each sample records its
input, expected revision, actual revision, compiled IR identity, and the helper
and profile hashes. This is an exact convergence check using the production
primitive; it is not independent reimplementation evidence for that primitive.

## Measurements and release gate

Preserve raw samples, actual image IDs, source/artifact digests, resolved runtime
configuration, cluster versions, hardware allocation, and command durations.
Report per-domain and aggregate results; include every Apsides native, Envoy, and
admin container. The namespace-wide collector also captures the Flux server.
Separate application workload containers and measured support containers in
analysis without dropping either from the full total.

| Required measurement | Evidence and current boundary |
| --- | --- |
| Controller CPU and memory | Kubelet summary CPU cumulative counters, RSS, working set for every container. Means use collection wall time; retain raw kubelet sample timestamps for precision analysis. Restart, UID change, missing samples, and counter reset invalidate the interval. |
| Total cluster watch connections | Raw API server `apiserver_longrunning_requests` series with watch verb, before/steady/during/after. Preserve total cluster count and idle-cluster baseline; do not count watch-start requests as concurrent connections. |
| API request rate | Raw API server request counter deltas divided by measured duration; report cluster total and idle baseline. Kubelet/API scraping itself has observer overhead and must be identical for both runs. |
| Cache size | Apsides exports apply-cache entry count and serialized bytes. Label that representation; it is not allocator heap size or total cache memory. The collector accepts pinned runtime status routes through `--runtime-status`; the paired live measurement remains pending. Flux cache occupancy needs an actual exported measure or separately instrumented profile; RSS is not cache size. |
| Queue latency | Preserve Flux workqueue histogram series. Apsides exports pending coalesced triggers and trigger-to-cycle scheduling count, sum and maximum. Report their different semantics explicitly; count/sum/maximum cannot establish percentiles. The collector records these runtime counters with identity and reset checks; paired live collection remains pending. |
| Relist-storm recovery | Requires synchronized real watch disruption/recovery for every domain, verified relists, convergence time, request spikes and memory peaks. Process startup alone is not proof of a relist storm. Not yet automated here. |
| Image size | `image_sizes.py` verifies OCI descriptors and reports compressed layer bytes per image and deduplicated across images. Docker-reported size is labeled by storage semantics; descriptor-based stores report packed resources, not unpacked filesystem size. Unpacked aggregate remains unknown. Apsides totals must include Envoy/admin; Flux totals must include Reloader and the artifact server. Full-profile collection remains pending. |
| Build/deployment latency | Time source build, native build, image packaging/import, platform staging/activation, and initial convergence separately. Flux artifact assembly is not a native compilation. Distinguish warm and cold caches; pre-pulled images are not network distribution measurements. Not yet integrated. |

`summary.json` deliberately reports `completeSection26: false` while these
remaining measurements and the real paired run are incomplete. The whole-spec
performance gate remains open. Before publishing comparisons, obtain independent
review of scope equivalence, missing series, counter resets, domain aggregation,
and actual evidence. Never infer that Apsides is faster from specialization alone.
