import {ConfigMap, Service, defineDomain} from "@reakt/kubernetes/legacy";

// Synthetic source used only by the disposable deployment integration suite.
export default function program(buildInputs: {readonly sourceDigest: string}) {
  const namespace = "reakt-test-workload";
  const source = {file: "program.ts", line: 1, column: 1};
  const domain = defineDomain({
    id: "lifecycle", namespace, controlNamespace: "reakt-test-control",
    clusterBinding: "reakt-disposable-test", policyRef: "platform/lifecycle", enforcement: "api-bound",
    budgets: {maxResources: 16, maxInputs: 16, maxReactions: 16, maxObjectBytes: 262144, maxQueueDepth: 128, maxConcurrentRequests: 4},
    provenance: {inputs: [{path: "program.ts", sha256: buildInputs.sourceDigest}]},
  });
  domain.manage(new ConfigMap({id: "retained", metadata: {name: "retained", namespace}, data: {value: "version-1"}}), {deletionPolicy: "delete", source});
  domain.manage(new ConfigMap({id: "orphan", metadata: {name: "orphan", namespace}, data: {value: "retained"}}), {deletionPolicy: "orphan", source});
  domain.manage(new Service({id: "obsolete-service", metadata: {name: "obsolete-service", namespace}, spec: {selector: {app: "fixture"}, ports: [{port: 80, targetPort: 8080}]}}), {deletionPolicy: "delete", source});
  return {productionIR: domain.toIR()};
}
