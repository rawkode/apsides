use crate::{Result, invalid};
use k8s_openapi::api::core::v1::Secret;

#[derive(Clone, Copy)]
pub(crate) enum TlsSecretExpectation<'a> {
    RecordedUid(&'a str),
    Created(&'a Secret),
}

/// Validate only stable Secret identity and admission properties on resume. For
/// creation, also prove that admission returned the exact ephemeral TLS data.
/// # Errors
/// Rejects replacement, deletion, mutation, scope changes, or altered creation
/// content without exposing Secret data in diagnostics.
pub(crate) fn validate_tls_secret(
    actual: &Secret,
    expected_name: &str,
    expected_namespace: &str,
    expectation: TlsSecretExpectation<'_>,
) -> Result<String> {
    let uid = actual
        .metadata
        .uid
        .as_deref()
        .filter(|uid| !uid.is_empty())
        .ok_or_else(|| invalid("generation TLS Secret integrity check failed"))?;
    let valid_identity = actual.metadata.name.as_deref() == Some(expected_name)
        && actual.metadata.namespace.as_deref() == Some(expected_namespace)
        && actual.metadata.deletion_timestamp.is_none()
        && actual.immutable == Some(true)
        && actual.type_.as_deref() == Some("Opaque");
    let valid_expectation = match expectation {
        TlsSecretExpectation::RecordedUid(expected_uid) => uid == expected_uid,
        TlsSecretExpectation::Created(expected) => {
            actual.data == expected.data
                && actual.type_ == expected.type_
                && actual.immutable == expected.immutable
                && actual.metadata.name == expected.metadata.name
                && actual.metadata.namespace == expected.metadata.namespace
        }
    };
    if !valid_identity || !valid_expectation {
        return Err(invalid("generation TLS Secret integrity check failed"));
    }
    Ok(uid.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::ByteString;
    use kube::core::ObjectMeta;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn secret(uid: Option<&str>) -> Secret {
        Secret {
            metadata: ObjectMeta {
                name: Some("runtime-tls".into()),
                namespace: Some("control".into()),
                uid: uid.map(str::to_owned),
                ..Default::default()
            },
            immutable: Some(true),
            type_: Some("Opaque".into()),
            data: Some(BTreeMap::from([
                ("ca.crt".into(), ByteString(vec![1, 2, 3])),
                ("client.key".into(), ByteString(vec![4, 5, 6])),
            ])),
            ..Default::default()
        }
    }

    #[test]
    fn recorded_secret_requires_exact_uid_and_immutable_identity()
    -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let valid = secret(Some("uid-one"));
        assert_eq!(
            validate_tls_secret(
                &valid,
                "runtime-tls",
                "control",
                TlsSecretExpectation::RecordedUid("uid-one"),
            )?,
            "uid-one"
        );

        let missing_uid = secret(None);
        assert!(
            validate_tls_secret(
                &missing_uid,
                "runtime-tls",
                "control",
                TlsSecretExpectation::RecordedUid("uid-one"),
            )
            .is_err()
        );
        let replacement = secret(Some("uid-two"));
        assert!(
            validate_tls_secret(
                &replacement,
                "runtime-tls",
                "control",
                TlsSecretExpectation::RecordedUid("uid-one"),
            )
            .is_err()
        );

        let mut mutable = valid.clone();
        mutable.immutable = Some(false);
        let mut wrong_type = valid.clone();
        wrong_type.type_ = Some("kubernetes.io/tls".into());
        let mut wrong_name = valid.clone();
        wrong_name.metadata.name = Some("other".into());
        let mut wrong_namespace = valid.clone();
        wrong_namespace.metadata.namespace = Some("other".into());
        let mut deleting = valid;
        deleting.metadata.deletion_timestamp =
            Some(serde_json::from_value(json!("2026-09-06T00:00:00Z"))?);
        for changed in [mutable, wrong_type, wrong_name, wrong_namespace, deleting] {
            assert!(
                validate_tls_secret(
                    &changed,
                    "runtime-tls",
                    "control",
                    TlsSecretExpectation::RecordedUid("uid-one"),
                )
                .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn created_secret_requires_unchanged_ephemeral_content()
    -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let expected = secret(None);
        let admitted = secret(Some("uid-one"));
        validate_tls_secret(
            &admitted,
            "runtime-tls",
            "control",
            TlsSecretExpectation::Created(&expected),
        )?;

        let mut changed = admitted;
        changed
            .data
            .as_mut()
            .ok_or("fixture Secret data missing")?
            .insert("client.key".into(), ByteString(vec![9, 9, 9]));
        assert!(
            validate_tls_secret(
                &changed,
                "runtime-tls",
                "control",
                TlsSecretExpectation::Created(&expected),
            )
            .is_err()
        );
        Ok(())
    }
}
