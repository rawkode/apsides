import {
  compile, ConfigMap, Controller, Deployment, Service,
  useResource, useResourceRef, useFinalizer, type ResourceGraph,
} from "../src";
import type { Snapshots } from "../src/reactive";

function Website({ replicas, image }: { replicas: number; image: string }) {
  const config = useResource(ConfigMap, { name: "nginx-config" });
  const nginx = useResourceRef();
  useFinalizer(nginx, "website.example.com/cleanup", async ({ resource, signal }) => {
    signal.throwIfAborted();
    console.log(JSON.stringify({ event: "managed-finalizer-complete", resource: resource.id, uid: resource.uid }));
  });
  return <Deployment id="nginx" name="nginx" image={image} replicas={replicas} port={80} ref={nginx} envFrom={[config]}>
    <Service id="service" readyAfter={[nginx]} />
  </Deployment>;
}

export function website(options: { namespace?: string; replicas?: number; image?: string; snapshots?: Snapshots } = {}): ResourceGraph {
  const replicas = options.replicas ?? Number(process.env.NGINX_REPLICAS ?? 2);
  if (!Number.isInteger(replicas) || replicas < 1) throw new Error("NGINX_REPLICAS must be a positive integer");
  return compile(<Controller name="website" namespace={options.namespace ?? "tsx-e2e"}>
    <Website replicas={replicas} image={options.image ?? process.env.NGINX_IMAGE ?? "nginx:1.27-alpine"} />
  </Controller>, { snapshots: options.snapshots });
}
