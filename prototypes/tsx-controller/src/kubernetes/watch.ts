import { request as httpsRequest } from "node:https";
import { request as httpRequest } from "node:http";
import { readFileSync } from "node:fs";
import type { Observation } from "../contracts";
import { observationKey } from "../authoring/context";
import { resourcePath, type Connection } from "./client";

/** Named watches wake reconciliation; the deadline forces a fresh GET after gaps/410s. */
export async function waitForChanges(
  connection: Connection, namespace: string, observations: Observation[], versions: Map<string, string>,
  signal: AbortSignal, timeoutMs: number,
): Promise<"watch" | "refresh" | "stopped"> {
  if (signal.aborted) return "stopped";
  return new Promise(resolve => {
    const requests = new Set<ReturnType<typeof httpsRequest>>();
    let done = false;
    const finish = (reason: "watch" | "refresh" | "stopped") => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      signal.removeEventListener("abort", stop);
      for (const request of requests) request.destroy();
      resolve(reason);
    };
    const timer = setTimeout(() => finish("refresh"), timeoutMs);
    const stop = () => finish("stopped");
    signal.addEventListener("abort", stop, { once: true });
    for (const target of observations) {
      const named = resourcePath(target, namespace);
      const url = new URL(named.slice(0, named.lastIndexOf("/")), connection.server);
      url.search = new URLSearchParams({ watch: "true", fieldSelector: `metadata.name=${target.name}`,
        resourceVersion: versions.get(observationKey(target)) ?? "", allowWatchBookmarks: "true",
        timeoutSeconds: String(Math.max(1, Math.ceil(timeoutMs / 1000))) }).toString();
      let token: string | undefined;
      try { token = connection.tokenFile ? readFileSync(connection.tokenFile, "utf8").trim() : connection.token; }
      catch { continue; } // The refresh cycle reports credential errors with normal backoff.
      const request = (url.protocol === "http:" ? httpRequest : httpsRequest)(url, {
        ca: connection.ca, rejectUnauthorized: !connection.insecureSkipTLSVerify,
        headers: { Accept: "application/json", ...(token ? { Authorization: `Bearer ${token}` } : {}) },
      }, response => {
        if (response.statusCode !== 200) { response.resume(); return; }
        let buffer = "";
        response.setEncoding("utf8");
        response.on("data", chunk => {
          buffer += chunk;
          for (;;) {
            const boundary = buffer.indexOf("\n");
            if (boundary < 0) break;
            const line = buffer.slice(0, boundary); buffer = buffer.slice(boundary + 1);
            if (!line.trim()) continue;
            try {
              const event = JSON.parse(line);
              if (["ADDED", "MODIFIED", "DELETED"].includes(event.type)) finish("watch");
              // ERROR (including expired resource versions) falls back to deadline refresh.
            } catch { /* Retry with a fresh snapshot at the deadline. */ }
          }
        });
        response.on("error", () => {});
      });
      requests.add(request);
      request.on("close", () => requests.delete(request));
      request.on("error", () => {});
      request.end();
    }
    if (signal.aborted) stop();
  });
}
