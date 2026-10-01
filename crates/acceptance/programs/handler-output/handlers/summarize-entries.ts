/**
 * Reads the `handler-derived` output the `derive-keys` handler writes and
 * records its whole `data` as one `entries` key of the `handler-summary`
 * output: sorted `key=value` pairs joined by `;`. It runs only once
 * `derive-keys` is settled, so `entries` always reflects a complete
 * upstream commit.
 */

import type { HandlerEvent } from "@apsides/kubernetes/handler";

export async function summarizeEntries(
  event: HandlerEvent<"handler-derived">,
): Promise<{ entries: string }> {
  const data = event.inputs["handler-derived"].data;
  const entries = Object.keys(data)
    .sort()
    .map((key) => `${key}=${data[key]}`)
    .join(";");
  apsides.writeOutput("handler-summary", "entries", entries);
  return { entries };
}
