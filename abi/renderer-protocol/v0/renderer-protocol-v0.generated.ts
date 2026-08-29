// Generated from oxideloop-renderer-protocol Rust types. Do not edit.

export type PhaseV0 = "descriptor" | "render" | "action";

export type ResponseStatusV0 = "ok" | "recoverable" | "fatal" | "protocol";

export type ErrorCategoryV0 = "invalidInput" | "guestFailure" | "bundleUnavailable" | "deadlineExceeded" | "heapLimit" | "memoryKilled" | "processLost" | "integrityFailure" | "internal" | "protocolViolation";

export type RequestIdV0 = { bytes: number[], };

export type RequestV0 = { requestId: RequestIdV0, phase: PhaseV0, bundleDigest: string, moduleDigest: string | null, durationMillis: number, payload: number[], };

export type ResponseV0 = { requestId: RequestIdV0, phase: PhaseV0, bundleDigest: string, moduleDigest: string | null, status: ResponseStatusV0, errorCategory: ErrorCategoryV0 | null, payload: number[], };
