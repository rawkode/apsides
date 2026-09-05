import { Controller } from "@reakt/kubernetes";
import { Api } from "./api.tsx";

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
    <Api
      name="api"
      namespace="production"
      image="registry.example.com/api@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    />
  </Controller>
);
