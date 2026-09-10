import assert from "node:assert/strict";
import test from "node:test";

import { classifyDiagnostic, type Probe, type Rejection } from "./isolation_probe.mts";

const DENO = "/nix/store/fixture-deno/bin/deno failed (1): ";

const runtimeDiagnostic = (type: "Error" | "TypeError", message: string): string => {
  return `${DENO}error: Uncaught (in promise) ${type}: ${message}\n    at eval (source.tsx:1:1)`;
};

test("classifies only anchored Deno evaluation failures", () => {
  const cases: ReadonlyArray<[Probe, string, "Error" | "TypeError", Rejection]> = [
    ["clock", "secure mode Calling %SharedDate%.now() throws", "TypeError", "clock-unavailable"],
    ["random", "secure mode %SharedMath%.random() throws", "TypeError", "random-unavailable"],
    ["env", "APSIDES_ISOLATION_ENV_ABSENT", "Error", "environment-unavailable"],
    ["read", "APSIDES_ISOLATION_READ_ABSENT", "Error", "filesystem-unavailable"],
    ["net", "APSIDES_ISOLATION_NET_ABSENT", "Error", "network-unavailable"],
  ];

  for (const [probe, message, type, expected] of cases) {
    assert.equal(classifyDiagnostic(probe, runtimeDiagnostic(type, message)), expected);
  }
});

test("does not classify echoed sentinels or non-Deno failures", () => {
  const typecheck = `${DENO}Check source.tsx\nTS2322 [ERROR]: Type 'string' is not assignable to type 'number'.\nconst broken: number = "APSIDES_ISOLATION_ENV_ABSENT";\nerror: Type checking failed.`;
  const unrelated = "oxc failed: error: Uncaught Error: APSIDES_ISOLATION_ENV_ABSENT";
  const zeroStatus = runtimeDiagnostic("Error", "APSIDES_ISOLATION_ENV_ABSENT")
    .replace("failed (1)", "failed (0)");

  assert.equal(classifyDiagnostic("env", typecheck), undefined);
  assert.equal(classifyDiagnostic("env", unrelated), undefined);
  assert.equal(classifyDiagnostic("env", zeroStatus), undefined);
});

test("undeclared modules require the anchored Deno typecheck envelope", () => {
  const missing = `${DENO}Check source.tsx\nTS2307 [ERROR]: Cannot find module 'file:///source/aps-isolation-dependency.ts'.\nerror: Type checking failed.`;

  assert.equal(classifyDiagnostic("undeclared", missing), "undeclared-source-module");
  assert.equal(classifyDiagnostic("undeclared", `oxc failed: ${missing}`), undefined);
  assert.equal(
    classifyDiagnostic("undeclared", runtimeDiagnostic("Error", "aps-isolation-dependency.ts")),
    undefined,
  );
});
