import { ConfigMap, Controller } from "@reakt/kubernetes";

export default (
  <Controller
    id="team-api-production"
    namespace="production"
    controlNamespace="reakt-system"
    clusterBinding="local-fixture"
    policyRef="platform/team-api"
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
    <ConfigMap
      metadata={{ name: "api-config", namespace: "production" }}
      data={{ LOG_LEVEL: "before-upgrade" }}
      deletionPolicy="orphan"
    />
  </Controller>
);
