import { ConfigMap, Controller, Service } from "@reakt/kubernetes";

// Synthetic source used only by the disposable deployment integration suite.
export default (
  <Controller
    id="lifecycle"
    namespace="reakt-test-workload"
    controlNamespace="reakt-test-control"
    clusterBinding="reakt-disposable-test"
    policyRef="platform/lifecycle"
    enforcement="api-bound"
    budgets={{
      maxResources: 16,
      maxInputs: 16,
      maxReactions: 16,
      maxObjectBytes: 262_144,
      maxQueueDepth: 128,
      maxConcurrentRequests: 4,
    }}
  >
    <ConfigMap id="retained" metadata={{ name: "retained", namespace: "reakt-test-workload" }} data={{ value: "version-1" }} deletionPolicy="delete" />
    <ConfigMap id="orphan" metadata={{ name: "orphan", namespace: "reakt-test-workload" }} data={{ value: "retained" }} deletionPolicy="orphan" />
    <Service
      id="obsolete-service"
      metadata={{ name: "obsolete-service", namespace: "reakt-test-workload" }}
      spec={{ selector: { app: "fixture" }, ports: [{ port: 80, targetPort: 8080 }] }}
      deletionPolicy="delete"
    />
  </Controller>
);
