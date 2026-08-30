use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const BUNDLE_MANIFEST_VERSION_V0: &str = "bundle.oxideloop.dev/v0";
pub const DESCRIPTOR_VERSION_V0: &str = "oxideloop.dev/descriptor/v0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolIdentityV0 {
    pub identity: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileIntegrityV0 {
    pub path: String,
    #[ts(type = "number")]
    pub length: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModuleIntegrityV0 {
    pub identity: String,
    #[serde(flatten)]
    pub file: FileIntegrityV0,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub enum ExecutionPhaseV0 {
    Descriptor,
    Render,
    Action,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PhaseModuleV0 {
    pub phase: ExecutionPhaseV0,
    pub module_identity: String,
}

/// The only canonical integrity root for an installed v0 bundle.
///
/// `bundleDigest` deliberately does not appear here: it is SHA-256 over the
/// canonical JSON bytes of this document and lives in the outer install record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BundleIntegrityManifestV0 {
    pub manifest_version: String,
    pub compiler: ToolIdentityV0,
    pub bundler: ToolIdentityV0,
    pub renderer_abi_version: String,
    pub descriptor: FileIntegrityV0,
    pub render_modules: Vec<ModuleIntegrityV0>,
    pub action_modules: Vec<ModuleIntegrityV0>,
    pub source_maps: Vec<FileIntegrityV0>,
    pub phase_modules: Vec<PhaseModuleV0>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BundleInstallRecordV0 {
    pub bundle_digest: String,
    pub manifest: BundleIntegrityManifestV0,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DescriptorModuleV0 {
    pub identity: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BundleDescriptorV0 {
    pub descriptor_version: String,
    pub renderer_abi_version: String,
    pub render_module: DescriptorModuleV0,
    pub action_modules: Vec<DescriptorModuleV0>,
}
