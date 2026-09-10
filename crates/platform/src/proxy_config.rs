//! Fixed OSS Envoy configuration. Secrets remain mounted files; no bearer values
//! or TLS private keys are serialized into this configuration.
use crate::{Result, invalid, valid_name};
use serde_json::{Value, json};

/// Build the pinned proxy profile for a generation. The platform must validate
/// and admit its digest independently before recording it as a qualified profile.
/// # Errors
/// Rejects invalid generation names. No arbitrary proxy hooks or routes exist.
pub fn envoy_configuration(generation: &str) -> Result<Value> {
    if !valid_name(generation) || generation.len() > 51 || generation.contains('.') {
        return Err(invalid("invalid proxy generation"));
    }
    let headers = [
        "authorization",
        "x-envoy-retry-on",
        "x-envoy-retry-grpc-on",
        "x-envoy-max-retries",
        "x-envoy-upstream-rq-timeout-ms",
        "x-envoy-upstream-rq-per-try-timeout-ms",
        "x-envoy-hedge-on-per-try-timeout",
        "x-envoy-upstream-rq-timeout-alt-response",
        "x-envoy-expected-rq-timeout-ms",
    ];
    let filters = json!([
        {"name":"envoy.filters.http.health_check","typed_config":{"@type":"type.googleapis.com/envoy.extensions.filters.http.health_check.v3.HealthCheck","pass_through_mode":true}},
        {"name":"envoy.filters.http.header_mutation","typed_config":{"@type":"type.googleapis.com/envoy.extensions.filters.http.header_mutation.v3.HeaderMutation","mutations":{"request_mutations":headers.iter().map(|header|json!({"remove":header})).collect::<Vec<_>>()}}},
        {"name":"envoy.filters.http.credential_injector","typed_config":{"@type":"type.googleapis.com/envoy.extensions.filters.http.credential_injector.v3.CredentialInjector","allow_request_without_credential":false,"overwrite":true,"credential":{"name":"envoy.http.injected_credentials.generic","typed_config":{"@type":"type.googleapis.com/envoy.extensions.http.injected_credentials.generic.v3.Generic","credential":{"name":"api-token","sds_config":{"path_config_source":{"path":"/etc/apsides/envoy/token-sds.json","watched_directory":{"path":"/etc/apsides/envoy"}},"resource_api_version":"V3"}},"header_value_prefix":"Bearer "}}}},
        {"name":"envoy.filters.http.router","typed_config":{"@type":"type.googleapis.com/envoy.extensions.filters.http.router.v3.Router"}}
    ]);
    let method = json!({"name":":method","string_match":{"safe_regex":{"google_re2":{},"regex":"^(POST|PUT|PATCH|DELETE)$"}}});
    let hcm = json!({"@type":"type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager","stat_prefix":"apsides","codec_type":"HTTP1","stream_idle_timeout":"20s","request_timeout":"20s","max_request_headers_kb":16,
        "route_config":{"name":"api","virtual_hosts":[{"name":"api","domains":["*"],"routes":[{"match":{"prefix":"/","headers":[method]},"route":{"cluster":"write_api","timeout":"15s","host_rewrite_literal":"kubernetes.default.svc"}},{"match":{"prefix":"/"},"route":{"cluster":"read_api","timeout":"15s","host_rewrite_literal":"kubernetes.default.svc"}}]}]},"http_filters":filters,
        "access_log":[{"name":"envoy.access_loggers.file","filter":{"header_filter":{"header":method}},"typed_config":{"@type":"type.googleapis.com/envoy.extensions.access_loggers.file.v3.FileAccessLog","path":"/evidence/access.jsonl","log_format":{"json_format":{"method":"%REQ(:METHOD)%","code":"%RESPONSE_CODE%","details":"%RESPONSE_CODE_DETAILS%","flags":"%RESPONSE_FLAGS%","attempts":"%UPSTREAM_REQUEST_ATTEMPT_COUNT%","upstream":"%UPSTREAM_HOST%"}}}}]});
    let configuration = json!({"node":{"id":generation,"cluster":"apsides"},"admin":{"address":{"pipe":{"path":"/evidence/admin.sock","mode":384}}},
        "overload_manager":{"resource_monitors":[{"name":"envoy.resource_monitors.global_downstream_max_connections","typed_config":{"@type":"type.googleapis.com/envoy.extensions.resource_monitors.downstream_connections.v3.DownstreamConnectionsConfig","max_active_downstream_connections":128}}]},
        "static_resources":{"listeners":[{"name":"gate","address":{"socket_address":{"address":"0.0.0.0","port_value":8443}},"per_connection_buffer_limit_bytes":65536,"filter_chains":[{"transport_socket":{"name":"envoy.transport_sockets.tls","typed_config":{"@type":"type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.DownstreamTlsContext","require_client_certificate":true,"common_tls_context":{"tls_params":{"tls_minimum_protocol_version":"TLSv1_3"},"tls_certificates":[{"certificate_chain":{"filename":"/etc/apsides/server-tls/tls.crt"},"private_key":{"filename":"/etc/apsides/server-tls/tls.key"}}],"validation_context":{"trusted_ca":{"filename":"/etc/apsides/server-tls/ca.crt"}}}}},"filters":[{"name":"envoy.filters.network.http_connection_manager","typed_config":hcm}]}]}],
        "clusters":(["write_api", "read_api"].map(|name| json!({"name":name,"type":"STRICT_DNS","connect_timeout":"5s","per_connection_buffer_limit_bytes":65536,"circuit_breakers":{"thresholds":[{"priority":"DEFAULT","max_connections":64,"max_pending_requests":64,"max_requests":64,"max_retries":0}]},"load_assignment":{"cluster_name":name,"endpoints":[{"lb_endpoints":[{"endpoint":{"address":{"socket_address":{"address":"kubernetes.default.svc","port_value":443}}}}]}]},"transport_socket":{"name":"envoy.transport_sockets.tls","typed_config":{"@type":"type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.UpstreamTlsContext","sni":"kubernetes.default.svc","common_tls_context":{"validation_context":{"trusted_ca":{"filename":"/var/run/secrets/kubernetes.io/serviceaccount/ca.crt"},"match_typed_subject_alt_names":[{"san_type":"DNS","matcher":{"exact":"kubernetes.default.svc"}}]}}}}})))}});
    Ok(configuration)
}

/// Standard SDS resource wrapper. Envoy watches the projected token's directory
/// for atomic rename, including Kubernetes projected-volume symlink changes.
#[must_use]
pub fn envoy_token_sds() -> Value {
    json!({"version_info":"1","resources":[{"@type":"type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.Secret","name":"api-token","generic_secret":{"secret":{"filename":"/var/run/secrets/kubernetes.io/serviceaccount/token"}}}]})
}
