#![forbid(unsafe_code)]
//! An in-process stand-in for a Sigstore instance, for tests only.
//!
//! [`Authority`] is a throwaway certificate authority and transparency log.
//! For each signature it issues a short-lived certificate carrying the Fulcio
//! claims a CI workload's certificate carries, signs an in-toto statement into
//! a DSSE envelope with it, and records that envelope in a one-entry log with
//! an inclusion proof, a signed checkpoint and a signed entry timestamp. The
//! bundle it writes and the trusted root it describes have the shape a real
//! instance produces, so the release verifier checks them on its production
//! path with no network and no external program.
//!
//! It issues no signed certificate timestamps: a policy that requires
//! certificate transparency rejects its bundles.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rcgen::{
    BasicConstraints, CertificateParams, CustomExtension, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, PublicKeyData as _, SanType,
};
use ring::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair},
};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{path::Path, sync::OnceLock};
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The DSSE payload type of an in-toto statement.
const PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
/// The checkpoint origin of the authority's log.
const ORIGIN: &str = "log.test-sigstore.invalid";
/// The Fulcio extension arc the CI claims live under.
const FULCIO_ARC: [u64; 8] = [1, 3, 6, 1, 4, 1, 57264, 1];

/// The CI workload a signing certificate is issued to, as its Fulcio claims
/// describe it.
#[derive(Clone, Debug)]
pub struct Workload {
    /// The certificate's URI subject alternative name, which is also its
    /// build signer URI.
    pub identity: String,
    pub issuer: String,
    pub signer_digest: String,
    pub runner_environment: String,
    pub repository: String,
    pub commit: String,
    pub source_ref: String,
}

impl Workload {
    /// A GitHub-hosted Actions run of `repository`'s release workflow on its
    /// main branch at `commit`.
    #[must_use]
    pub fn github(repository: &str, commit: &str) -> Self {
        Self {
            identity: format!("{repository}/.github/workflows/release.yml@refs/heads/main"),
            issuer: "https://token.actions.githubusercontent.com".into(),
            signer_digest: commit.into(),
            runner_environment: "github-hosted".into(),
            repository: repository.into(),
            commit: commit.into(),
            source_ref: "refs/heads/main".into(),
        }
    }

    /// A trust policy `signature` approving exactly this workload's
    /// signatures under `trusted_root`, requiring the transparency log the
    /// authority keeps and not certificate transparency, which it lacks.
    #[must_use]
    pub fn signature_policy(&self, trusted_root: &Value) -> Value {
        json!({
            "trustedRoot": trusted_root,
            "transparencyLog": true,
            "certificateTransparency": false,
            "certificateIdentity": self.identity,
            "certificateIssuer": self.issuer,
            "signerDigest": self.signer_digest,
            "sourceRef": self.source_ref,
        })
    }

    fn claims(&self) -> Vec<CustomExtension> {
        [
            (8, &self.issuer),
            (9, &self.identity),
            (10, &self.signer_digest),
            (11, &self.runner_environment),
            (12, &self.repository),
            (13, &self.commit),
            (14, &self.source_ref),
        ]
        .into_iter()
        .map(|(claim, value)| {
            let mut oid = FULCIO_ARC.to_vec();
            oid.push(claim);
            CustomExtension::from_oid_content(&oid, der_utf8_string(value))
        })
        .collect()
    }
}

/// A throwaway certificate authority and transparency log.
pub struct Authority {
    ca_params: CertificateParams,
    ca_key: KeyPair,
    ca_der: Vec<u8>,
    log: EcdsaKeyPair,
    log_spki: Vec<u8>,
    valid_from: OffsetDateTime,
}

impl Authority {
    /// # Errors
    /// Fails if a key or the authority certificate cannot be generated.
    pub fn new() -> Result<Self> {
        let valid_from = OffsetDateTime::now_utc() - Duration::hours(1);
        let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "test-sigstore");
        ca_params
            .distinguished_name
            .push(DnType::OrganizationName, "test-sigstore.invalid");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        ca_params.not_before = valid_from;
        ca_params.not_after = valid_from + Duration::days(30);
        let ca_der = ca_params.self_signed(&ca_key)?.der().to_vec();
        let log_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let log = signing_key(&log_key)?;
        Ok(Self {
            ca_params,
            ca_key,
            ca_der,
            log,
            log_spki: log_key.subject_public_key_info(),
            valid_from,
        })
    }

    /// One authority shared by every caller in this process, so a test can
    /// sign again with the authority a policy it built earlier trusts.
    ///
    /// # Errors
    /// Fails if the shared authority could not be created.
    pub fn shared() -> Result<&'static Self> {
        static SHARED: OnceLock<Result<Authority, String>> = OnceLock::new();
        SHARED
            .get_or_init(|| Self::new().map_err(|error| error.to_string()))
            .as_ref()
            .map_err(|error| error.clone().into())
    }

    /// The Sigstore trusted root naming this authority's certificate
    /// authority and log, with no certificate transparency logs or timestamp
    /// authorities.
    ///
    /// # Errors
    /// Fails if the validity start cannot be formatted.
    pub fn trusted_root(&self) -> Result<Value> {
        let start = self.valid_from.format(&Rfc3339)?;
        Ok(json!({
            "mediaType": "application/vnd.dev.sigstore.trustedroot+json;version=0.1",
            "tlogs": [{
                "baseUrl": format!("https://{ORIGIN}"),
                "hashAlgorithm": "SHA2_256",
                "publicKey": {
                    "rawBytes": BASE64.encode(&self.log_spki),
                    "keyDetails": "PKIX_ECDSA_P256_SHA_256",
                    "validFor": {"start": start},
                },
                "logId": {"keyId": BASE64.encode(self.log_id())},
            }],
            "certificateAuthorities": [{
                "subject": {"organization": "test-sigstore.invalid", "commonName": "test-sigstore"},
                "uri": "https://ca.test-sigstore.invalid",
                "certChain": {"certificates": [{"rawBytes": BASE64.encode(&self.ca_der)}]},
                "validFor": {"start": start},
            }],
            "ctlogs": [],
            "timestampAuthorities": [],
        }))
    }

    /// Sign `statement` as `workload` and return the Sigstore bundle.
    ///
    /// # Errors
    /// Fails if a key, the certificate or a signature cannot be produced.
    pub fn sign_statement(&self, workload: &Workload, statement: &[u8]) -> Result<Vec<u8>> {
        let now = OffsetDateTime::now_utc();
        let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let mut leaf = CertificateParams::new(Vec::<String>::new())?;
        leaf.subject_alt_names = vec![SanType::URI(workload.identity.as_str().try_into()?)];
        leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::CodeSigning];
        leaf.not_before = now - Duration::minutes(1);
        leaf.not_after = now + Duration::minutes(10);
        leaf.custom_extensions = workload.claims();
        let issuer = Issuer::new(self.ca_params.clone(), &self.ca_key);
        let certificate = leaf.signed_by(&leaf_key, &issuer)?;
        let signature = sign(&signing_key(&leaf_key)?, &pae(statement))?;

        let envelope = json!({
            "payload": BASE64.encode(statement),
            "payloadType": PAYLOAD_TYPE,
            "signatures": [{"sig": BASE64.encode(&signature), "keyid": ""}],
        });
        let body = serde_json_canonicalizer::to_vec(&json!({
            "apiVersion": "0.0.1",
            "kind": "dsse",
            "spec": {
                "envelopeHash": {"algorithm": "sha256", "value": hex::encode(Sha256::digest(serde_json::to_vec(&envelope)?))},
                "payloadHash": {"algorithm": "sha256", "value": hex::encode(Sha256::digest(statement))},
                "signatures": [{"signature": BASE64.encode(&signature), "verifier": BASE64.encode(certificate.pem())}],
            },
        }))?;
        // A one-entry tree: its root is the entry's leaf hash and the
        // inclusion proof has no hashes.
        let root = Sha256::digest([&[0_u8][..], &body].concat());
        let note = format!("{ORIGIN}\n1\n{}\n", BASE64.encode(root));
        let hinted = [
            &Sha256::digest(&self.log_spki)[..4],
            &sign(&self.log, note.as_bytes())?,
        ]
        .concat();
        let checkpoint = format!("{note}\n\u{2014} {ORIGIN} {}\n", BASE64.encode(hinted));
        let integrated = now.unix_timestamp();
        let entry_timestamp = sign(
            &self.log,
            &serde_json_canonicalizer::to_vec(&json!({
                "body": BASE64.encode(&body),
                "integratedTime": integrated,
                "logIndex": 0,
                "logID": hex::encode(self.log_id()),
            }))?,
        )?;

        Ok(serde_json::to_vec(&json!({
            "mediaType": "application/vnd.dev.sigstore.bundle.v0.3+json",
            "verificationMaterial": {
                "certificate": {"rawBytes": BASE64.encode(certificate.der())},
                "tlogEntries": [{
                    "logIndex": "0",
                    "logId": {"keyId": BASE64.encode(self.log_id())},
                    "kindVersion": {"kind": "dsse", "version": "0.0.1"},
                    "integratedTime": integrated.to_string(),
                    "inclusionPromise": {"signedEntryTimestamp": BASE64.encode(entry_timestamp)},
                    "inclusionProof": {
                        "logIndex": "0",
                        "rootHash": BASE64.encode(root),
                        "treeSize": "1",
                        "hashes": [],
                        "checkpoint": {"envelope": checkpoint},
                    },
                    "canonicalizedBody": BASE64.encode(&body),
                }],
                "timestampVerificationData": {},
            },
            "dsseEnvelope": envelope,
        }))?)
    }

    /// Sign the release in `directory` as `workload`: attest its
    /// `release.json` and write the bundle to `provenance.sigstore.json`.
    ///
    /// # Errors
    /// Fails if `release.json` cannot be read or the bundle cannot be
    /// produced or written.
    pub fn sign_release(&self, workload: &Workload, directory: &Path) -> Result<()> {
        let statement = release_statement(&std::fs::read(directory.join("release.json"))?)?;
        let bundle = self.sign_statement(workload, &statement)?;
        std::fs::write(directory.join("provenance.sigstore.json"), bundle)?;
        Ok(())
    }

    fn log_id(&self) -> [u8; 32] {
        Sha256::digest(&self.log_spki).into()
    }
}

/// An in-toto SLSA v1 provenance statement whose one subject is
/// `release.json` with these bytes.
///
/// # Errors
/// Fails only if the statement cannot be serialized.
pub fn release_statement(release: &[u8]) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{"name": "release.json", "digest": {"sha256": hex::encode(Sha256::digest(release))}}],
        "predicateType": "https://slsa.dev/provenance/v1",
        "predicate": {"buildDefinition": {"buildType": "https://test-sigstore.invalid/build/v1"}},
    }))?)
}

/// DSSE's pre-authentication encoding of an in-toto `payload`.
fn pae(payload: &[u8]) -> Vec<u8> {
    let mut encoded = format!(
        "DSSEv1 {} {PAYLOAD_TYPE} {} ",
        PAYLOAD_TYPE.len(),
        payload.len()
    )
    .into_bytes();
    encoded.extend_from_slice(payload);
    encoded
}

fn signing_key(key: &KeyPair) -> Result<EcdsaKeyPair> {
    EcdsaKeyPair::from_pkcs8(
        &ECDSA_P256_SHA256_ASN1_SIGNING,
        &key.serialize_der(),
        &SystemRandom::new(),
    )
    .map_err(|error| error.to_string().into())
}

fn sign(key: &EcdsaKeyPair, message: &[u8]) -> Result<Vec<u8>> {
    key.sign(&SystemRandom::new(), message)
        .map(|signature| signature.as_ref().to_vec())
        .map_err(|_| "signing failed".into())
}

/// `value` as a DER `UTF8String`, how Fulcio encodes its claims from `.1.8`.
fn der_utf8_string(value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut encoded = vec![0x0c];
    match u8::try_from(bytes.len()) {
        Ok(length) if length < 0x80 => encoded.push(length),
        Ok(length) => encoded.extend([0x81, length]),
        Err(_) => {
            let [high, low] = u16::try_from(bytes.len()).unwrap_or(u16::MAX).to_be_bytes();
            encoded.extend([0x82, high, low]);
        }
    }
    encoded.extend_from_slice(bytes);
    encoded
}
