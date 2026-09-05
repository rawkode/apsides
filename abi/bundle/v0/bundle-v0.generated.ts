// Generated from bundle-contract Rust types. Do not edit.

export type ToolIdentityV0 = { identity: string, version: string, };

export type FileIntegrityV0 = { path: string, length: number, sha256: string, };

export type ModuleIntegrityV0 = { identity: string, path: string, length: number, sha256: string, };

export type ExecutionPhaseV0 = "descriptor" | "render" | "action";

export type PhaseModuleV0 = { phase: ExecutionPhaseV0, moduleIdentity: string, };

export type BundleIntegrityManifestV0 = { manifestVersion: string, compiler: ToolIdentityV0, bundler: ToolIdentityV0, rendererAbiVersion: string, descriptor: FileIntegrityV0, renderModules: Array<ModuleIntegrityV0>, actionModules: Array<ModuleIntegrityV0>, sourceMaps: Array<FileIntegrityV0>, phaseModules: Array<PhaseModuleV0>, };

export type BundleInstallRecordV0 = { bundleDigest: string, manifest: BundleIntegrityManifestV0, };
