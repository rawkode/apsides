import { ConfigMap, Controller } from "@apsides/kubernetes";
import { Cluster } from "./.aps/crds/cnpg.ts";

const namespace = "default";

const Database = () => {

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
        metadata={{ name: "api-config", namespace }}
        data={{ PGRST_DB_SCHEMAS: "public", PGRST_DB_ANON_ROLE: "app", PGRST_SERVER_PORT: "3000" }}
        deletionPolicy="orphan"
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
    <Database />
  </Controller>
);
