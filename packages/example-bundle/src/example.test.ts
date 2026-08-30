import { render } from "./render.js";
import { action } from "./action.js";
import type { RenderInputV0 } from "@oxideloop/sdk";
import { readFileSync } from "node:fs";

const BUNDLE_DIGEST = "a".repeat(64);
const ACTION_DIGEST = "b".repeat(64);
(globalThis as { __OXIDELOOP_ACTION_MODULE_DIGEST__?: string }).__OXIDELOOP_ACTION_MODULE_DIGEST__ = ACTION_DIGEST;
const input = (spec: unknown, observations: RenderInputV0["observations"] = []): RenderInputV0 => ({ abiVersion: "renderer.oxideloop.dev/v0", root: { apiVersion: "demo.oxideloop.dev/v1alpha1", kind: "WebApplication", namespace: "demo", name: "portal", uid: "root-1", generation: 1, spec: spec as never }, bundleDigest: BUNDLE_DIGEST, cacheEpoch: "0", observationToken: "0", observations });
const absent = render(input({ message: "hello", expose: false }));
if (absent.resources.length !== 1 || absent.resources[0]?.logicalId !== "configmap/portal-config") throw new Error("absent input/expose false graph mismatch");
if (absent.dependencies.length !== 1 || absent.automations.length !== 0) throw new Error("absent observations must not create an automation");
const present = render(input({ message: "updated", expose: true }, [{ apiVersion: "v1", kind: "ConfigMap", namespace: "demo", name: "demo-input", uid: "input-1", resourceVersion: "2", value: { data: { flavor: "vanilla" } } }]));
if (present.resources.length !== 2 || (present.resources[0]?.object as never as { data: Record<string, string> }).data.flavor !== "vanilla") throw new Error("present input enrichment mismatch");
if (present.resources[1]?.logicalId !== "service/portal-service") throw new Error("expose true service mismatch");
if (present.automations.length !== 1 || present.automations[0]?.bundleDigest !== BUNDLE_DIGEST || present.automations[0]?.actionModuleDigest !== ACTION_DIGEST) throw new Error("automation pinning mismatch");
const expectedAction = JSON.parse(readFileSync(new URL("../fixtures/input-changed-action-output.json", import.meta.url), "utf8"));
const actualAction = action({ occurrenceId: "occurrence-1", eventMetadata: { source: { namespace: "demo", name: "input" }, note: "updated" }, canonical: {} as never });
if (JSON.stringify(actualAction) !== JSON.stringify(expectedAction)) throw new Error("checked-in action fixture mismatch");
