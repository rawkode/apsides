#![cfg(feature = "test-support")]
//! Explicitly invoked by the disposable lifecycle runner, never a release bypass.
use serde_json::json;

mod driver {
    use platform as platform_api;
    include!("support/candidate_driver.rs");
}
type Result<T> = driver::Result<T>;

#[tokio::test]
#[ignore = "requires the explicitly created disposable lifecycle cluster and signed candidate"]
async fn candidate_transition_step() -> Result<()> {
    let outcome = Box::pin(driver::transition_step(|platform, settings| {
        if settings.get("pauseAfterPhase").is_some() {
            return Err("ordinary lifecycle driver does not support checkpoints".into());
        }
        Ok(platform)
    }))
    .await;
    if let Err(error) = &outcome {
        let pending = matches!(
            error.downcast_ref::<platform::Error>(),
            Some(platform::Error::Pending(_))
        );
        println!(
            "APSIDES_TEST_ERROR={}",
            json!({"pending":pending,"kind":error_kind(error.as_ref()),"message":error.to_string()})
        );
    }
    outcome
}

fn error_kind(error: &(dyn std::error::Error + 'static)) -> &'static str {
    let ceiling = matches!(
        error.downcast_ref::<compiler_core::Error>(),
        Some(compiler_core::Error::Ceiling(_))
    ) || matches!(
        error.downcast_ref::<deploy::Error>(),
        Some(deploy::Error::Core(compiler_core::Error::Ceiling(_)))
    ) || matches!(
        error.downcast_ref::<platform::Error>(),
        Some(
            platform::Error::Core(compiler_core::Error::Ceiling(_))
                | platform::Error::Deploy(deploy::Error::Core(compiler_core::Error::Ceiling(_)))
        )
    );
    if ceiling {
        "permission-ceiling"
    } else if matches!(error.downcast_ref::<kube::Error>(), Some(kube::Error::Api(status)) if status.code == 403)
        || matches!(error.downcast_ref::<platform::Error>(), Some(platform::Error::Kubernetes(kube::Error::Api(status))) if status.code == 403)
    {
        "kubernetes-forbidden"
    } else {
        "other"
    }
}

#[test]
fn ceiling_error_kind_requires_a_typed_ceiling_variant() {
    let ceiling = || compiler_core::Error::Ceiling("typed fixture".into());
    let positives: Vec<Box<dyn std::error::Error + Send + Sync>> = vec![
        Box::new(ceiling()),
        Box::new(deploy::Error::Core(ceiling())),
        Box::new(platform::Error::Core(ceiling())),
        Box::new(platform::Error::Deploy(deploy::Error::Core(ceiling()))),
    ];
    for error in positives {
        assert_eq!(error_kind(error.as_ref()), "permission-ceiling");
    }
    let invalid = || compiler_core::Error::Invalid("permission ceiling exceeded".into());
    let unrelated: Vec<Box<dyn std::error::Error + Send + Sync>> = vec![
        Box::new(invalid()),
        Box::new(deploy::Error::Core(invalid())),
        Box::new(platform::Error::Core(invalid())),
        Box::new(platform::Error::Deploy(deploy::Error::Core(invalid()))),
        Box::new(deploy::Error::Plan("permission-ceiling".into())),
        Box::new(platform::Error::Pending(
            "permission ceiling exceeded".into(),
        )),
        Box::new(platform::Error::CapacityResizeRequired),
        Box::new(std::io::Error::other("permission-ceiling")),
    ];
    for error in unrelated {
        assert_eq!(error_kind(error.as_ref()), "other");
    }
}

#[test]
fn kubernetes_forbidden_kind_requires_a_typed_api_403() {
    let api_error = |code| {
        kube::Error::Api(Box::new(kube::core::Status {
            code,
            message: "forbidden 403 permission-ceiling".into(),
            reason: "Forbidden".into(),
            ..kube::core::Status::default()
        }))
    };
    for code in [0, 401, 403, 404, 409, 500] {
        let direct = api_error(code);
        let nested = platform::Error::Kubernetes(api_error(code));
        let expected = if code == 403 {
            "kubernetes-forbidden"
        } else {
            "other"
        };
        assert_eq!(error_kind(&direct), expected);
        assert_eq!(error_kind(&nested), expected);
    }
    for error in [
        platform::Error::Invalid("ApiError: Forbidden 403".into()),
        platform::Error::Pending("kubernetes-forbidden".into()),
    ] {
        assert_eq!(error_kind(&error), "other");
    }
    assert_eq!(
        error_kind(&std::io::Error::other("ApiError: Forbidden 403")),
        "other"
    );
}

#[test]
#[ignore = "metadata for explicit disposable-cluster bootstrap"]
fn proxy_profile() -> Result<()> {
    println!(
        "APSIDES_TEST_PROFILE={}",
        json!({"envoyImage":platform::ENVOY_IMAGE,"adminImage":platform::ADMIN_IMAGE,"configurationTemplateDigest":compiler_core::digest(&platform::envoy_configuration("qualified-template")?)?,"maxLedgerBytes":platform::MAX_PROXY_LEDGER_BYTES})
    );
    Ok(())
}
