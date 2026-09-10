import {
  ConfigMap,
  Deployment,
  Service,
  useConfigMap,
  useResourceRef,
  useSecret,
} from "@apsides/kubernetes";

interface ApiProps {
  readonly name: string;
  readonly namespace: string;
  readonly image: string;
}

export const Api = ({ name, namespace, image }: ApiProps) => {
  const config = useResourceRef("ConfigMap");
  const settings = useConfigMap({ name: "external-settings" });
  const credentials = useSecret({ name: "database-credentials" });
  const labels = { app: name };

  return (
    <>
      <ConfigMap
        ref={config}
        metadata={{ name: `${name}-config`, namespace }}
        data={{ LOG_LEVEL: "info" }}
        deletionPolicy="orphan"
      />
      <Deployment
        metadata={{ name, namespace }}
        deletionPolicy="orphan"
        rolloutOn={[config.data, settings.data, credentials.data]}
        spec={{
          replicas: 3,
          selector: { matchLabels: labels },
          template: {
            metadata: { labels },
            spec: {
              automountServiceAccountToken: false,
              containers: [{
                name,
                image,
                ports: [{ containerPort: 8080 }],
                resources: {
                  requests: { cpu: "10m", memory: "16Mi" },
                  limits: { memory: "64Mi" },
                },
                envFrom: [
                  { configMapRef: { name: `${name}-config` } },
                  { configMapRef: { name: "external-settings" } },
                  { secretRef: { name: "database-credentials" } },
                ],
              }],
            },
          },
        }}
      />
      <Service
        metadata={{ name, namespace }}
        deletionPolicy="orphan"
        spec={{ selector: labels, ports: [{ port: 80, targetPort: 8080 }] }}
      />
    </>
  );
};
