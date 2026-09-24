/**
 * Copies every key of the `handler-source` input's `data` into the
 * `handler-derived` output, unchanged. No egress: the whole point of this
 * handler is to exercise the runtime's own commit path (uid binding,
 * per-key SSA ownership), not any capability the handler itself uses.
 */
export async function deriveKeys(event: {
  inputs: { [id: string]: { data?: Record<string, string> } };
}): Promise<{ keys: string[] }> {
  const data = event.inputs["handler-source"]?.data ?? {};
  const keys = Object.keys(data).sort();
  for (const key of keys) {
    apsides.writeOutput("handler-derived", key, data[key] ?? "");
  }
  return { keys };
}
