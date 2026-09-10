use http::{Request, Response};
use http_body_util::BodyExt;
use kube::{Client, client::Body};
use platform::{PlatformClient, SCHEMA_VERSION};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    convert::Infallible,
    sync::{Arc, Mutex},
};
use tower::service_fn;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn identity(revision: &str) -> Value {
    json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"apsides-deploy-identity-example","namespace":"control","uid":"identity-uid","resourceVersion":revision},"data":{"state.json":json!({"schemaVersion":SCHEMA_VERSION,"domainId":"example","clusterBinding":"cluster-a","serviceAccountName":"deployer","serviceAccountUid":"sa-uid","bindingSecretName":"binding","bindingSecretUid":"secret-uid","audience":"https://kubernetes.default.svc","lifetimeSeconds":600}).to_string()}})
}

fn client(replies: Vec<Value>) -> TestResult<(PlatformClient, Arc<Mutex<Vec<Value>>>)> {
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let requests: Arc<Mutex<Vec<Value>>> = Arc::default();
    let captured = requests.clone();
    let service = service_fn(move |request: Request<Body>| {
        let replies = replies.clone();
        let captured = captured.clone();
        async move {
            Ok::<_, Infallible>(match reply(request, &replies, &captured).await {
                Ok(response) => response,
                Err(error) => fixture_failure(&error),
            })
        }
    });
    Ok((
        PlatformClient::new(
            Client::new(service, "control"),
            "control",
            "workload",
            "cluster-a",
        )?,
        requests,
    ))
}

/// A fixture problem inside the mock transport, which itself cannot fail.
type FixtureError = Box<dyn std::error::Error + Send + Sync>;

/// The transport signature is infallible, so a fixture problem becomes a 500
/// naming it; the assertions then report that rather than a panic.
fn fixture_failure(error: &FixtureError) -> Response<Body> {
    Response::builder()
        .status(500)
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "fixtureError": error.to_string() })
                .to_string()
                .into_bytes(),
        ))
        .unwrap_or_else(|_| Response::new(Body::from(Vec::new())))
}

async fn reply(
    request: Request<Body>,
    replies: &Mutex<VecDeque<Value>>,
    captured: &Mutex<Vec<Value>>,
) -> std::result::Result<Response<Body>, FixtureError> {
    let (parts, body) = request.into_parts();
    let bytes = body.collect().await?.to_bytes();
    captured
        .lock()
        .map_err(|_| "captured mutex is poisoned")?
        .push(json!({
            "method": parts.method.as_str(),
            "path": parts.uri.path(),
            "body": if bytes.is_empty() { Value::Null } else { serde_json::from_slice::<Value>(&bytes)? },
        }));
    let reply = replies
        .lock()
        .map_err(|_| "replies mutex is poisoned")?
        .pop_front()
        .ok_or("fixture ran out of queued replies")?;
    Ok(Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&reply)?))?)
}

fn mint_replies(after_revision: &str) -> TestResult<Vec<Value>> {
    let expires = k8s_openapi::jiff::Timestamp::from_second(
        k8s_openapi::jiff::Timestamp::now().as_second() + 600,
    )?;
    Ok(vec![
        identity("1"),
        json!({"metadata":{"uid":"sa-uid"}}),
        json!({"metadata":{"uid":"secret-uid"}}),
        json!({"status":{"token":"synthetic-test-token","expirationTimestamp":expires.to_string()}}),
        identity(after_revision),
    ])
}

#[tokio::test]
async fn mint_uses_protected_uid_bound_identity_and_short_lifetime() -> TestResult {
    let (platform, requests) = client(mint_replies("1")?)?;
    let token = platform.approved_deployment_credential("example").await?;
    assert_eq!(token.service_account_uid, "sa-uid");
    let requests = requests.lock().map_err(|_| "requests mutex is poisoned")?;
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[3]["method"], "POST");
    assert_eq!(
        requests[3]["path"],
        "/api/v1/namespaces/control/serviceaccounts/deployer/token"
    );
    assert_eq!(
        requests[3]["body"]["spec"]["audiences"],
        json!(["https://kubernetes.default.svc"])
    );
    assert_eq!(requests[3]["body"]["spec"]["expirationSeconds"], 600);
    assert_eq!(
        requests[3]["body"]["spec"]["boundObjectRef"]["uid"],
        "secret-uid"
    );
    Ok(())
}

#[tokio::test]
async fn changed_identity_policy_and_replaced_binding_block_credentials() -> TestResult {
    let (platform, _) = client(mint_replies("2")?)?;
    assert!(
        platform
            .approved_deployment_credential("example")
            .await
            .is_err()
    );
    let mut replies = mint_replies("1")?;
    replies[2]["metadata"]["uid"] = json!("replacement");
    let (platform, requests) = client(replies)?;
    assert!(
        platform
            .approved_deployment_credential("example")
            .await
            .is_err()
    );
    assert_eq!(
        requests
            .lock()
            .map_err(|_| "requests mutex is poisoned")?
            .len(),
        3
    );
    Ok(())
}
