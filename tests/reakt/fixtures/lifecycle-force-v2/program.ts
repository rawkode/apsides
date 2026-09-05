import {ConfigMap, Service, defineDomain} from "@reakt/kubernetes/legacy";

// Synthetic source used only by the disposable deployment integration suite.
export default function program(buildInputs: {readonly sourceDigest: string}) {
  const namespace = "reakt-test-workload";
  const source = {file: "program.ts", line: 1, column: 1};
  const domain = defineDomain({
    id: "lifecycle", force: true, namespace, controlNamespace: "reakt-test-control",
    clusterBinding: "reakt-disposable-test", policyRef: "platform/lifecycle", enforcement: "api-bound",
    budgets: {maxResources: 16, maxInputs: 16, maxReactions: 16, maxObjectBytes: 262144, maxQueueDepth: 128, maxConcurrentRequests: 4},
    provenance: {inputs: [{path: "program.ts", sha256: buildInputs.sourceDigest}]},
  });
  domain.manage(new ConfigMap({id: "retained", metadata: {name: "retained", namespace}, data: {value: "version-2"}}), {deletionPolicy: "delete", source});
  domain.manage(new ConfigMap({id: "added", metadata: {name: "added", namespace}, data: {value: "new-permission"}}), {force: false, deletionPolicy: "delete", source});
  return {productionIR: domain.toIR()};
}
