import { createHash } from "node:crypto";

/** The only five P8A envelope members, in frozen lexicographic order. */
export const BUNDLE_ARCHIVE_ENTRY_NAMES = [
  "action.callable.js",
  "bundle-install.json",
  "descriptor.json",
  "integrity-manifest.json",
  "render.callable.js",
] as const;

export type BundleArchiveEntryName = (typeof BUNDLE_ARCHIVE_ENTRY_NAMES)[number];
export type BundleArchiveEntries = Readonly<Record<BundleArchiveEntryName, Uint8Array>>;

const MAGIC = Buffer.from("OLBA", "ascii");
const VERSION = 1;
const MAX_ARCHIVE_BYTES = 128 * 1024;
const MAX_ENTRY_BYTES = 128 * 1024;
const typedArrayPrototype = Object.getPrototypeOf(Uint8Array.prototype) as object;
const typedArrayByteLength = trustedGetter("byteLength") as (this: unknown) => number;
const typedArrayByteOffset = trustedGetter("byteOffset") as (this: unknown) => number;
const typedArrayBuffer = trustedGetter("buffer") as (this: unknown) => ArrayBufferLike;
const arrayBufferByteLength = trustedArrayBufferGetter("byteLength") as (this: unknown) => number;

/**
 * Encode exactly five already-owned, already-bounded byte arrays. The v1 wire
 * format is `OLBA`, version/count, then ordered path/u32-big-endian-length/
 * 64-character lowercase SHA-256 headers, then contiguous payloads and EOF.
 * This pure API deliberately accepts no filesystem paths.
 */
export function encodeBundleArchive(entries: BundleArchiveEntries): Buffer {
  const supplied = Object.keys(entries).sort(compareUtf16);
  if (JSON.stringify(supplied) !== JSON.stringify(BUNDLE_ARCHIVE_ENTRY_NAMES)) {
    throw new Error("bundle archive requires exactly its five frozen entries");
  }
  const snapshots = new Map<BundleArchiveEntryName, Buffer>();
  let total = MAGIC.length + 2;
  for (const name of BUNDLE_ARCHIVE_ENTRY_NAMES) {
    const bytes = snapshotUint8Array(entries[name], name);
    if (bytes.byteLength > MAX_ENTRY_BYTES) throw new Error(`archive entry exceeds 128KiB: ${name}`);
    snapshots.set(name, bytes);
    const path = Buffer.from(name, "utf8");
    total += 1 + path.length + 4 + 64 + bytes.byteLength;
    if (total > MAX_ARCHIVE_BYTES) throw new Error("bundle archive exceeds 128KiB");
  }
  const header: Buffer[] = [MAGIC, Buffer.from([VERSION, BUNDLE_ARCHIVE_ENTRY_NAMES.length])];
  for (const name of BUNDLE_ARCHIVE_ENTRY_NAMES) {
    const bytes = snapshots.get(name);
    if (!bytes) throw new Error(`missing archive snapshot: ${name}`);
    const path = Buffer.from(name, "utf8");
    const length = Buffer.allocUnsafe(4);
    length.writeUInt32BE(bytes.byteLength);
    header.push(Buffer.from([path.length]), path, length, Buffer.from(hash(bytes), "ascii"));
  }
  const encoded = Buffer.allocUnsafe(total);
  let offset = 0;
  for (const part of header) {
    part.copy(encoded, offset);
    offset += part.length;
  }
  for (const name of BUNDLE_ARCHIVE_ENTRY_NAMES) {
    const bytes = snapshots.get(name);
    if (!bytes) throw new Error(`missing archive snapshot: ${name}`);
    encoded.set(bytes, offset);
    offset += bytes.byteLength;
  }
  return encoded;
}

function snapshotUint8Array(value: unknown, name: string): Buffer {
  if (!(value instanceof Uint8Array)) throw new Error(`archive entry is not a Uint8Array: ${name}`);
  let byteLength: number;
  let byteOffset: number;
  let buffer: ArrayBufferLike;
  try {
    byteLength = typedArrayByteLength.call(value);
    byteOffset = typedArrayByteOffset.call(value);
    buffer = typedArrayBuffer.call(value) as ArrayBufferLike;
  } catch {
    throw new Error(`archive entry is not a real Uint8Array: ${name}`);
  }
  if (byteLength < 0 || byteLength > MAX_ENTRY_BYTES || byteOffset < 0) {
    throw new Error(`archive entry exceeds 128KiB: ${name}`);
  }
  try {
    const backingLength = arrayBufferByteLength.call(buffer);
    if (byteOffset + byteLength > backingLength) throw new Error("view exceeds backing buffer");
    return Buffer.from(new Uint8Array(buffer, byteOffset, byteLength));
  } catch {
    throw new Error(`archive entry must use an unshared ArrayBuffer: ${name}`);
  }
}

function trustedGetter(name: string): () => unknown {
  const getter = Object.getOwnPropertyDescriptor(typedArrayPrototype, name)?.get;
  if (!getter) throw new Error("TypedArray intrinsics are unavailable");
  return getter;
}

function trustedArrayBufferGetter(name: string): () => unknown {
  const getter = Object.getOwnPropertyDescriptor(ArrayBuffer.prototype, name)?.get;
  if (!getter) throw new Error("ArrayBuffer intrinsics are unavailable");
  return getter;
}

function hash(value: Uint8Array): string {
  return createHash("sha256").update(value).digest("hex");
}

function compareUtf16(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0;
}
