use hyper::{Body, Request, Response, body::HttpBody, service::service_fn};
use libsql::Builder;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::{net::TcpListener, sync::oneshot, task::JoinSet, time::timeout};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{ClientConfig, RootCertStore, ServerConfig, pki_types::PrivatePkcs8KeyDer},
};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const DEADLINE: Duration = Duration::from_secs(5);
const SYNTHETIC_AUTH: &str = "Bearer synthetic-transport-test-token";

fn tls_configs() -> TestResult<(ClientConfig, ServerConfig)> {
    let generated = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()])?;
    let certificate = generated.cert.der().clone();
    let mut roots = RootCertStore::empty();
    roots.add(certificate.clone())?;
    let client = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate],
            PrivatePkcs8KeyDer::from(generated.signing_key.serialize_der()).into(),
        )?;
    Ok((client, server))
}

async fn redirect_response(
    mut request: Request<Body>,
    destination: String,
    requests: Arc<AtomicUsize>,
) -> TestResult<Response<Body>> {
    requests.fetch_add(1, Ordering::SeqCst);
    if request.method() != hyper::Method::POST
        || request.uri().path() != "/v3/pipeline"
        || request.headers().get(hyper::header::AUTHORIZATION)
            != Some(&hyper::header::HeaderValue::from_static(SYNTHETIC_AUTH))
    {
        return Err("TLS fixture received an unexpected request".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = request.body_mut().data().await {
        let chunk = chunk?;
        if chunk.len() > 8192 - bytes.len() {
            return Err("TLS fixture request exceeded its bound".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if value["requests"][0]["type"] != "batch"
        || value["requests"][0]["batch"]["steps"][0]["stmt"]["sql"] != "BEGIN IMMEDIATE;"
        || value["requests"][1]["type"] != "get_autocommit"
        || value["requests"].as_array().map(Vec::len) != Some(2)
    {
        return Err("TLS fixture did not receive the expected transaction start".into());
    }
    let response = serde_json::json!({
        "baton": "synthetic-test-baton",
        "base_url": destination,
        "results": [
            {"type": "ok", "response": {"type": "batch", "result": {
                "step_results": [{"cols": [], "rows": [], "affected_row_count": 0,
                                  "last_insert_rowid": null}],
                "step_errors": []
            }}},
            {"type": "ok", "response": {"type": "get_autocommit", "is_autocommit": false}}
        ]
    });
    Ok(Response::builder()
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&response)?))?)
}

fn read_only_cursor(columns: &serde_json::Value, rows: &serde_json::Value) -> TestResult<Vec<u8>> {
    let entries = [
        serde_json::json!({"baton": null, "base_url": null}),
        serde_json::json!({"type": "step_begin", "step": 0, "cols": columns}),
        serde_json::json!({"type": "step_end", "affected_row_count": 0,
                               "last_insert_rowid": null}),
    ];
    let mut encoded = Vec::new();
    serde_json::to_writer(&mut encoded, &entries[0])?;
    encoded.push(b'\n');
    serde_json::to_writer(&mut encoded, &entries[1])?;
    encoded.push(b'\n');
    for row in rows
        .as_array()
        .ok_or("read-only fixture rows were not an array")?
    {
        serde_json::to_writer(
            &mut encoded,
            &serde_json::json!({"type": "row", "row": row}),
        )?;
        encoded.push(b'\n');
    }
    serde_json::to_writer(&mut encoded, &entries[2])?;
    encoded.push(b'\n');
    Ok(encoded)
}

async fn read_only_response(
    mut request: Request<Body>,
    statements: Arc<Mutex<Vec<String>>>,
) -> TestResult<Response<Body>> {
    if request.method() != hyper::Method::POST
        || !matches!(request.uri().path(), "/v3/pipeline" | "/v3/cursor")
        || request.headers().get(hyper::header::AUTHORIZATION)
            != Some(&hyper::header::HeaderValue::from_static(SYNTHETIC_AUTH))
    {
        return Err("read-only fixture received an unexpected request".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = request.body_mut().data().await {
        let chunk = chunk?;
        if chunk.len() > 8192 - bytes.len() {
            return Err("read-only fixture request exceeded its bound".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let sql = if request.uri().path() == "/v3/pipeline" {
        if value["requests"].as_array().map(Vec::len) != Some(1)
            || value["requests"][0]["type"] != "describe"
        {
            return Err("read-only fixture received an unexpected pipeline".into());
        }
        value["requests"][0]["sql"].as_str()
    } else {
        value["batch"]["steps"][0]["stmt"]["sql"].as_str()
    };
    let Some(sql) = sql else {
        return Err("read-only fixture request omitted SQL".into());
    };
    let statement = if let Some(without_semicolon) = sql.strip_suffix(';') {
        without_semicolon
    } else {
        sql
    };
    let (columns, rows) = match statement {
        "SELECT version FROM reakt_inventory_schema WHERE id=1"
        | "SELECT version FROM reakt_inventory_schema WHERE id = 1" => (
            serde_json::json!([{"name": "version", "decltype": "INTEGER"}]),
            serde_json::json!([[{"type": "integer", "value": "1"}]]),
        ),
        "SELECT scope,digest,metadata FROM reakt_inventory_snapshots LIMIT 0"
        | "SELECT scope, digest, metadata FROM reakt_inventory_snapshots LIMIT 0"
        | "SELECT scope,revision,digest FROM reakt_inventory_heads LIMIT 0"
        | "SELECT scope, revision, digest FROM reakt_inventory_heads LIMIT 0"
        | "SELECT revision,digest FROM reakt_inventory_heads WHERE scope=?1"
        | "SELECT revision, digest FROM reakt_inventory_heads WHERE scope = ?1" => {
            (serde_json::json!([]), serde_json::json!([]))
        }
        _ => {
            return Err("read-only fixture observed a non-allowlisted statement".into());
        }
    };
    statements
        .lock()
        .map_err(|_| "read-only fixture statement lock was poisoned")?
        .push(sql.to_owned());
    let response = if request.uri().path() == "/v3/pipeline" {
        serde_json::to_vec(&serde_json::json!({
            "baton": null,
            "results": [{"type": "ok", "response": {"type": "describe", "result": {
                "params": if statement.contains("?1") {
                    serde_json::json!([{"name": "?1"}])
                } else {
                    serde_json::json!([])
                },
                "cols": [],
                "is_explain": false,
                "is_readonly": true
            }}}]
        }))?
    } else {
        read_only_cursor(&columns, &rows)?
    };
    Ok(Response::builder()
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Body::from(response))?)
}

async fn exercise_redirect(guarded: bool) -> TestResult {
    let tls_listener = TcpListener::bind("127.0.0.1:0").await?;
    let http_listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("https://{}", tls_listener.local_addr()?);
    let destination = format!("http://{}", http_listener.local_addr()?);
    let (client_config, server_config) = tls_configs()?;
    let requests = Arc::new(AtomicUsize::new(0));
    let observed_requests = requests.clone();
    let mut tasks = JoinSet::new();
    tasks.spawn(async move {
        let (socket, _) = tls_listener.accept().await?;
        let stream = TlsAcceptor::from(Arc::new(server_config))
            .accept(socket)
            .await?;
        hyper::server::conn::Http::new()
            .http1_only(true)
            .serve_connection(
                stream,
                service_fn(move |request| {
                    redirect_response(request, destination.clone(), observed_requests.clone())
                }),
            )
            .await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (accepted, mut observed_accept) = oneshot::channel();
    tasks.spawn(async move {
        let (socket, _) = http_listener.accept().await?;
        let _ = accepted.send(());
        drop(socket);
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(client_config)
        .https_only()
        .enable_http1()
        .build();
    let builder = Builder::new_remote(endpoint, "synthetic-transport-test-token".to_owned());
    let database = if guarded {
        builder
            .connector(super::guarded_connector(connector))
            .build()
            .await?
    } else {
        // Negative control: this pinned connector accepts HTTP despite https_only().
        builder.connector(connector).build().await?
    };
    let connection = database.connect()?;
    require_begin(&connection, &mut tasks).await?;
    if timeout(DEADLINE, connection.execute("SELECT 1", ()))
        .await?
        .is_ok()
    {
        return Err("redirect fixture unexpectedly executed the second statement".into());
    }
    drop(connection);
    drop(database);
    if guarded {
        if timeout(Duration::from_millis(250), &mut observed_accept)
            .await
            .is_ok()
        {
            return Err("guarded client attempted an HTTP connection".into());
        }
    } else {
        timeout(DEADLINE, observed_accept).await??;
    }
    if requests.load(Ordering::SeqCst) != 1 {
        return Err("redirect fixture did not observe exactly one TLS request".into());
    }
    while let Some(completed) = tasks.try_join_next() {
        completed??;
    }
    tasks.shutdown().await;
    Ok(())
}

async fn require_begin(
    connection: &libsql::Connection,
    tasks: &mut JoinSet<TestResult>,
) -> TestResult {
    if let Err(error) = timeout(DEADLINE, connection.execute("BEGIN IMMEDIATE", ())).await? {
        // A rejected fixture request otherwise appears only as client-side TLS EOF.
        if let Ok(Some(completed)) = timeout(Duration::from_millis(250), tasks.join_next()).await {
            completed??;
        }
        return Err(error.into());
    }
    Ok(())
}

#[tokio::test]
async fn real_libsql_rejects_server_http_base_url_after_authenticated_tls_begin() -> TestResult {
    timeout(Duration::from_secs(15), exercise_redirect(true)).await?
}

#[tokio::test]
async fn unguarded_libsql_control_reaches_the_server_http_base_url() -> TestResult {
    timeout(Duration::from_secs(15), exercise_redirect(false)).await?
}

async fn serve_read_only(
    listener: TcpListener,
    server_config: rustls::ServerConfig,
    observed: Arc<Mutex<Vec<String>>>,
) -> TestResult {
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let mut connections = JoinSet::new();
    let mut accepted_count = 0;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (socket, _) = accepted?;
                accepted_count += 1;
                if accepted_count > 8 {
                    return Err("read-only fixture connection bound exceeded".into());
                }
                let acceptor = acceptor.clone();
                let observed = observed.clone();
                connections.spawn(async move {
                    let stream = acceptor.accept(socket).await?;
                    hyper::server::conn::Http::new()
                        .http1_only(true)
                        .serve_connection(stream, service_fn(move |request| {
                            read_only_response(request, observed.clone())
                        }))
                        .await?;
                    Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
                });
            }
            Some(completed) = connections.join_next(), if !connections.is_empty() => {
                completed??;
            }
        }
    }
}

async fn exercise_read_only() -> TestResult {
    use crate::{
        INVENTORY_SCHEMA, InventoryScope, InventorySnapshot, InventoryStore, LibsqlInventory,
    };

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("https://{}", listener.local_addr()?);
    let (client_config, server_config) = tls_configs()?;
    let statements = Arc::new(Mutex::new(Vec::new()));
    let observed = statements.clone();
    let mut tasks: JoinSet<TestResult> = JoinSet::new();
    tasks.spawn(serve_read_only(listener, server_config, observed));
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(client_config)
        .https_only()
        .enable_http1()
        .build();
    let database = Builder::new_remote(endpoint, "synthetic-transport-test-token".to_owned())
        .connector(super::guarded_connector(connector))
        .build()
        .await?;
    let store = match LibsqlInventory::from_remote_read_only_database(database).await {
        Ok(store) => store,
        Err(error) => {
            if let Ok(Some(completed)) =
                timeout(Duration::from_millis(250), tasks.join_next()).await
            {
                completed??;
            }
            return Err(error.into());
        }
    };
    let scope = InventoryScope {
        cluster: "cluster".to_owned(),
        namespace: "workload".to_owned(),
        controller: "controller".to_owned(),
    };
    match store.load(&scope).await {
        Ok(None) => {}
        Ok(Some(_)) => return Err("read-only fixture returned an unexpected inventory head".into()),
        Err(error) => {
            if let Ok(Some(completed)) =
                timeout(Duration::from_millis(250), tasks.join_next()).await
            {
                completed??;
            }
            return Err(error.into());
        }
    }
    let before_publish = statements
        .lock()
        .map_err(|_| "read-only fixture statement lock was poisoned")?
        .len();
    let snapshot = InventorySnapshot {
        schema_version: INVENTORY_SCHEMA.to_owned(),
        generation: "generation".to_owned(),
        resources: Vec::new(),
    };
    if !matches!(
        store.publish(&scope, None, &snapshot).await,
        Err(crate::Error::Invalid("read-only inventory store"))
    ) {
        return Err("remote read-only inventory accepted publication".into());
    }
    {
        let captured = statements
            .lock()
            .map_err(|_| "read-only fixture statement lock was poisoned")?;
        if captured.len() != before_publish
            || captured
                .iter()
                .any(|sql| !sql.trim_start().starts_with("SELECT "))
        {
            return Err("remote read-only inventory issued a write".into());
        }
    }
    drop(store);
    tasks.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn remote_read_only_validates_and_reads_without_issuing_writes() -> TestResult {
    timeout(Duration::from_secs(15), exercise_read_only()).await?
}
