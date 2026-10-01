import { Controller, On, useConfigMap, useHandlerOutput } from "@apsides/kubernetes";

/// The whole scenario: an observed `ConfigMap` input (`handler-source`,
/// created and updated directly by the acceptance test, not managed by
/// this domain), one handler with no egress that copies every key of the
/// input's `data` into its declared output (`handler-derived`) verbatim,
/// and a second handler chained on that output that summarizes it into
/// `handler-summary`.
const HandlerApp = () => {
  const source = useConfigMap({ id: "handler-source", name: "handler-source" });
  const derived = useHandlerOutput({ id: "handler-derived", name: "handler-derived" });
  const summary = useHandlerOutput({ id: "handler-summary", name: "handler-summary" });
  return (
    <>
      <On
        trigger={source.changed}
        handler="./handlers/derive-keys.ts#deriveKeys"
        inputs={[source]}
        egress={[]}
        outputs={[derived]}
      />
      <On
        trigger={derived.changed}
        handler="./handlers/summarize-entries.ts#summarizeEntries"
        inputs={[derived]}
        egress={[]}
        outputs={[summary]}
      />
    </>
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
      maxResources: 2,
      maxInputs: 3,
      maxReactions: 1,
      maxObjectBytes: 262_144,
      maxQueueDepth: 8,
      maxConcurrentRequests: 1,
    }}
  >
    <HandlerApp />
  </Controller>
);
