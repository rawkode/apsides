import { ConfigMap, Controller, Service } from "@apsides/kubernetes";

// Synthetic source used only by the disposable deployment integration suite.
export default (
  <Controller
    id="lifecycle"
    namespace="apsides-test-workload"
    controlNamespace="apsides-test-control"
    clusterBinding="apsides-disposable-test"
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
    <ConfigMap id="retained" metadata={{ name: "retained", namespace: "apsides-test-workload" }} data={{ value: "version-1" }} deletionPolicy="delete" />
    <ConfigMap id="orphan" metadata={{ name: "orphan", namespace: "apsides-test-workload" }} data={{ value: "retained" }} deletionPolicy="orphan" />
    <Service
      id="obsolete-service"
      metadata={{ name: "obsolete-service", namespace: "apsides-test-workload" }}
      spec={{ selector: { app: "fixture" }, ports: [{ port: 80, targetPort: 8080 }] }}
      deletionPolicy="delete"
    />
  </Controller>
);
