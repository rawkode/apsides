import {
  ConfigMap,
  Container,
  Data,
  Deployment,
  Env,
  Event,
  Port,
  Resources,
  Service,
  defineAction,
  defineController,
  useAutomation,
  useEnrichment,
  useResource,
} from "@oxideloop/kubernetes";

interface WebApplicationSpec {
  image: string;
  replicas?: number;
  expose?: boolean;
}

const configurationChanged = defineAction<{ message: string }>({
  id: "configuration-changed",
  version: "v1",
  run: ({ source, metadata }) => (
    <Event
      id="notification"
      regarding={source}
      reason="ConfigurationChanged"
      action="ConfigurationChanged"
      note={metadata.message}
    />
  ),
});

export default defineController<WebApplicationSpec>(({ root }) => {
  const name = root.metadata.name;
  const configName = `${name}-config`;
  const labels = { app: name };
  const settings = useResource(ConfigMap, { name: "demo-input" });
  const message = settings.current?.data?.message ?? "Hello from OxideLoop";
  const Defaults = useEnrichment({
    labels,
    podSecurityContext: { runAsNonRoot: true },
  });

  useAutomation({
    when: settings.changed(),
    action: configurationChanged,
    metadata: { message },
  });

  return (
    <Defaults>
      <ConfigMap id="config" name={configName}>
        <Data name="message" value={message} />
      </ConfigMap>
      <Deployment
        id="deployment"
        name={name}
        replicas={root.spec.replicas ?? 1}
        selector={labels}
      >
        <Container name="web" image={root.spec.image}>
          <Port name="http" containerPort={8080} />
          <Env
            name="MESSAGE"
            configMap={{ name: configName, key: "message" }}
          />
          <Resources
            requests={{ cpu: "10m", memory: "32Mi" }}
            limits={{ memory: "64Mi" }}
          />
        </Container>
      </Deployment>
      {root.spec.expose ? (
        <Service id="service" name={`${name}-service`} selector={labels}>
          <Port name="http" port={80} targetPort="http" />
        </Service>
      ) : null}
    </Defaults>
  );
});
