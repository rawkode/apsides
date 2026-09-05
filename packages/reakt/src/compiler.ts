import { Domain, type DomainOptions } from "./legacy.ts";
import { renderTree } from "./runtime.ts";

/** This subpath is available only to the trusted compiler-generated wrapper. */
export function renderExport(defaultExport: unknown, buildContext: { readonly provenance: DomainOptions["provenance"]; readonly sourceDigest?: string }): { productionIR: unknown; fixtures: unknown } {
  const value = typeof defaultExport === "function" ? defaultExport(buildContext) : defaultExport;
  if (value instanceof Domain) return { productionIR: value.toIR(), fixtures: null };
  if (value && typeof value === "object" && "productionIR" in value) {
    const result = value as { productionIR: unknown; fixtures?: unknown };
    return { productionIR: result.productionIR instanceof Domain ? result.productionIR.toIR() : result.productionIR, fixtures: result.fixtures ?? null };
  }
  if (value && typeof value === "object" && "schemaVersion" in value) return { productionIR: value, fixtures: null };
  return { productionIR: renderTree(value, buildContext.provenance), fixtures: null };
}
