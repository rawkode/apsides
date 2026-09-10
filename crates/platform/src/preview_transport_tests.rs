use super::*;
use bytes::Bytes;
use http::{Method, Response};
use http_body::{Body as HttpBody, Frame};
use serde_json::json;
use std::{
    collections::VecDeque,
    convert::Infallible,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};
use tower::service_fn;

type Call = (Method, String, Option<String>, Value);

struct Chunks {
    values: VecDeque<Bytes>,
    polls: Arc<AtomicUsize>,
}
impl Chunks {
    fn one(bytes: Vec<u8>) -> Self {
        Self {
            values: VecDeque::from([Bytes::from(bytes)]),
            polls: Arc::default(),
        }
    }
    fn tracked(values: Vec<Bytes>, polls: Arc<AtomicUsize>) -> Self {
        Self {
            values: values.into(),
            polls,
        }
    }
}
impl HttpBody for Chunks {
    type Data = Bytes;
    type Error = Infallible;
    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Self::Data>, Self::Error>>> {
        let body = self.get_mut();
        body.polls.fetch_add(1, Ordering::Relaxed);
        Poll::Ready(body.values.pop_front().map(|value| Ok(Frame::data(value))))
    }
}

fn target() -> Result<PreviewTarget> {
    PreviewTarget::new("v1", "ConfigMap", "", "configmaps", "workload", "settings")
}
fn object(resource_version: &str) -> Value {
    json!({
        "apiVersion":"v1", "kind":"ConfigMap",
        "metadata":{"name":"settings","namespace":"workload","uid":"uid-1",
            "resourceVersion":resource_version},
        "data":{"private":"response-canary"}
    })
}
fn list_item(resource_version: &str) -> Value {
    json!({
        "metadata":{"name":"settings","namespace":"workload","uid":"uid-1",
            "resourceVersion":resource_version},
        "data":{"private":"response-canary"}
    })
}
fn desired(name: &str) -> Value {
    json!({
        "apiVersion":"v1", "kind":"ConfigMap",
        "metadata":{"name":name,"namespace":"workload"},
        "data":{"private":"request-canary"}
    })
}

async fn list_response(value: Value) -> Result<NamedPreview> {
    let response =
        serde_json::to_vec(&value).map_err(|_| invalid("preview test response is invalid"))?;
    let service = service_fn(move |_: http::Request<Body>| {
        let response = response.clone();
        async move { Ok::<_, Infallible>(Response::new(Chunks::one(response))) }
    });
    PreviewTransport::new(Client::new(service, "workload"))
        .list_named(&target()?)
        .await
}

fn assert_fixed_calls(calls: &[Call]) -> Result<()> {
    assert_eq!(calls.len(), 2);
    let list = calls
        .first()
        .ok_or_else(|| invalid("preview list call missing"))?;
    assert_eq!(list.0, Method::GET);
    assert!(list.1.contains("fieldSelector=metadata.name%3Dsettings"));
    assert!(list.1.contains("limit=2"));
    let patch = calls
        .get(1)
        .ok_or_else(|| invalid("preview patch call missing"))?;
    assert_eq!(patch.0, Method::PATCH);
    assert!(patch.1.contains("dryRun=All"));
    assert!(patch.1.contains("force=true"));
    assert!(patch.1.contains("fieldManager=apsides-example"));
    assert!(patch.1.contains("fieldValidation=Strict"));
    assert_eq!(patch.2.as_deref(), Some("application/apply-patch+yaml"));
    assert_eq!(
        patch.3.pointer("/metadata/uid").and_then(Value::as_str),
        Some("uid-1")
    );
    assert!(
        calls
            .iter()
            .all(|call| matches!(call.0, Method::GET | Method::PATCH))
    );
    Ok(())
}

#[tokio::test]
async fn fixed_list_and_patch_are_exact_named_strict_dry_run_requests() -> Result<()> {
    let calls: Arc<Mutex<Vec<Call>>> = Arc::default();
    let service = service_fn({
        let calls = calls.clone();
        move |request: http::Request<Body>| {
            let calls = calls.clone();
            async move {
                let (parts, body) = request.into_parts();
                let body = body
                    .collect()
                    .await
                    .map_err(std::io::Error::other)?
                    .to_bytes();
                let value = if body.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&body).map_err(std::io::Error::other)?
                };
                let content_type = parts
                    .headers
                    .get(http::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                calls
                    .lock()
                    .map_err(|_| std::io::Error::other("call lock poisoned"))?
                    .push((
                        parts.method.clone(),
                        parts.uri.to_string(),
                        content_type,
                        value,
                    ));
                let response = if parts.method == Method::GET {
                    json!({"apiVersion":"v1","kind":"ConfigMapList",
                            "metadata":{"resourceVersion":"41","continue":""},
                            "items":[list_item("40")]})
                } else {
                    object("42")
                };
                Response::builder()
                    .header("content-type", "application/json")
                    .body(Chunks::one(
                        serde_json::to_vec(&response).map_err(std::io::Error::other)?,
                    ))
                    .map_err(std::io::Error::other)
            }
        }
    });
    let transport = PreviewTransport::new(Client::new(service, "workload"));
    let target = target()?;
    let listed = transport.list_named(&target).await?;
    assert_eq!(listed.resource_version(), "41");
    assert_eq!(
        listed
            .object()
            .and_then(|item| item.metadata.uid.as_deref()),
        Some("uid-1")
    );
    assert_eq!(
        listed
            .object()
            .and_then(|item| item.metadata.resource_version.as_deref()),
        Some("40")
    );
    assert!(
        listed
            .object()
            .and_then(|item| item.types.as_ref())
            .is_some_and(|types| types.api_version == "v1" && types.kind == "ConfigMap")
    );
    let apply = crate::conditional_apply_request(
        &desired("settings"),
        "settings",
        "workload",
        "apsides-example",
        true,
        crate::ApplyPrecondition::Existing {
            uid: "uid-1",
            resource_version: "40",
        },
    )?;
    let projected = transport.dry_run_apply(&target, &apply).await?;
    assert_eq!(
        projected.object().metadata.resource_version.as_deref(),
        Some("42")
    );
    let calls = calls
        .lock()
        .map_err(|_| invalid("preview test call lock poisoned"))?;
    assert_fixed_calls(&calls)
}

#[tokio::test]
async fn unsupported_or_mismatched_targets_fail_before_transport() -> Result<()> {
    for invalid_target in [
        PreviewTarget::new("v1", "Secret", "", "secrets", "workload", "settings"),
        PreviewTarget::new(
            "v1",
            "ConfigMap",
            "apps",
            "configmaps",
            "workload",
            "settings",
        ),
        PreviewTarget::new(
            "apps/v1",
            "Deployment",
            "apps",
            "pods",
            "workload",
            "settings",
        ),
    ] {
        assert!(invalid_target.is_err());
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let response = serde_json::to_vec(&object("42"))
        .map_err(|_| invalid("preview test response is invalid"))?;
    let service = service_fn({
        let calls = calls.clone();
        let response = response.clone();
        move |_: http::Request<Body>| {
            calls.fetch_add(1, Ordering::Relaxed);
            let response = response.clone();
            async move { Ok::<_, Infallible>(Response::new(Chunks::one(response))) }
        }
    });
    let transport = PreviewTransport::new(Client::new(service, "workload"));
    let apply = crate::conditional_apply_request(
        &desired("other"),
        "other",
        "workload",
        "apsides-example",
        false,
        crate::ApplyPrecondition::Absent {
            list_resource_version: "41",
        },
    )?;
    assert!(transport.dry_run_apply(&target()?, &apply).await.is_err());
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    Ok(())
}

#[test]
fn uid_and_resource_version_scalars_are_bounded_ascii_graphic() -> Result<()> {
    assert!(valid_uid(&"u".repeat(MAX_UID_BYTES)));
    assert!(!valid_uid(&"u".repeat(MAX_UID_BYTES + 1)));
    assert!(!valid_uid("uid with space"));
    assert!(!valid_uid("uid-é"));
    assert!(valid_version(&"v".repeat(MAX_RESOURCE_VERSION_BYTES)));
    assert!(!valid_version(&"v".repeat(MAX_RESOURCE_VERSION_BYTES + 1)));
    assert!(!valid_version("version with space"));
    assert!(!valid_version("0"));

    let invalid_uid = crate::conditional_apply_request(
        &desired("settings"),
        "settings",
        "workload",
        "apsides-example",
        false,
        crate::ApplyPrecondition::Existing {
            uid: "uid with space",
            resource_version: "40",
        },
    )?;
    assert!(validate_apply(&target()?, &invalid_uid).is_err());
    Ok(())
}

#[tokio::test]
async fn existing_patch_requires_exact_bounded_response_uid() -> Result<()> {
    let mut response = object("42");
    let metadata = response
        .get_mut("metadata")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| invalid("preview test metadata is invalid"))?;
    metadata.remove("uid");
    let response =
        serde_json::to_vec(&response).map_err(|_| invalid("preview test response is invalid"))?;
    let service = service_fn(move |_: http::Request<Body>| {
        let response = response.clone();
        async move { Ok::<_, Infallible>(Response::new(Chunks::one(response))) }
    });
    let transport = PreviewTransport::new(Client::new(service, "workload"));
    let apply = crate::conditional_apply_request(
        &desired("settings"),
        "settings",
        "workload",
        "apsides-example",
        false,
        crate::ApplyPrecondition::Existing {
            uid: "uid-1",
            resource_version: "40",
        },
    )?;
    assert!(transport.dry_run_apply(&target()?, &apply).await.is_err());
    Ok(())
}

#[tokio::test]
async fn list_rejects_continuation_cardinality_and_object_identity_drift() -> Result<()> {
    let invalid_responses = [
        json!({"apiVersion":"v1","kind":"ConfigMapList",
            "metadata":{"resourceVersion":"41","continue":"next"},"items":[]}),
        json!({"apiVersion":"v1","kind":"ConfigMapList",
            "metadata":{"resourceVersion":"41","remainingItemCount":1},"items":[]}),
        json!({"apiVersion":"v1","kind":"ConfigMapList",
            "metadata":{"resourceVersion":"41"},"items":[object("40"),object("40")]}),
        json!({"apiVersion":"v1","kind":"ConfigMapList",
            "metadata":{"resourceVersion":"41"},"items":[{
                "apiVersion":"v1","kind":"ConfigMap",
                "metadata":{"name":"other","namespace":"workload","uid":"uid-1",
                    "resourceVersion":"40"}}]}),
        json!({"apiVersion":"v1","kind":"ConfigMapList",
            "metadata":{"resourceVersion":"41"},"items":[{
                "apiVersion":"v1","kind":"Service",
                "metadata":{"name":"settings","namespace":"workload","uid":"uid-1",
                    "resourceVersion":"40"}}]}),
        json!({"apiVersion":"v1","kind":"ConfigMapList",
            "metadata":{"resourceVersion":"41"},"items":[{
                "apiVersion":"v1",
                "metadata":{"name":"settings","namespace":"workload","uid":"uid-1",
                    "resourceVersion":"40"}}]}),
        json!({"apiVersion":"v1","kind":"ConfigMapList",
            "metadata":{"resourceVersion":"41"},"items":[{
                "kind":"ConfigMap",
                "metadata":{"name":"settings","namespace":"workload","uid":"uid-1",
                    "resourceVersion":"40"}}]}),
        json!({"apiVersion":"v1","kind":"ConfigMapList",
            "metadata":{"resourceVersion":"41"},"items":[{
                "apiVersion":"v1","kind":"ConfigMap",
                "metadata":{"name":"settings","namespace":"workload",
                    "resourceVersion":"40"}}]}),
        json!({"apiVersion":"v1","kind":"ConfigMapList",
            "metadata":{"resourceVersion":"0"},"items":[]}),
    ];
    for response in invalid_responses {
        assert!(list_response(response).await.is_err());
    }
    Ok(())
}

#[tokio::test]
async fn oversized_chunked_success_is_bounded_before_decode() -> Result<()> {
    let polls = Arc::new(AtomicUsize::new(0));
    let service = service_fn({
        let polls = polls.clone();
        move |_: http::Request<Body>| {
            let chunks = vec![
                Bytes::from(vec![b' '; MAX_PREVIEW_RESPONSE_BYTES / 2]),
                Bytes::from(vec![b' '; MAX_PREVIEW_RESPONSE_BYTES / 2]),
                Bytes::from_static(b"x"),
            ];
            let body = Chunks::tracked(chunks, polls.clone());
            async move { Ok::<_, Infallible>(Response::new(body)) }
        }
    });
    let transport = PreviewTransport::new(Client::new(service, "workload"));
    assert!(transport.list_named(&target()?).await.is_err());
    assert!(polls.load(Ordering::Relaxed) >= 3);
    Ok(())
}

#[tokio::test]
async fn malformed_success_and_non_success_bodies_are_private() -> Result<()> {
    let error_polls = Arc::new(AtomicUsize::new(0));
    let oversized_polls = Arc::new(AtomicUsize::new(0));
    let replies = Arc::new(Mutex::new(VecDeque::from([
        (
            200,
            Chunks::one(br#"{"apiVersion":"v1","apiVersion":"private-canary"}"#.to_vec()),
        ),
        (
            403,
            Chunks::tracked(
                vec![Bytes::from_static(b"private-error-canary")],
                error_polls.clone(),
            ),
        ),
        (
            200,
            Chunks::tracked(
                vec![Bytes::from_static(b"private-oversized-canary")],
                oversized_polls.clone(),
            ),
        ),
    ])));
    let service = service_fn({
        let oversized_polls = oversized_polls.clone();
        move |_: http::Request<Body>| {
            let replies = replies.clone();
            let oversized_polls = oversized_polls.clone();
            async move {
                let (status, body) = replies
                    .lock()
                    .map_err(|_| std::io::Error::other("reply lock poisoned"))?
                    .pop_front()
                    .ok_or_else(|| std::io::Error::other("preview reply missing"))?;
                let mut response = Response::builder().status(status);
                if status == 200 && Arc::ptr_eq(&body.polls, &oversized_polls) {
                    response = response.header(CONTENT_LENGTH, MAX_PREVIEW_RESPONSE_BYTES + 1);
                }
                response.body(body).map_err(std::io::Error::other)
            }
        }
    });
    let transport = PreviewTransport::new(Client::new(service, "workload"));
    let malformed = transport.list_named(&target()?).await;
    assert!(malformed.is_err());
    let rejected = transport.list_named(&target()?).await;
    let text = rejected
        .err()
        .ok_or_else(|| invalid("preview non-success response was accepted"))?
        .to_string();
    assert!(!text.contains("private-error-canary"));
    assert_eq!(error_polls.load(Ordering::Relaxed), 0);
    let oversized = transport.list_named(&target()?).await;
    assert!(oversized.is_err());
    assert_eq!(oversized_polls.load(Ordering::Relaxed), 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn transport_deadline_invokes_supplied_service_once() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let service = service_fn({
        let calls = calls.clone();
        move |_: http::Request<Body>| {
            calls.fetch_add(1, Ordering::Relaxed);
            async {
                tokio::time::sleep(PREVIEW_DEADLINE + Duration::from_secs(1)).await;
                Ok::<_, Infallible>(Response::new(Chunks::one(Vec::new())))
            }
        }
    });
    let transport = PreviewTransport::new(Client::new(service, "workload"));
    let error = transport
        .list_named(&target()?)
        .await
        .err()
        .ok_or_else(|| invalid("preview timeout was accepted"))?;
    assert!(error.to_string().contains("timed out"));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    Ok(())
}
