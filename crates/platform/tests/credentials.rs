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

fn identity(revision: &str) -> Value {
    json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"reakt-deploy-identity-example","namespace":"control","uid":"identity-uid","resourceVersion":revision},"data":{"state.json":json!({"schemaVersion":SCHEMA_VERSION,"domainId":"example","clusterBinding":"cluster-a","serviceAccountName":"deployer","serviceAccountUid":"sa-uid","bindingSecretName":"binding","bindingSecretUid":"secret-uid","audience":"https://kubernetes.default.svc","lifetimeSeconds":600}).to_string()}})
}

fn client(replies: Vec<Value>) -> (PlatformClient, Arc<Mutex<Vec<Value>>>) {
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let requests: Arc<Mutex<Vec<Value>>> = Arc::default();
    let captured = requests.clone();
    let service = service_fn(move |request: Request<Body>| {
        let replies = replies.clone();
        let captured = captured.clone();
        async move {
            let (parts, body) = request.into_parts();
            let bytes = body.collect().await.unwrap().to_bytes();
            captured.lock().unwrap().push(json!({"method":parts.method.as_str(),"path":parts.uri.path(),"body":if bytes.is_empty() {Value::Null} else {serde_json::from_slice::<Value>(&bytes).unwrap()}}));
            let reply = replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected request");
            Ok::<_, Infallible>(
                Response::builder()
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&reply).unwrap()))
                    .unwrap(),
            )
        }
    });
    (
        PlatformClient::new(
            Client::new(service, "control"),
            "control",
            "workload",
            "cluster-a",
        )
        .unwrap(),
        requests,
    )
}

fn mint_replies(after_revision: &str) -> Vec<Value> {
    let expires = k8s_openapi::jiff::Timestamp::from_second(
        k8s_openapi::jiff::Timestamp::now().as_second() + 600,
    )
    .unwrap();
    vec![
        identity("1"),
        json!({"metadata":{"uid":"sa-uid"}}),
        json!({"metadata":{"uid":"secret-uid"}}),
        json!({"status":{"token":"synthetic-test-token","expirationTimestamp":expires.to_string()}}),
        identity(after_revision),
    ]
}

#[tokio::test]
async fn mint_uses_protected_uid_bound_identity_and_short_lifetime() {
    let (platform, requests) = client(mint_replies("1"));
    let token = platform
        .approved_deployment_credential("example")
        .await
        .unwrap();
    assert_eq!(token.service_account_uid, "sa-uid");
    let requests = requests.lock().unwrap();
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
}

#[tokio::test]
async fn changed_identity_policy_and_replaced_binding_block_credentials() {
    let (platform, _) = client(mint_replies("2"));
    assert!(
        platform
            .approved_deployment_credential("example")
            .await
            .is_err()
    );
    let mut replies = mint_replies("1");
    replies[2]["metadata"]["uid"] = json!("replacement");
    let (platform, requests) = client(replies);
    assert!(
        platform
            .approved_deployment_credential("example")
            .await
            .is_err()
    );
    assert_eq!(requests.lock().unwrap().len(), 3);
}
