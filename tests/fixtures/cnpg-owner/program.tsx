import { ConfigMap, Controller } from "@apsides/kubernetes";

export default (
  <Controller
    id="database-owner"
    namespace="default"
    controlNamespace="apsides-system"
    clusterBinding="local-demo"
    policyRef="platform/database-owner"
    enforcement="api-bound"
    budgets={{
      maxResources: 1,
      maxInputs: 1,
      maxReactions: 1,
      maxObjectBytes: 262_144,
      maxQueueDepth: 8,
      maxConcurrentRequests: 1,
    }}
  >
    <ConfigMap
      metadata={{ name: "api-config", namespace: "default" }}
      data={{ owner: "database-owner" }}
      deletionPolicy="orphan"
    />
  </Controller>
);
