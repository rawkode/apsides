import process from "node:process";
import { pathToFileURL } from "node:url";

const DEPENDENCY = {
  name: "aps-isolation-dependency.ts",
  source: 'export const declared = "present";\n',
} as const;
const DEPENDENCY_PREFIX = `import { declared as isolationDependency } from "./${DEPENDENCY.name}";
if (isolationDependency !== "present") throw new Error("REAKT_ISOLATION_DEPENDENCY_INVALID");
`;

const PROBES = {
  declared: { prefix: DEPENDENCY_PREFIX, dependency: DEPENDENCY },
  undeclared: { prefix: DEPENDENCY_PREFIX, dependency: DEPENDENCY },
  clock: { prefix: "Date.now();\n" },
  random: { prefix: "Math.random();\n" },
  env: {
    prefix: `const isolationGlobal = globalThis as unknown as { Deno?: { env?: { get?: (name: string) => string | undefined } } };
if (typeof isolationGlobal.Deno?.env?.get !== "function") throw new Error("REAKT_ISOLATION_ENV_ABSENT");
throw new Error("REAKT_ISOLATION_ENV_PRESENT");
`,
  },
  read: {
    prefix: `const isolationGlobal = globalThis as unknown as { Deno?: { readTextFile?: (path: string) => Promise<string> } };
if (typeof isolationGlobal.Deno?.readTextFile !== "function") throw new Error("REAKT_ISOLATION_READ_ABSENT");
throw new Error("REAKT_ISOLATION_READ_PRESENT");
`,
  },
  net: {
    prefix: `const isolationGlobal = globalThis as unknown as { fetch?: unknown };
if (typeof isolationGlobal.fetch !== "function") throw new Error("REAKT_ISOLATION_NET_ABSENT");
throw new Error("REAKT_ISOLATION_NET_PRESENT");
`,
  },
} as const;

export type Probe = keyof typeof PROBES;
export type Rejection =
  | "undeclared-source-module"
  | "clock-unavailable"
  | "random-unavailable"
  | "environment-unavailable"
  | "filesystem-unavailable"
  | "network-unavailable";

const probeNames = Object.keys(PROBES) as Probe[];

function isProbe(value: string): value is Probe {
  return probeNames.some(probe => probe === value);
}

function diagnostic(error: unknown): string | undefined {
  if (!(error instanceof Error)) return undefined;
  return error.message.replaceAll(/\x1b\[[0-9;]*m/g, "");
}

function evaluationMessage(message: string): string | undefined {
  const match = /^\/[^\r\n]*\/deno failed \([1-9]\d*\): error: Uncaught(?: \(in promise\))? (?:Error|TypeError): ([^\r\n]+)(?:\r?\n|$)/.exec(message);
  return match?.[1];
}

export function classifyDiagnostic(probe: Probe, message: string): Rejection | undefined {
  switch (probe) {
    case "undeclared":
      return message.startsWith("/")
        && /^\/[^\r\n]*\/deno failed \([1-9]\d*\):/.test(message)
        && message.includes("TS2307")
        && message.includes("Cannot find module")
        && message.includes(DEPENDENCY.name)
        && !message.includes("error: Uncaught")
        ? "undeclared-source-module"
        : undefined;
    case "clock":
      return evaluationMessage(message) === "secure mode Calling %SharedDate%.now() throws"
        ? "clock-unavailable"
        : undefined;
    case "random":
      return evaluationMessage(message) === "secure mode %SharedMath%.random() throws"
        ? "random-unavailable"
        : undefined;
    case "env":
      return evaluationMessage(message) === "REAKT_ISOLATION_ENV_ABSENT"
        ? "environment-unavailable"
        : undefined;
    case "read":
      return evaluationMessage(message) === "REAKT_ISOLATION_READ_ABSENT"
        ? "filesystem-unavailable"
        : undefined;
    case "net":
      return evaluationMessage(message) === "REAKT_ISOLATION_NET_ABSENT"
        ? "network-unavailable"
        : undefined;
    case "declared":
      return undefined;
  }
}

function rejection(probe: Probe, error: unknown): Rejection | undefined {
  const message = diagnostic(error);
  return message === undefined ? undefined : classifyDiagnostic(probe, message);
}

function describe(): void {
  console.log(JSON.stringify({ schemaVersion: "reakt.dev/isolation-probes/v1", probes: PROBES }));
}

async function run(source: string, output: string, probe: Probe): Promise<boolean> {
  const { compileReakt } = await import("../../packages/bundle-builder/src/reakt.ts");
  try {
    await compileReakt(source, output);
  } catch (error) {
    const reason = rejection(probe, error);
    if (reason === undefined) return false;
    console.log(JSON.stringify({ probe, outcome: "rejected", reason }));
    return true;
  }
  if (probe !== "declared") return false;
  console.log(JSON.stringify({ probe, outcome: "compiled" }));
  return true;
}

async function main(): Promise<void> {
  const arguments_ = process.argv.slice(2);
  if (arguments_.length === 1 && arguments_[0] === "--describe") {
    describe();
    return;
  }
  const [source, output, expectation, ...extra] = arguments_;
  if (!source || !output || !expectation || extra.length > 0 || !isProbe(expectation)) {
    console.error("usage: isolation_probe.mts SOURCE OUTPUT EXPECTATION");
    process.exitCode = 2;
    return;
  }
  if (!(await run(source, output, expectation))) {
    console.error("reakt: isolation probe did not produce its expected outcome");
    process.exitCode = 1;
  }
}

const entry = process.argv[1];
if (entry !== undefined && import.meta.url === pathToFileURL(entry).href) {
  void main().catch(() => {
    console.error("reakt: isolation probe failed");
    process.exitCode = 1;
  });
}
