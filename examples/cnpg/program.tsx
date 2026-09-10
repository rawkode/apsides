import { ConfigMap, Controller, Deployment, Service, useConfigMap, useResourceRef, useSecret } from "@apsides/kubernetes";
import { Cluster } from "./.aps/crds/cnpg.ts";

const namespace = "default";

const DatabaseApi = () => {
  const config = useResourceRef("ConfigMap");
  const settings = useConfigMap({ name: "api-settings" });
  const database = useSecret({ name: "database-app" });
  const labels = { app: "database-api" };

  return (
    <>
      <Cluster
        metadata={{ name: "database", namespace }}
        deletionPolicy="orphan"
        spec={{
          instances: 1,
          imageName: "ghcr.io/cloudnative-pg/postgresql:17@sha256:70664ebcfa1100361b5bdc28bbf06fdbe08db2dc4ad7bd14de33c5e05fe8ea8e",
          storage: { size: "1Gi" },
          bootstrap: {
            initdb: {
              database: "app",
              owner: "app",
              postInitApplicationSQL: [
                "CREATE TABLE messages (id integer PRIMARY KEY, message text NOT NULL)",
                "INSERT INTO messages VALUES (1, 'Hello from CloudNativePG'), (2, 'Configured by Apsides')",
                "GRANT SELECT ON messages TO app",
              ],
            },
          },
        }}
      />
      <ConfigMap
        ref={config}
        metadata={{ name: "api-config", namespace }}
        data={{ PGRST_DB_SCHEMAS: "public", PGRST_DB_ANON_ROLE: "app", PGRST_SERVER_PORT: "3000" }}
        deletionPolicy="orphan"
      />
      <Deployment
        metadata={{ name: "database-api", namespace }}
        deletionPolicy="orphan"
        rolloutOn={[config.data, settings.data, database.data]}
        spec={{
          replicas: 1,
          selector: { matchLabels: labels },
          template: {
            metadata: { labels },
            spec: {
              automountServiceAccountToken: false,
              containers: [{
                name: "api",
                image: "docker.io/postgrest/postgrest@sha256:85258123312dc496ad4c2ed832154a65e9746f84df0d6d09b44229ff9230c08e",
                ports: [{ containerPort: 3000 }],
                envFrom: [{ configMapRef: { name: "api-config" } }, { configMapRef: { name: "api-settings" } }],
                env: [{ name: "PGRST_DB_URI", valueFrom: { secretKeyRef: { name: "database-app", key: "uri" } } }],
                resources: { requests: { cpu: "10m", memory: "32Mi" }, limits: { memory: "128Mi" } },
              }],
            },
          },
        }}
      />
      <Service
        metadata={{ name: "database-api", namespace }}
        deletionPolicy="orphan"
        spec={{ selector: labels, ports: [{ port: 80, targetPort: 3000 }] }}
      />
    </>
  );
};

export default (
  <Controller
    id="database-demo"
    namespace={namespace}
    controlNamespace="apsides-system"
    clusterBinding="local-demo"
    policyRef="platform/database-demo"
    enforcement="api-bound"
    budgets={{ maxResources: 16, maxInputs: 16, maxReactions: 16, maxObjectBytes: 262_144, maxQueueDepth: 128, maxConcurrentRequests: 4 }}
  >
    <DatabaseApi />
  </Controller>
);
