import { Database } from "bun:sqlite";
import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { ResolvedResource } from "./contracts.ts";

export interface InventoryEntry {
  desired?: ResolvedResource;
  uid?: string;
  finalizerKeys: string[];
  deletionPolicy: "retain" | "delete";
  dependsOn: string[];
  deleting?: boolean;
}

export interface Inventory {
  version: 1;
  controller: string;
  resources: Record<string, InventoryEntry>;
}

export function readInventory(directory: string, controller: string): Inventory {
  const path = join(directory, "inventory.json");
  let raw: string;
  try { raw = readFileSync(path, "utf8"); }
  catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return { version: 1, controller, resources: {} };
    throw error;
  }
  const value = JSON.parse(raw) as Inventory;
  if (value.version !== 1 || value.controller !== controller || !value.resources || typeof value.resources !== "object") {
    throw new Error("Inventory belongs to a different controller or has an unsupported format");
  }
  return value;
}

export function saveInventory(directory: string, inventory: Inventory): void {
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  const temporary = join(directory, `inventory.${crypto.randomUUID()}.tmp`);
  writeFileSync(temporary, JSON.stringify(inventory, null, 2) + "\n", { mode: 0o600 });
  renameSync(temporary, join(directory, "inventory.json"));
}

/** OS-released SQLite write lock; supports local processes, not distributed HA. */
export async function withControllerLock<T>(directory: string, run: () => Promise<T>): Promise<T> {
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  const db = new Database(join(directory, "writer-lock.sqlite"));
  let acquired = false;
  try {
    db.exec("PRAGMA busy_timeout=0");
    try { db.exec("BEGIN IMMEDIATE"); acquired = true; }
    catch (error) {
      if ((error as { code?: string }).code === "SQLITE_BUSY") throw new Error("Another controller owns this state directory");
      throw error;
    }
    return await run();
  } finally {
    if (acquired) db.exec("ROLLBACK");
    db.close();
  }
}
