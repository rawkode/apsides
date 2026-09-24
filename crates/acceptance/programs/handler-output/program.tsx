import { Controller, On, useConfigMap, useHandlerOutput } from "@apsides/kubernetes";

/// The whole scenario: an observed `ConfigMap` input (`handler-source`,
/// created and updated directly by the acceptance test, not managed by
/// this domain), one handler with no egress that copies every key of the
/// input's `data` into its declared output (`handler-derived`) verbatim.
const HandlerApp = () => {
  const source = useConfigMap({ id: "handler-source", name: "handler-source" });
  const derived = useHandlerOutput({ id: "handler-derived", name: "handler-derived" });
  return (
    <On
      trigger={source.changed}
      handler="./handlers/derive-keys.ts#deriveKeys"
      inputs={[source]}
      egress={[]}
      outputs={[derived]}
    />
  );
};

export default (
  <Controller
    id="handler-output"
    namespace="default"
    controlNamespace="apsides-system"
    clusterBinding="local-demo"
    policyRef="platform/handler-output"
    enforcement="api-bound"
    budgets={{
      maxResources: 1,
      maxInputs: 2,
      maxReactions: 1,
      maxObjectBytes: 262_144,
      maxQueueDepth: 8,
      maxConcurrentRequests: 1,
    }}
  >
    <HandlerApp />
  </Controller>
);
