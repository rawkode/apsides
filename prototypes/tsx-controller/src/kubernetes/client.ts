import { readFileSync } from "node:fs";
import { request } from "node:https";
import { request as requestHTTP } from "node:http";
import type { ResourceKind } from "../contracts";

export interface Connection { server: string; token?: string; tokenFile?: string; ca?: string; insecureSkipTLSVerify?: boolean }
export class KubernetesError extends Error {
  constructor(readonly status: number, message: string) { super(message); }
}
export function inClusterConnection(): Connection {
  const host = process.env.KUBERNETES_SERVICE_HOST;
  if (!host) throw new Error("KUBERNETES_SERVICE_HOST is missing; supply a Kubernetes connection");
  const directory = "/var/run/secrets/kubernetes.io/serviceaccount";
  return { server: `https://${host.includes(":") ? `[${host}]` : host}:${process.env.KUBERNETES_SERVICE_PORT_HTTPS ?? "443"}`,
    tokenFile: `${directory}/token`, ca: readFileSync(`${directory}/ca.crt`, "utf8") };
}
export function resourcePath(target: { kind: ResourceKind; namespace?: string; name: string }, namespace: string): string {
  const prefix = target.kind === "kubernetes:Deployment" ? "/apis/apps/v1" : "/api/v1";
  const plural = ({ "kubernetes:Deployment": "deployments", "kubernetes:Service": "services", "kubernetes:ConfigMap": "configmaps" } as Record<string, string>)[target.kind];
  if (!plural) throw new Error(`Unsupported Kubernetes resource: ${target.kind}`);
  return `${prefix}/namespaces/${encodeURIComponent(target.namespace ?? namespace)}/${plural}/${encodeURIComponent(target.name)}`;
}
export class KubernetesClient {
  constructor(readonly connection: Connection) {}
  async call(method: string, path: string, body?: unknown, contentType = "application/json", signal?: AbortSignal): Promise<any> {
    signal?.throwIfAborted();
    const url = new URL(path, this.connection.server);
    const payload = body === undefined ? undefined : JSON.stringify(body);
    const token = this.connection.tokenFile ? readFileSync(this.connection.tokenFile, "utf8").trim() : this.connection.token;
    return new Promise((resolve, reject) => {
      const req = (url.protocol === "http:" ? requestHTTP : request)(url, {
        method, ca: this.connection.ca, rejectUnauthorized: !this.connection.insecureSkipTLSVerify,
        signal, headers: { Accept: "application/json", ...(token ? { Authorization: `Bearer ${token}` } : {}),
          ...(payload ? { "Content-Type": contentType, "Content-Length": Buffer.byteLength(payload) } : {}) },
      }, (response) => {
        let text = "";
        response.setEncoding("utf8"); response.on("data", (chunk) => { text += chunk; });
        response.on("end", () => {
          let value: any; try { value = text ? JSON.parse(text) : undefined; } catch { reject(new Error("Invalid Kubernetes JSON response")); return; }
          if ((response.statusCode ?? 500) >= 400) reject(new KubernetesError(response.statusCode!, value?.message ?? `Kubernetes HTTP ${response.statusCode}`));
          else resolve(value);
        });
      });
      req.on("error", reject); req.setTimeout(30_000, () => req.destroy(new Error("Kubernetes request timeout")));
      req.end(payload);
    });
  }
  async get(path: string, signal?: AbortSignal): Promise<any | undefined> {
    try { return await this.call("GET", path, undefined, undefined, signal); }
    catch (error) { if (error instanceof KubernetesError && error.status === 404) return undefined; throw error; }
  }
}
