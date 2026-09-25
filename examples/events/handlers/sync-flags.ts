/**
 * Fetches the feature flags for the environment the `feature-settings`
 * input names from the flag service, and writes one `FEATURE_<NAME>` key
 * per flag (`"true"` or `"false"`), plus `FLAGS_ENVIRONMENT`, into the
 * `feature-flags` output.
 *
 * Any failure -- a missing or malformed `environment`, the flag service
 * being unreachable or answering anything but 2xx, or a response that is
 * not the expected document -- throws before any key is written. The
 * runtime commits nothing for a failed invocation, so the storefront keeps
 * running with the last flags that were fetched successfully while the
 * runtime retries this event with backoff.
 */

/** The handler globals the controller's embedded engine provides. */
declare const apsides: {
  fetch(
    url: string,
    options?: { readonly method?: string; readonly headers?: Readonly<Record<string, string>> },
  ): Promise<{ readonly status: number; readonly body: Uint8Array }>;
  writeOutput(id: string, key: string, data: string): void;
  log(level: "debug" | "info" | "warn" | "error", message: string): void;
};

/** The only origin this handler reaches; `<On egress>` declares it. */
const FLAG_SERVICE_ORIGIN = "https://flags.example.com";
const ENVIRONMENT = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;
const FLAG_NAME = /^[a-z][a-z0-9-]{0,62}$/;

interface SettingsEvent {
  readonly inputs: { readonly [id: string]: { readonly data?: Readonly<Record<string, string>> } };
}

/** Decodes strict UTF-8; throws a `URIError` on invalid bytes. */
const decodeUtf8 = (bytes: Uint8Array): string => {
  let escaped = "";
  for (const byte of bytes) escaped += `%${byte.toString(16).padStart(2, "0")}`;
  return decodeURIComponent(escaped);
};

/** `{"flags": {"<name>": <boolean>, ...}}`, as sorted `[name, enabled]` pairs. */
const parseFlags = (text: string): [string, boolean][] => {
  const document: unknown = JSON.parse(text);
  const flags = typeof document === "object" && document !== null
    ? (document as { flags?: unknown }).flags
    : undefined;
  if (typeof flags !== "object" || flags === null || Array.isArray(flags)) {
    throw new Error("flag service response has no flags object");
  }
  const entries = Object.entries(flags).sort(([left], [right]) => left < right ? -1 : 1);
  for (const [name, enabled] of entries) {
    if (!FLAG_NAME.test(name) || typeof enabled !== "boolean") {
      throw new Error(`flag service response has an invalid flag: ${JSON.stringify(name)}`);
    }
  }
  return entries as [string, boolean][];
};

const outputKey = (flag: string): string => `FEATURE_${flag.toUpperCase().replaceAll("-", "_")}`;

export async function syncFlags(event: SettingsEvent): Promise<{ environment: string; flags: number }> {
  const environment = event.inputs["feature-settings"]?.data?.["environment"];
  if (environment === undefined || !ENVIRONMENT.test(environment)) {
    throw new Error("feature-settings must set environment to a lowercase DNS label");
  }
  const url = `${FLAG_SERVICE_ORIGIN}/v1/environments/${environment}/flags`;
  let response: { readonly status: number; readonly body: Uint8Array };
  try {
    response = await apsides.fetch(url, { headers: { accept: "application/json" } });
  } catch (error) {
    apsides.log("warn", `flag service unreachable for ${environment}: ${String(error)}`);
    throw error;
  }
  if (response.status < 200 || response.status > 299) {
    apsides.log("warn", `flag service answered HTTP ${response.status} for ${environment}`);
    throw new Error(`flag service answered HTTP ${response.status}`);
  }
  const flags = parseFlags(decodeUtf8(response.body));
  for (const [name, enabled] of flags) {
    apsides.writeOutput("feature-flags", outputKey(name), enabled ? "true" : "false");
  }
  apsides.writeOutput("feature-flags", "FLAGS_ENVIRONMENT", environment);
  apsides.log("info", `wrote ${flags.length} flags for ${environment}`);
  return { environment, flags: flags.length };
}
