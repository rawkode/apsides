import { ConfigMap, Controller } from "@apsides/kubernetes";

// Synthetic source used only by the disposable deployment integration suite.
export default (
  <Controller
    id="lifecycle"
    namespace="apsides-test-workload"
    controlNamespace="apsides-test-control"
    clusterBinding="apsides-disposable-test"
    policyRef="platform/lifecycle"
    enforcement="api-bound"
    force
    budgets={{
      maxResources: 16,
      maxInputs: 16,
      maxReactions: 16,
      maxObjectBytes: 262_144,
      maxQueueDepth: 128,
      maxConcurrentRequests: 4,
    }}
  >
    <ConfigMap id="retained" metadata={{ name: "retained", namespace: "apsides-test-workload" }} data={{ value: "version-2" }} deletionPolicy="delete" />
    <ConfigMap id="added" metadata={{ name: "added", namespace: "apsides-test-workload" }} data={{ value: "new-permission" }} deletionPolicy="delete" force={false} />
  </Controller>
);
