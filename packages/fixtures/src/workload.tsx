import { useResource } from "@oxideloop/sdk/hooks";
import React from "react";
export function Workload({ name = "web", namespace = "default" }: { name?: string; namespace?: string }) {
  const service = useResource({ apiVersion: "v1", kind: "Service", namespace, name: `${name}-service` });
  useResource({ apiVersion: "v1", kind: "Service", namespace, name: `${name}-service` });
  return <Deployment apiVersion="apps/v1" kind="Deployment" metadata={{ name }} spec={{ replicas: service.present ? 2 : 1 }} />;
}
export function ReactWorkload({ name = "web", namespace = "default" }: { name?: string; namespace?: string }) {
  const service = useResource({ apiVersion: "v1", kind: "Service", namespace, name: `${name}-service` });
  useResource({ apiVersion: "v1", kind: "Service", namespace, name: `${name}-service` });
  return React.createElement("Deployment", { apiVersion: "apps/v1", kind: "Deployment", metadata: { name }, spec: { replicas: service.present ? 2 : 1 } });
}
function Deployment(node: { apiVersion: string; kind: string; metadata: { name: string }; spec: Record<string, unknown> }): unknown { return node; }
