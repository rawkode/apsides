import {
  ConfigMap,
  Controller,
  Deployment,
  useConfigMap,
  type DomainBudgets,
} from "@apsides/kubernetes";

const budgets: DomainBudgets = {
  maxResources: 8,
  maxInputs: 8,
  maxReactions: 8,
  maxObjectBytes: 262_144,
  maxQueueDepth: 32,
  maxConcurrentRequests: 2,
};

const App = () => {
  const settings = useConfigMap({ name: "app-settings" });
  const labels = { app: "hello" };

  return (
    <>
      <ConfigMap
        metadata={{ name: "hello-config", namespace: "default" }}
        data={{ LOG_LEVEL: "info" }}
      />
      <Deployment
        metadata={{ name: "hello", namespace: "default" }}
        rolloutOn={[settings.data]}
        spec={{
          replicas: 1,
          selector: { matchLabels: labels },
          template: {
            metadata: { labels },
            spec: {
              automountServiceAccountToken: false,
              containers: [{
                name: "hello",
                image: "docker.io/nginxinc/nginx-unprivileged@sha256:9b87ad3dd9f431c733f19dfb278c7eb3dba9dca381942c79818bb42f1a566a83",
                ports: [{ containerPort: 8080 }],
                resources: {
                  requests: { cpu: "10m", memory: "16Mi" },
                  limits: { memory: "64Mi" },
                },
                envFrom: [{ configMapRef: { name: "app-settings" } }],
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
    id="hello"
    namespace="default"
    controlNamespace="apsides-system"
    clusterBinding="local"
    policyRef="platform/hello"
    enforcement="api-bound"
    budgets={budgets}
  >
    <App />
  </Controller>
);
