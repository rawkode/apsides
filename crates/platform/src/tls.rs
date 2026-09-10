use crate::{Result, invalid};
use k8s_openapi::{ByteString, api::core::v1::Secret};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose,
};
use std::collections::BTreeMap;
use time::{Duration, OffsetDateTime};

/// Generate a fresh private generation CA and exactly two TLS identities using
/// rcgen/ring. Only its public CA is retained. Secret data is never logged.
pub(crate) fn generation_tls_secret(
    name: &str,
    namespace: &str,
    generation: &str,
    lifetime_seconds: i64,
) -> Result<Secret> {
    if !(86_400..=31_536_000).contains(&lifetime_seconds) {
        return Err(invalid(
            "proxy certificate lifetime must be one day to one year",
        ));
    }
    let now = OffsetDateTime::now_utc();
    let mut ca_params = parameters(vec![], now, lifetime_seconds)?;
    ca_params.distinguished_name = DistinguishedName::new();
    ca_params.distinguished_name.push(
        DnType::CommonName,
        format!("Apsides generation {generation} CA"),
    );
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_key = KeyPair::generate().map_err(|_| invalid("TLS CA key generation failed"))?;
    let ca = ca_params
        .self_signed(&ca_key)
        .map_err(|_| invalid("TLS CA certificate generation failed"))?;
    let issuer = Issuer::new(ca_params, ca_key);
    let server_dns = format!("apsides-proxy-{generation}.{namespace}.svc");
    let server = leaf(
        vec![server_dns],
        ExtendedKeyUsagePurpose::ServerAuth,
        now,
        lifetime_seconds,
        &issuer,
    )?;
    let client = leaf(
        vec![format!("apsides-runtime-{generation}")],
        ExtendedKeyUsagePurpose::ClientAuth,
        now,
        lifetime_seconds,
        &issuer,
    )?;
    let data: BTreeMap<String, ByteString> = [
        ("ca.crt", ca.pem()),
        ("server.crt", server.0),
        ("server.key", server.1),
        ("client.crt", client.0),
        ("client.key", client.1),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), ByteString(value.into_bytes())))
    .collect();
    Ok(Secret {
        metadata: kube::core::ObjectMeta {
            name: Some(name.into()),
            namespace: Some(namespace.into()),
            ..Default::default()
        },
        immutable: Some(true),
        type_: Some("Opaque".into()),
        data: Some(data),
        ..Default::default()
    })
}

fn parameters(names: Vec<String>, now: OffsetDateTime, lifetime: i64) -> Result<CertificateParams> {
    let mut params =
        CertificateParams::new(names).map_err(|_| invalid("invalid TLS certificate identity"))?;
    params.not_before = now - Duration::minutes(5);
    params.not_after = now + Duration::seconds(lifetime);
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    Ok(params)
}

fn leaf(
    names: Vec<String>,
    purpose: ExtendedKeyUsagePurpose,
    now: OffsetDateTime,
    lifetime: i64,
    issuer: &Issuer<'_, KeyPair>,
) -> Result<(String, String)> {
    let common_name = names
        .first()
        .cloned()
        .ok_or_else(|| invalid("TLS leaf identity missing"))?;
    let mut params = parameters(names, now, lifetime)?;
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    params.use_authority_key_identifier_extension = true;
    params.extended_key_usages = vec![purpose];
    let key = KeyPair::generate().map_err(|_| invalid("TLS leaf key generation failed"))?;
    let certificate = params
        .signed_by(&key, issuer)
        .map_err(|_| invalid("TLS leaf certificate generation failed"))?;
    Ok((certificate.pem(), key.serialize_pem()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generations_never_share_ca_or_leaf_private_keys() {
        let first = generation_tls_secret("tls-one", "control", "one", 86_400).unwrap();
        let second = generation_tls_secret("tls-two", "control", "two", 86_400).unwrap();
        let first = first.data.unwrap();
        let second = second.data.unwrap();
        assert_eq!(first.len(), 5);
        assert_ne!(first["ca.crt"], second["ca.crt"]);
        assert_ne!(first["client.key"], first["server.key"]);
        assert!(!first.contains_key("ca.key"));
        assert!(generation_tls_secret("tls", "control", "one", 60).is_err());
    }

    #[test]
    fn copied_old_client_certificate_is_rejected_by_new_generation_trust() {
        let old = generation_tls_secret("old", "control", "old", 86_400)
            .unwrap()
            .data
            .unwrap();
        let new = generation_tls_secret("new", "control", "new", 86_400)
            .unwrap()
            .data
            .unwrap();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("apsides-tls-trust-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        // Only public certificates are written. No private key leaves Secret data.
        std::fs::write(root.join("old-ca.pem"), &old["ca.crt"].0).unwrap();
        std::fs::write(root.join("new-ca.pem"), &new["ca.crt"].0).unwrap();
        std::fs::write(root.join("old-client.pem"), &old["client.crt"].0).unwrap();
        let verify = |ca: &str| {
            std::process::Command::new("openssl")
                .arg("verify")
                .arg("-purpose")
                .arg("sslclient")
                .arg("-CAfile")
                .arg(root.join(ca))
                .arg(root.join("old-client.pem"))
                .output()
                .unwrap()
        };
        let own = verify("old-ca.pem");
        let foreign = verify("new-ca.pem");
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            own.status.success(),
            "standard TLS verifier rejected own generation: {}",
            String::from_utf8_lossy(&own.stderr)
        );
        assert!(
            !foreign.status.success(),
            "copied client crossed generation CA boundary"
        );
    }
}
