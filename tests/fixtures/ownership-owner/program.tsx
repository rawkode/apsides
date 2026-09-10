import { ConfigMap, Controller } from "@apsides/kubernetes";

export default (
  <Controller
    id="ownership-owner-production"
    namespace="production"
    controlNamespace="apsides-system"
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
      data={{ OWNER: "ownership-owner-production" }}
      deletionPolicy="orphan"
    />
  </Controller>
);
