/**
 * Types for event handler modules: the event a handler receives and the
 * `apsides` global the controller's embedded engine provides.
 *
 * A handler module imports these as types only, which the compiler erases
 * before bundling:
 *
 * ```ts
 * import type { HandlerEvent } from "@apsides/kubernetes/handler";
 *
 * export async function syncFlags(event: HandlerEvent<"feature-settings">) {
 *   const environment = event.inputs["feature-settings"].data.environment;
 *   apsides.writeOutput("feature-flags", "FLAGS_ENVIRONMENT", environment ?? "production");
 * }
 * ```
 *
 * `aps compile` type-checks every handler module against these
 * declarations and the ECMAScript library only: the engine has no Deno,
 * Node, DOM or Web API globals besides `queueMicrotask`.
 */

/** The data of one declared input, as the handler sees it. */
export interface HandlerInput {
  readonly data: Readonly<Record<string, string>>;
}

/** The event a handler's export is called with. */
export interface HandlerEvent<InputId extends string = string> {
  /** Digest of the handler id and input revision; stable across redelivery. */
  readonly id: string;
  readonly trigger: { readonly kind: "input-changed"; readonly input: InputId };
  /** The current data of every declared input, keyed by input id. */
  readonly inputs: { readonly [Id in InputId]: HandlerInput };
}

export interface FetchOptions {
  /** Defaults to `GET`. */
  readonly method?: string;
  /** `Host`, `Content-Length`, `Transfer-Encoding`, `Connection`, `TE`, `Upgrade` and `Trailer` are refused. */
  readonly headers?: Readonly<Record<string, string>>;
  readonly body?: string | Uint8Array;
}

export interface FetchResponse {
  readonly status: number;
  /** Response headers as `[name, value]` pairs, in the order received. */
  readonly headers: readonly (readonly [string, string])[];
  readonly body: Uint8Array;
}

export type LogLevel = "debug" | "info" | "warn" | "error";

/** An error thrown by an `apsides` call carries a machine-readable `code`. */
export interface HandlerError extends Error {
  readonly code: string;
}

/** The frozen `apsides` global. */
export interface HandlerApi {
  /** HTTPS only, to a host and port the handler declares in `egress`; redirects are returned, never followed. */
  fetch(url: string, options?: FetchOptions): Promise<FetchResponse>;
  /** Buffers one key of a declared output; committed only after the handler returns successfully. */
  writeOutput(outputId: string, key: string, data: string | Uint8Array): void;
  /** Writes one structured line to the controller's stderr; non-strings are JSON-encoded. */
  log(level: LogLevel, message: unknown): void;
}

declare global {
  const apsides: HandlerApi;
  function queueMicrotask(callback: () => void): void;
}
