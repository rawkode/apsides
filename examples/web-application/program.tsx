import { Controller } from "@apsides/kubernetes";
import { Api } from "./api.tsx";

export default (
  <Controller
    id="team-api-production"
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
    <Api
      name="api"
      namespace="production"
      image="docker.io/nginxinc/nginx-unprivileged@sha256:9b87ad3dd9f431c733f19dfb278c7eb3dba9dca381942c79818bb42f1a566a83"
    />
  </Controller>
);
