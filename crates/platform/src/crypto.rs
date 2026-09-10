use crate::{Result, invalid};
use std::sync::OnceLock;

/// Select the process-wide Rustls `ring` provider before constructing a
/// Kubernetes TLS client.
///
/// Applications must call this before `kube::Config` or `kube::Client` can
/// build a Rustls configuration. Repeated Apsides calls are idempotent; a provider
/// installed earlier by unrelated code is rejected because its identity cannot
/// be established as the supported provider.
///
/// # Errors
/// Returns an error if another TLS path initialized Rustls first or installation
/// of the supported provider fails.
pub fn install_rustls_ring_provider() -> Result<()> {
    static INSTALLATION: OnceLock<std::result::Result<(), &'static str>> = OnceLock::new();
    match INSTALLATION.get_or_init(|| {
        if rustls::crypto::CryptoProvider::get_default().is_some() {
            return Err("Rustls crypto provider was initialized before Apsides selected ring");
        }
        rustls::crypto::ring::default_provider()
            .install_default()
            .map_err(|_| "failed to install the process-wide Rustls ring crypto provider")
    }) {
        Ok(()) => Ok(()),
        Err(message) => Err(invalid(message)),
    }
}

#[cfg(test)]
mod tests {
    use super::install_rustls_ring_provider;
    use kube::{Client, Config};

    #[tokio::test]
    async fn explicit_provider_constructs_a_kubernetes_tls_client() {
        install_rustls_ring_provider().unwrap();
        install_rustls_ring_provider().unwrap();
        let config = Config::new("https://127.0.0.1:6443".parse().unwrap());
        Client::try_from(config).unwrap();
    }
}
