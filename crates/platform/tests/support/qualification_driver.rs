use kube::{
    Config,
    config::{KubeConfigOptions, Kubeconfig},
};
use secrecy::ExposeSecret;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    env,
    fmt::{Display, Formatter},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub(super) const MAX_SETTINGS_BYTES: u64 = 256 * 1024;
pub(super) const MAX_POLICY_BYTES: u64 = 1024 * 1024;
pub(super) const MAX_KUBECONFIG_BYTES: u64 = 1024 * 1024;
pub(super) const MAX_RESULT_BYTES: usize = 128 * 1024;

#[derive(Debug)]
pub(crate) struct DriverError(pub(super) &'static str);

impl Display for DriverError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for DriverError {}

pub(crate) type Result<T> = std::result::Result<T, DriverError>;

#[derive(Clone, Copy)]
pub(super) enum ConfigIdentity {
    Admin,
    Issuer {
        name: &'static str,
        error: &'static str,
    },
}

pub(super) fn load_settings<T: DeserializeOwned>(environment: &'static str) -> Result<T> {
    let path = environment_path(environment)?;
    let bytes = bounded_regular_file(&path, MAX_SETTINGS_BYTES, "step settings are invalid")?;
    let text = std::str::from_utf8(&bytes).map_err(|_| DriverError("step settings are invalid"))?;
    let value =
        compiler_core::parse_strict(text).map_err(|_| DriverError("step settings are invalid"))?;
    serde_json::from_value(value).map_err(|_| DriverError("step settings are invalid"))
}

pub(super) fn trust_policy(path: &Path) -> Result<release::TrustPolicy> {
    let bytes = bounded_regular_file(path, MAX_POLICY_BYTES, "trust policy is invalid")?;
    let text = std::str::from_utf8(&bytes).map_err(|_| DriverError("trust policy is invalid"))?;
    let value =
        compiler_core::parse_strict(text).map_err(|_| DriverError("trust policy is invalid"))?;
    serde_json::from_value(value).map_err(|_| DriverError("trust policy is invalid"))
}

pub(super) fn artifact(path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| DriverError("production candidate path is invalid"))?;
    if !path.is_absolute() || !metadata.file_type().is_dir() {
        return Err(DriverError("production candidate path is invalid"));
    }
    Ok(path.to_path_buf())
}

pub(super) async fn explicit_config(identity: ConfigIdentity) -> Result<Config> {
    platform_api::install_rustls_ring_provider()
        .map_err(|_| DriverError("TLS provider initialization failed"))?;
    let path = environment_path("KUBECONFIG")?;
    let bytes = bounded_regular_file(
        &path,
        MAX_KUBECONFIG_BYTES,
        "dedicated kubeconfig is invalid",
    )?;
    let text =
        std::str::from_utf8(&bytes).map_err(|_| DriverError("dedicated kubeconfig is invalid"))?;
    let kubeconfig =
        Kubeconfig::from_yaml(text).map_err(|_| DriverError("dedicated kubeconfig is invalid"))?;
    validate_kubeconfig_selection(&kubeconfig)?;
    if let ConfigIdentity::Issuer { name, error } = identity {
        let user = kubeconfig
            .auth_infos
            .first()
            .ok_or(DriverError("dedicated kubeconfig is invalid"))?;
        if !issuer_only(user, name) {
            return Err(DriverError(error));
        }
    }
    let current = kubeconfig
        .current_context
        .clone()
        .ok_or(DriverError("dedicated kubeconfig has no current context"))?;
    let options = KubeConfigOptions {
        context: Some(current),
        cluster: None,
        user: None,
    };
    let mut config = Config::from_custom_kubeconfig(kubeconfig, &options)
        .await
        .map_err(|_| DriverError("dedicated kubeconfig load failed"))?;
    if config.cluster_url.scheme_str() != Some("https")
        || config.accept_invalid_certs
        || (config.root_cert.is_none() && config.root_cert_file.is_none())
    {
        return Err(DriverError(
            "dedicated kubeconfig does not verify the API CA",
        ));
    }
    config.default_retry = false;
    config.connect_timeout = Some(Duration::from_secs(10));
    config.read_timeout = Some(Duration::from_secs(30));
    config.write_timeout = Some(Duration::from_secs(30));
    Ok(config)
}

fn validate_kubeconfig_selection(kubeconfig: &Kubeconfig) -> Result<()> {
    let ([cluster], [user], [context]) = (
        kubeconfig.clusters.as_slice(),
        kubeconfig.auth_infos.as_slice(),
        kubeconfig.contexts.as_slice(),
    ) else {
        return Err(DriverError("dedicated kubeconfig is invalid"));
    };
    let current = kubeconfig
        .current_context
        .as_deref()
        .ok_or(DriverError("dedicated kubeconfig has no current context"))?;
    let selected = context
        .context
        .as_ref()
        .ok_or(DriverError("dedicated kubeconfig context is invalid"))?;
    let cluster_config = cluster
        .cluster
        .as_ref()
        .ok_or(DriverError("dedicated kubeconfig cluster is invalid"))?;
    if context.name != current
        || selected.cluster != cluster.name
        || selected.user.as_deref() != Some(user.name.as_str())
        || cluster_config.insecure_skip_tls_verify == Some(true)
        || (cluster_config.certificate_authority.is_none()
            && cluster_config.certificate_authority_data.is_none())
    {
        return Err(DriverError("dedicated kubeconfig selection is invalid"));
    }
    Ok(())
}

fn issuer_only(user: &kube::config::NamedAuthInfo, expected_name: &str) -> bool {
    let Some(auth) = &user.auth_info else {
        return false;
    };
    user.name == expected_name
        && user.other.is_empty()
        && auth
            .token
            .as_ref()
            .is_some_and(|token| !token.expose_secret().is_empty())
        && auth.username.is_none()
        && auth.password.is_none()
        && auth.token_file.is_none()
        && auth.client_certificate.is_none()
        && auth.client_certificate_data.is_none()
        && auth.client_key.is_none()
        && auth.client_key_data.is_none()
        && auth.impersonate.is_none()
        && auth.impersonate_uid.is_none()
        && auth.impersonate_groups.is_none()
        && auth.impersonate_user_extra.is_none()
        && auth.extensions.is_none()
        && auth.auth_provider.is_none()
        && auth.exec.is_none()
        && auth.other.is_empty()
}

pub(super) async fn coordination_store(
    environment: &'static str,
) -> Result<Arc<coordination::SqliteCoordination>> {
    let path = database_path(environment)?;
    Ok(Arc::new(
        coordination::SqliteCoordination::open_local(&path)
            .await
            .map_err(|_| DriverError("local coordination database open failed"))?,
    ))
}

pub(super) async fn inventory_store(
    environment: &'static str,
) -> Result<inventory::SqliteInventory> {
    let path = database_path(environment)?;
    inventory::SqliteInventory::open_local(&path)
        .await
        .map_err(|_| DriverError("local inventory database open failed"))
}

pub(super) fn required<'a>(value: Option<&'a str>, error: &'static str) -> Result<&'a str> {
    let value = value.ok_or(DriverError(error))?;
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(DriverError(error));
    }
    Ok(value)
}

pub(super) fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

pub(super) fn bounded_metadata(uid: &str, resource_version: &str) -> Result<()> {
    if uid.is_empty()
        || uid.len() > 256
        || resource_version.is_empty()
        || resource_version.len() > 256
        || uid.chars().any(char::is_control)
        || resource_version.chars().any(char::is_control)
    {
        return Err(DriverError("result metadata is invalid"));
    }
    Ok(())
}

pub(super) fn emit(value: &impl Serialize, prefix: &str) -> Result<()> {
    let encoded = serde_json::to_string(value)
        .map_err(|_| DriverError("qualification result serialization failed"))?;
    if encoded.len() > MAX_RESULT_BYTES || encoded.chars().any(|character| character == '\n') {
        return Err(DriverError("qualification result exceeds its bound"));
    }
    println!("{prefix}{encoded}");
    Ok(())
}

fn environment_path(name: &'static str) -> Result<PathBuf> {
    let value = env::var_os(name).ok_or(DriverError("required path environment is absent"))?;
    let paths = env::split_paths(&value).collect::<Vec<_>>();
    let [path] = paths.as_slice() else {
        return Err(DriverError("required path environment is invalid"));
    };
    if !path.is_absolute() {
        return Err(DriverError("required path environment is invalid"));
    }
    Ok(path.clone())
}

fn database_path(name: &'static str) -> Result<PathBuf> {
    let path = environment_path(name)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Ok(_) | Err(_) => Err(DriverError("local database path is invalid")),
    }
}

fn bounded_regular_file(path: &Path, maximum: u64, error: &'static str) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).map_err(|_| DriverError(error))?;
    if !path.is_absolute()
        || !metadata.file_type().is_file()
        || metadata.len() == 0
        || metadata.len() > maximum
    {
        return Err(DriverError(error));
    }
    let bytes = fs::read(path).map_err(|_| DriverError(error))?;
    let length = u64::try_from(bytes.len()).map_err(|_| DriverError(error))?;
    if bytes.is_empty() || length > maximum {
        return Err(DriverError(error));
    }
    Ok(bytes)
}
