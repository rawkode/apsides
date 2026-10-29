//! The `ring` backend for Sigstore verification.
//!
//! `sigstore-crypto` ships only an aws-lc backend. The workspace's TLS and
//! certificate code uses ring, so verification installs this provider rather
//! than adding a second cryptographic library. It verifies; it never signs.
//! ML-DSA and prehashed verification are refused: neither Fulcio, GitHub's
//! instance nor DSSE envelopes need them.

use ring::{
    digest::{self, Context, SHA256, SHA384, SHA512},
    signature::{
        ECDSA_P256_SHA256_ASN1, ECDSA_P256_SHA384_ASN1, ECDSA_P384_SHA256_ASN1,
        ECDSA_P384_SHA384_ASN1, ED25519, RSA_PKCS1_2048_8192_SHA256, RSA_PKCS1_2048_8192_SHA384,
        RSA_PKCS1_2048_8192_SHA512, RSA_PSS_2048_8192_SHA256, RSA_PSS_2048_8192_SHA384,
        RSA_PSS_2048_8192_SHA512, UnparsedPublicKey, VerificationAlgorithm,
    },
};
use sigstore_verify::{
    crypto::{
        Error, SigningScheme,
        provider::{CryptoProvider, HashContext, install_default_provider},
    },
    types::HashAlgorithm,
};
use spki::{SubjectPublicKeyInfoRef, der::Decode as _};

static RING: Ring = Ring;

/// Make ring the provider every Sigstore verification in this process uses.
///
/// # Errors
/// Rejects when another provider was installed first.
pub(crate) fn install() -> crate::Result<()> {
    match install_default_provider(&RING) {
        Ok(()) => Ok(()),
        Err(installed) if std::ptr::addr_eq(installed, &raw const RING) => Ok(()),
        Err(_) => crate::reject("another Sigstore cryptographic provider is installed"),
    }
}

#[derive(Debug)]
struct Ring;

struct RingHash(Context);

impl HashContext for RingHash {
    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    fn finalize(self: Box<Self>) -> Vec<u8> {
        self.0.finish().as_ref().to_vec()
    }
}

fn algorithm(scheme: SigningScheme) -> Result<&'static dyn VerificationAlgorithm, Error> {
    Ok(match scheme {
        SigningScheme::EcdsaP256Sha256 => &ECDSA_P256_SHA256_ASN1,
        SigningScheme::EcdsaP256Sha384 => &ECDSA_P256_SHA384_ASN1,
        SigningScheme::EcdsaP384Sha256 => &ECDSA_P384_SHA256_ASN1,
        SigningScheme::EcdsaP384Sha384 => &ECDSA_P384_SHA384_ASN1,
        SigningScheme::Ed25519 => &ED25519,
        SigningScheme::RsaPssSha256 => &RSA_PSS_2048_8192_SHA256,
        SigningScheme::RsaPssSha384 => &RSA_PSS_2048_8192_SHA384,
        SigningScheme::RsaPssSha512 => &RSA_PSS_2048_8192_SHA512,
        SigningScheme::RsaPkcs1Sha256 => &RSA_PKCS1_2048_8192_SHA256,
        SigningScheme::RsaPkcs1Sha384 => &RSA_PKCS1_2048_8192_SHA384,
        SigningScheme::RsaPkcs1Sha512 => &RSA_PKCS1_2048_8192_SHA512,
        other => {
            return Err(Error::UnsupportedAlgorithm(format!(
                "{} verification is not supported",
                other.name()
            )));
        }
    })
}

/// ring takes only the bare key: the uncompressed point for ECDSA, the 32
/// bytes for Ed25519 and the PKCS#1 `RSAPublicKey` for RSA. Sigstore passes
/// a `SubjectPublicKeyInfo`, whose bit string holds that key, except from
/// certificate chain verification, which passes the bare key itself. Neither
/// bare form parses as a `SubjectPublicKeyInfo`.
fn bare_key(key: &[u8]) -> &[u8] {
    SubjectPublicKeyInfoRef::from_der(key)
        .ok()
        .and_then(|info| info.subject_public_key.as_bytes())
        .unwrap_or(key)
}

impl CryptoProvider for Ring {
    fn hash(&self, algorithm: HashAlgorithm) -> Result<Box<dyn HashContext>, Error> {
        let backend = match algorithm {
            HashAlgorithm::Sha2256 => &SHA256,
            HashAlgorithm::Sha2384 => &SHA384,
            HashAlgorithm::Sha2512 => &SHA512,
            other => {
                return Err(Error::UnsupportedAlgorithm(format!(
                    "cannot compute {other} digests"
                )));
            }
        };
        Ok(Box::new(RingHash(Context::new(backend))))
    }

    fn sha256(&self, data: &[u8]) -> [u8; 32] {
        let mut out = [0; 32];
        out.copy_from_slice(digest::digest(&SHA256, data).as_ref());
        out
    }

    fn sha384(&self, data: &[u8]) -> [u8; 48] {
        let mut out = [0; 48];
        out.copy_from_slice(digest::digest(&SHA384, data).as_ref());
        out
    }

    fn sha512(&self, data: &[u8]) -> [u8; 64] {
        let mut out = [0; 64];
        out.copy_from_slice(digest::digest(&SHA512, data).as_ref());
        out
    }

    fn verify_raw(
        &self,
        scheme: SigningScheme,
        spki_pubkey_bytes: &[u8],
        data: &[u8],
        signature: &[u8],
    ) -> Result<(), Error> {
        UnparsedPublicKey::new(algorithm(scheme)?, bare_key(spki_pubkey_bytes))
            .verify(data, signature)
            .map_err(|_| Error::Verification(format!("{} signature invalid", scheme.name())))
    }

    fn verify_prehashed(
        &self,
        scheme: SigningScheme,
        _spki_pubkey_bytes: &[u8],
        _digest: &[u8],
        _signature: &[u8],
    ) -> Result<(), Error> {
        Err(Error::UnsupportedAlgorithm(format!(
            "prehashed {} verification is not supported",
            scheme.name()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::{
        rand::SystemRandom,
        signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _},
    };

    #[test]
    fn verifies_ecdsa_from_a_subject_public_key_info() -> crate::Result<()> {
        let random = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &random)
            .map_err(|_| crate::Error::Rejected("key generation failed".into()))?;
        let key =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &random)
                .map_err(|_| crate::Error::Rejected("key parsing failed".into()))?;
        let signature = key
            .sign(&random, b"payload")
            .map_err(|_| crate::Error::Rejected("signing failed".into()))?;
        // The P-256 SPKI prefix, then the uncompressed point ring returns.
        let mut spki = hex_bytes("3059301306072a8648ce3d020106082a8648ce3d030107034200");
        spki.extend_from_slice(key.public_key().as_ref());
        let scheme = SigningScheme::EcdsaP256Sha256;
        assert!(
            RING.verify_raw(scheme, &spki, b"payload", signature.as_ref())
                .is_ok()
        );
        assert!(
            RING.verify_raw(scheme, &spki, b"altered", signature.as_ref())
                .is_err()
        );
        assert!(
            RING.verify_raw(scheme, b"not a key", b"payload", signature.as_ref())
                .is_err()
        );
        assert!(
            RING.verify_prehashed(scheme, &spki, &[0; 32], signature.as_ref())
                .is_err()
        );
        Ok(())
    }

    fn hex_bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .filter_map(|index| u8::from_str_radix(&hex[index..index + 2], 16).ok())
            .collect()
    }
}
