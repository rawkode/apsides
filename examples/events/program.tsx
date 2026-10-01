import { Controller, Deployment, On, useConfigMap, useHandlerOutput } from "@apsides/kubernetes";

/// An observed `feature-settings` ConfigMap (created by an operator, never
/// managed by this domain) names the environment whose feature flags the
/// storefront should run with. Each change to it runs `syncFlags`, which
/// may reach only `flags.example.com:443` and write only the
/// `feature-flags` output. The storefront Deployment reads that output as
/// environment variables and rolls out whenever the handler changes it.
const Storefront = () => {
  const settings = useConfigMap({ id: "feature-settings", name: "feature-settings" });
  const flags = useHandlerOutput({ id: "feature-flags", name: "feature-flags" });
  const labels = { app: "storefront" };
  return (
    <>
      <On
        id="sync-flags"
        trigger={settings.changed}
        handler="./handlers/sync-flags.ts#syncFlags"
        inputs={[settings]}
        egress={["flags.example.com"]}
        outputs={[flags]}
      />
      <Deployment
        metadata={{ name: "storefront", namespace: "default" }}
        deletionPolicy="orphan"
        rolloutOn={[flags.data]}
        spec={{
          replicas: 2,
          selector: { matchLabels: labels },
          template: {
            metadata: { labels },
            spec: {
              automountServiceAccountToken: false,
              containers: [{
                name: "storefront",
                image: "docker.io/nginxinc/nginx-unprivileged@sha256:9b87ad3dd9f431c733f19dfb278c7eb3dba9dca381942c79818bb42f1a566a83",
                ports: [{ containerPort: 8080 }],
                resources: {
                  requests: { cpu: "10m", memory: "16Mi" },
                  limits: { memory: "64Mi" },
                },
                envFrom: [{ configMapRef: { name: "feature-flags" } }],
              }],
            },
          },
        }}
      />
    </>
  );
};

export default (
  <Controller
    id="storefront-flags"
    namespace="default"
    controlNamespace="apsides-system"
    clusterBinding="local-demo"
    policyRef="platform/storefront-flags"
    enforcement="api-bound"
    budgets={{
      maxResources: 4,
      maxInputs: 4,
      maxReactions: 2,
      maxObjectBytes: 262_144,
      maxQueueDepth: 8,
      maxConcurrentRequests: 2,
    }}
  >
    <Storefront />
  </Controller>
);
