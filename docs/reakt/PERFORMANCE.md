# Performance comparison contract

`tests/reakt/performance` prepares and measures a scoped general-purpose Flux
baseline. It currently provides preparation, installation, raw collection, and
input-change stimulation. It has **not completed a live comparative benchmark**.
No speed, memory, scalability, or complete §26 claim follows from its unit tests.

The corrected native CNPG candidate passed a local run in
`work/cnpg-input-reactions-89ceec4`, using producer commit `89ceec4` and release
digest `sha256:ae8f43a0edacd6cb40962137ac2a7243c2ec23b4e79c274706c7683c7ebef5e3`.
It completed initial deployment and a permission-changing upgrade, then collected
13 idle and 13 active samples. Every runtime health sample was ready and
synchronized with zero errors; the runtime and proxy identities stayed fixed.
Changing the observed ConfigMap produced a new workload Pod and ReplicaSet,
and the HTTP result changed from two rows to one. After sampling, an actual
metadata update with unchanged ConfigMap data produced no rollout for 30 seconds.
Adding an unused synthetic Secret key caused another rollout; removing that key
restored the original Secret data and workload revision with working database
access. Secret values stayed in memory. This tests data-change reactions and
restoration, not database credential rotation.
The disposable cluster and credentials were removed.

These are single-domain raw measurements, not a comparative performance result.
The active window's workload Pod replacement leaves its whole-population CPU
interval incomplete; the collector preserves that limitation. Apply-cache
measurements represent four retained entries and 861 serialized bytes, not
total cache memory. Relist storms, increasing domain counts, full image and
build/deploy costs, and baseline cache occupancy remain open. New release
qualification must use the corrected candidate rather than old binary receipts.

## Offline metric analysis

Use the official [Prometheus Python parser](https://prometheus.github.io/client_python/parser/)
through the pinned dependency file to analyze an existing collection window:

```sh
cuenv task performanceMetricsTest
cuenv exec -- uv run --cache-dir work/uv-cache --no-project \
  --with-requirements tests/reakt/performance/requirements.txt \
  python3 tests/reakt/performance/analyze_metrics.py \
  --samples "$SAMPLE_WINDOW" --output "$NEW_REPORT_JSON"
```

The output path must be new. Reports bind every consumed sample and metrics file
by digest, plus the analyzer, dependency declaration and parser version. They
report concurrent WATCH gauges, API-server request rates, and separate controller
client request rates and per-queue duration means. Missing or changing counter
series, resets and invalid values produce unavailable metrics, not assumed zeros.
Queue means do not establish percentiles or share the native scheduling metric's
semantics. Reports do not authenticate producer identity or replace release evidence.

The four reports in `work/cnpg-metrics-analysis-68c2d8b` analyze the saved CNPG
baseline and corrected native idle/active windows. Baseline cluster-wide WATCH
counts were 197 throughout; native idle samples ranged from 182 to 184 and active
samples were 184 throughout. API-server request rates are unavailable in all
four windows because counter series changed. These observations include system
controllers and observers, and do not complete the scope-matched comparison.

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

The live prerequisite run at source revision `45101cb` passed in
`work/cnpg-baseline-live-45101cb/result.json`. It observed HTTP 200 with two rows
initially, a new Pod and ReplicaSet with one row after the ConfigMap change,
30 seconds without a rollout after an actual equal-data update, and another
rollout with one row after the temporary Secret key change. The original Secret
data was restored. Cleanup restored the prior cluster/container inventory and
removed the run's kubeconfigs. Earlier runs exposed and fixed tagged-digest
image import normalization and transient PostgREST startup HTTP 503 handling.
This establishes workload prerequisites only; paired performance, relist storms,
cache measurements and increasing-domain results remain pending.

Add `--collect-performance` to collect an idle window and a ConfigMap-change
window with the existing collector. `--performance-samples` (default 13) and
`--performance-interval` (default 5 seconds) configure both windows. Collection
includes `default` and `cnpg-system`, plus the exact Flux and Reloader Pod metrics
endpoints. A completed sample must precede the change and a sample must follow
its completion. Collector failures or controller identity changes reject the
window. These baseline samples still require a matching native run and analysis;
they do not close the benchmark gate. Retain replaced application-container
accounting as unavailable rather than treating the endpoint CPU delta as a full
active-window total.
Each window also performs four controller-identity checks of four Kubernetes GETs
each. API request rates include these checks and collection traffic; the native
comparison must account for equivalent observer overhead.

The first sampled run at `1c9563c` passed in
`work/cnpg-baseline-samples-1c9563c`: thirteen samples per window, about 60 seconds
per window, and no collector errors. Idle CPU population accounting was complete;
active accounting remained incomplete because the application Pod was replaced.
The run also passed the reaction checks and restored the original cluster and
container inventory with credentials removed. These are baseline-only samples;
no native performance comparison or complete §26 result is established.

For the native comparison, sampling belongs after the signed successor has
committed and published its inventory in `run_cnpg_demo.py`. It must cover
`default`, `cnpg-system`, and the control namespace, including the native
controller and every Envoy/admin container. The runtime exposes `/status` rather
than `/metrics`; each recorded status must match the protected generation and
artifact digest. The status collector makes three Kubernetes GETs per sample,
so its observer traffic differs from the baseline's metrics-only probes. Report
that overhead explicitly when analysing API request rates. Fixture-mode reactor
runs cannot substitute for these production-runtime measurements.

Use absolute verified input paths and a fresh ignored output directory:

```sh
cuenv exec -- python3 tests/reakt/run_cnpg_demo.py \
  --initial-artifact "$INITIAL_RELEASE" --successor-artifact "$CNPG_RELEASE" \
  --trust-policy "$TRUST_POLICY" --operator-manifest "$CNPG_OPERATOR_MANIFEST" \
  --operator-sha256 "$CNPG_OPERATOR_SHA256" \
  --output "$PWD/work/cnpg-native-samples" --collect-performance
```

The native runner uses the same sample-count and interval options as the baseline.
Its four boundary identity checks each make six GETs to bind the active allocation,
activation record, runtime, and proxy. The existing permission-upgrade evidence
remains scoped to that lifecycle check; collection does not add a benchmark receipt.

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
