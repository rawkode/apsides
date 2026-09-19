import type { Finalizer, ObservedResource, Output, ResourceKind, ResourceRef } from "../contracts";
import type { Child, Component } from "./jsx-runtime";

export const resourceKind = Symbol("resourceKind");
export type ResourceComponent<P = any> = Component<P> & { [resourceKind]: ResourceKind };

export interface CommonProps {
  name: string;
  id?: string;
  namespace?: string;
  ref?: ResourceRef<any>;
  readyAfter?: readonly ResourceRef<any>[];
  finalizers?: Record<string, Finalizer>;
  deletionPolicy?: "delete" | "retain";
}

export interface ControllerProps {
  name: string;
  namespace?: string;
  children?: Child;
}

export interface DeploymentProps extends CommonProps {
  image: string | Output<string>;
  replicas?: number | Output<number>;
  port?: number;
  ports?: readonly { name?: string; containerPort: number }[];
  selector?: Record<string, string>;
  env?: Record<string, string | Output<string>>;
  envFrom?: readonly ObservedResource[];
  podAnnotations?: Record<string, string>;
  children?: Child;
}

export interface ServiceProps extends Omit<CommonProps, "name"> {
  name?: string;
  type?: "ClusterIP" | "LoadBalancer" | "NodePort";
  selector?: Record<string, string>;
  port?: number;
  targetPort?: number | string;
  ports?: readonly { name?: string; port: number; targetPort?: number | string }[];
}

export interface ConfigMapProps extends CommonProps { data?: Record<string, string | Output<string>> }
export interface DatabaseProps extends CommonProps {
  organization: string;
  region?: string;
}
export interface DNSRecordProps extends CommonProps {
  zoneId: string;
  type: "A" | "AAAA" | "CNAME";
  content: string | Output<string>;
  ttl?: number;
  proxied?: boolean;
}

function resource<P>(kind: ResourceKind): ResourceComponent<P> {
  return Object.assign(() => null, { [resourceKind]: kind });
}

export const Controller: Component<ControllerProps> = () => null;
export const Deployment = resource<DeploymentProps>("kubernetes:Deployment");
export const Service = resource<ServiceProps>("kubernetes:Service");
export const ConfigMap = resource<ConfigMapProps>("kubernetes:ConfigMap");
export const PlanetScale = { Database: resource<DatabaseProps>("planetscale:Database") };
export const Cloudflare = { DNSRecord: resource<DNSRecordProps>("cloudflare:DNSRecord") };
