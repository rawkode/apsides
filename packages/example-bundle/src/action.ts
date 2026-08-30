import { defineAction } from "@oxideloop/sdk";
import type { ActionOutputV0 } from "@oxideloop/sdk";

export const ACTION_MODULE_IDENTITY = "demo.web-application.input-changed";

export interface InputChangedActionInput {
  readonly source: { readonly namespace: string; readonly name: string };
  readonly note: string;
}

export const action = defineAction<InputChangedActionInput>(({ eventMetadata: input, occurrenceId }): ActionOutputV0 => ({
  abiVersion: "renderer.oxideloop.dev/v0",
  actions: [{
    logicalId: `input-changed/${occurrenceId}`,
    kind: "Event",
    specification: {
      apiVersion: "events.k8s.io/v1",
      kind: "Event",
      metadata: { namespace: input.source.namespace, generateName: "demo-input-changed-" },
      regarding: { apiVersion: "v1", kind: "ConfigMap", name: input.source.name, namespace: input.source.namespace },
      note: input.note,
      type: "Normal",
      reason: "InputChanged",
    },
  }],
  diagnostics: [],
}));
