//! What a generation's runtime credential can and cannot do at the API
//! server.
//!
//! [`sweep`] derives every request from the compiled plan: each declared
//! capability exercised as the runtime would use it, a broader request for
//! each RBAC gap the compiler discloses (an arbitrarily named create, a patch
//! of a field the program never sets, an update of a field it never sets),
//! a dry-run Server-Side Apply of every desired object, and requests outside
//! the declared scope that must be denied. Every write is a dry run. Each
//! request carries the authorization it asks for, so the same list drives a
//! `SubjectAccessReview` sweep.
//!
//! Responses are read for their status only; bodies are dropped unread,
//! because a declared read returns Secret data.

use crate::{
    Error, Result, ensure, infrastructure,
    objects::{Objects, text},
};
use compiler_core::{Capability, CompiledPlan};
use k8s_openapi::api::{
    authentication::v1::{TokenReview, TokenReviewSpec},
    authorization::v1::{ResourceAttributes, SubjectAccessReview, SubjectAccessReviewSpec},
};
use kube::{Api, Client, api::PostParams, client::Body};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The gap a Secret-key change demonstrates rather than a request.
pub const SENSITIVE_REVISION: &str = "sensitive-revision-change-observable";
const CREATE_GAP: &str = "rbac-create-not-name-restricted";
const PATCH_GAP: &str = "rbac-patch-not-field-restricted";
const UPDATE_GAP: &str = "rbac-update-not-field-restricted";
/// The gaps this sweep knows how to demonstrate.
const KNOWN_GAPS: [&str; 4] = [CREATE_GAP, PATCH_GAP, UPDATE_GAP, SENSITIVE_REVISION];
/// The name of every object the sweep asks about but must never exist.
const PROBE_NAME: &str = "aps-credential-probe";
/// The annotation the broader patch and update requests add.
const ANNOTATION: &str = "acceptance.apsides.dev/credential-probe";
/// The field manager of the dry-run applies.
const FIELD_MANAGER: &str = "aps-credential-acceptance";

/// The authorization one request asks for, as a `SubjectAccessReview` names
/// it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Access {
    pub verb: String,
    pub api_group: String,
    pub resource: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subresource: Option<String>,
    pub namespace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// One request of the sweep.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    pub id: String,
    pub method: &'static str,
    pub path: String,
    pub query: Vec<(&'static str, String)>,
    /// Never reported: a create or apply body is a desired object.
    #[serde(skip)]
    pub body: Option<Value>,
    #[serde(skip)]
    pub content_type: &'static str,
    /// The one status that passes.
    pub expected: u16,
    pub access: Access,
    /// The disclosed gap this request demonstrates, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gap: Option<&'static str>,
}

impl Probe {
    /// Whether the request must be authorized.
    #[must_use]
    pub const fn allowed(&self) -> bool {
        self.expected < 400
    }
}

/// Every request, and the gaps they demonstrate.
pub struct Sweep {
    pub probes: Vec<Probe>,
    /// The disclosed gaps the requests demonstrate.
    pub demonstrated: BTreeSet<&'static str>,
    /// Whether the plan also declares [`SENSITIVE_REVISION`], which a
    /// Secret-key change must demonstrate.
    pub sensitive_revision: bool,
}

/// `(api group, resource, namespace, name)` of an object.
pub type Target = (String, String, String, String);

/// The objects a declared `update` names, which [`sweep`] needs in their
/// live form.
///
/// # Errors
/// Fails for an `update` of anything but a `ConfigMap`, or when one is
/// absent.
pub async fn updated_objects(
    objects: &Objects,
    plan: &CompiledPlan,
) -> Result<BTreeMap<Target, Value>> {
    let mut live = BTreeMap::new();
    for capability in plan
        .capabilities
        .iter()
        .filter(|item| item.verb == "update")
    {
        ensure(
            capability.api_group.is_empty() && capability.resource == "configmaps",
            format!(
                "the sweep cannot update {}/{}",
                capability.api_group, capability.resource
            ),
        )?;
        for name in &capability.names {
            let object = objects
                .require("v1", "ConfigMap", Some(&capability.namespace), name)
                .await?;
            live.insert(
                (
                    String::new(),
                    "configmaps".into(),
                    capability.namespace.clone(),
                    name.clone(),
                ),
                object,
            );
        }
    }
    Ok(live)
}

/// Derive the sweep from `plan`, with `live` holding every object a declared
/// `update` names.
///
/// # Errors
/// Fails when the plan declares a gap, verb, subresource or API group the
/// sweep cannot exercise, or a gap it cannot demonstrate; and when a
/// capability would demonstrate a gap the plan does not declare.
pub fn sweep(plan: &CompiledPlan, live: &BTreeMap<Target, Value>) -> Result<Sweep> {
    let declared = plan
        .enforcement_gaps
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if let Some(unknown) = declared.iter().find(|gap| !KNOWN_GAPS.contains(gap)) {
        return Err(Error::Check(format!(
            "the plan declares a gap the sweep cannot exercise: {unknown}"
        )));
    }
    let desired = desired_objects(plan)?;
    let mut builder = Builder::default();
    for capability in &plan.capabilities {
        builder.capability(capability, &desired, live)?;
    }
    for ((group, resource, namespace, name), body) in &desired {
        builder.push(Probe {
            id: "intended-apply".into(),
            method: "PATCH",
            path: path(group, namespace, resource, Some(name))?,
            query: vec![
                ("dryRun", "All".into()),
                ("fieldManager", FIELD_MANAGER.into()),
            ],
            body: Some(body.clone()),
            content_type: "application/apply-patch+yaml",
            expected: 200,
            access: access("patch", group, resource, namespace, Some(name)),
            gap: None,
        });
    }
    builder.out_of_scope(plan)?;
    let Builder {
        probes,
        demonstrated,
    } = builder;
    if let Some(undeclared) = demonstrated.iter().find(|gap| !declared.contains(*gap)) {
        return Err(Error::Check(format!(
            "a declared capability has an undisclosed gap: {undeclared}"
        )));
    }
    if let Some(missing) = declared
        .iter()
        .find(|gap| **gap != SENSITIVE_REVISION && !demonstrated.contains(*gap))
    {
        return Err(Error::Check(format!(
            "no declared capability demonstrates the disclosed gap {missing}"
        )));
    }
    Ok(Sweep {
        probes,
        demonstrated,
        sensitive_revision: declared.contains(SENSITIVE_REVISION),
    })
}

#[derive(Default)]
struct Builder {
    probes: Vec<Probe>,
    demonstrated: BTreeSet<&'static str>,
}

impl Builder {
    fn push(&mut self, mut probe: Probe) {
        if let Some(gap) = probe.gap {
            self.demonstrated.insert(gap);
        }
        probe.id = format!("{}-{}", probe.id, self.probes.len());
        self.probes.push(probe);
    }

    fn get(
        &mut self,
        label: &str,
        path: String,
        query: Vec<(&'static str, String)>,
        expected: u16,
        access: Access,
    ) {
        self.push(Probe {
            id: label.into(),
            method: "GET",
            path,
            query,
            body: None,
            content_type: "application/json",
            expected,
            access,
            gap: None,
        });
    }

    /// The requests one declared capability allows and, for a list or
    /// watch, the unfiltered form it does not.
    fn capability(
        &mut self,
        capability: &Capability,
        desired: &BTreeMap<Target, Value>,
        live: &BTreeMap<Target, Value>,
    ) -> Result {
        let Capability {
            api_group: group,
            resource,
            verb,
            names,
            subresource,
            ..
        } = capability;
        ensure(
            subresource.is_none(),
            "the sweep cannot exercise a subresource capability",
        )?;
        ensure(
            (verb == "create") == names.is_empty(),
            format!("{verb} {group}/{resource} has an unsupported name scope"),
        )?;
        match verb.as_str() {
            "get" | "list" | "watch" => self.reads(capability),
            "patch" | "update" => self.writes(capability, live),
            "create" => self.create(capability, desired),
            other => Err(Error::Check(format!(
                "the sweep cannot exercise a declared {other}"
            ))),
        }
    }

    fn reads(&mut self, capability: &Capability) -> Result {
        let Capability {
            namespace,
            api_group: group,
            resource,
            verb,
            names,
            ..
        } = capability;
        let collection = path(group, namespace, resource, None)?;
        if verb == "get" {
            for name in names {
                self.get(
                    "declared-get",
                    path(group, namespace, resource, Some(name))?,
                    Vec::new(),
                    200,
                    access("get", group, resource, namespace, Some(name)),
                );
            }
            return Ok(());
        }
        let watch = || {
            if verb == "watch" {
                vec![
                    ("watch", "true".to_owned()),
                    ("timeoutSeconds", "1".to_owned()),
                ]
            } else {
                Vec::new()
            }
        };
        for name in names {
            let mut query = vec![("fieldSelector", format!("metadata.name={name}"))];
            query.extend(watch());
            self.get(
                &format!("declared-{verb}"),
                collection.clone(),
                query,
                200,
                access(verb, group, resource, namespace, Some(name)),
            );
        }
        self.get(
            &format!("unfiltered-{verb}"),
            collection,
            watch(),
            403,
            access(verb, group, resource, namespace, None),
        );
        Ok(())
    }

    /// A patch or update of each named object that changes a field the
    /// program never sets.
    fn writes(&mut self, capability: &Capability, live: &BTreeMap<Target, Value>) -> Result {
        let Capability {
            namespace,
            api_group: group,
            resource,
            verb,
            names,
            ..
        } = capability;
        for name in names {
            let probe = if verb == "patch" {
                Probe {
                    id: "undeclared-field-patch".into(),
                    method: "PATCH",
                    path: path(group, namespace, resource, Some(name))?,
                    query: vec![("dryRun", "All".into())],
                    body: Some(json!({"metadata": {"annotations": {ANNOTATION: "true"}}})),
                    content_type: "application/merge-patch+json",
                    expected: 200,
                    access: access("patch", group, resource, namespace, Some(name)),
                    gap: Some(PATCH_GAP),
                }
            } else {
                let key = (
                    group.clone(),
                    resource.clone(),
                    namespace.clone(),
                    name.clone(),
                );
                let Some(current) = live.get(&key) else {
                    return infrastructure(format!("no live {resource} {name} to update"));
                };
                Probe {
                    id: "undeclared-field-update".into(),
                    method: "PUT",
                    path: path(group, namespace, resource, Some(name))?,
                    query: vec![("dryRun", "All".into())],
                    body: Some(annotated(current)),
                    content_type: "application/json",
                    expected: 200,
                    access: access("update", group, resource, namespace, Some(name)),
                    gap: Some(UPDATE_GAP),
                }
            };
            self.push(probe);
        }
        Ok(())
    }

    /// A create of a desired object of the same type under a name the program
    /// never declares.
    fn create(&mut self, capability: &Capability, desired: &BTreeMap<Target, Value>) -> Result {
        let Capability {
            namespace,
            api_group: group,
            resource,
            ..
        } = capability;
        let Some(template) = desired.iter().find_map(|((g, r, n, _), body)| {
            (g == group && r == resource && n == namespace).then_some(body)
        }) else {
            return Err(Error::Check(format!(
                "no desired {group}/{resource} in {namespace} to create under another name"
            )));
        };
        let mut body = template.clone();
        body["metadata"] = json!({"name": PROBE_NAME, "namespace": namespace});
        self.push(Probe {
            id: "undeclared-name-create".into(),
            method: "POST",
            path: path(group, namespace, resource, None)?,
            query: vec![("dryRun", "All".into())],
            body: Some(body),
            content_type: "application/json",
            expected: 201,
            access: access("create", group, resource, namespace, None),
            gap: Some(CREATE_GAP),
        });
        Ok(())
    }

    /// Requests no plan of the scenario declares: another namespace, another
    /// name, RBAC, deletion, the platform's own objects and tokens.
    fn out_of_scope(&mut self, plan: &CompiledPlan) -> Result {
        let domain = &plan.ir.domain;
        let workload = domain.namespace.as_str();
        let control = domain.control_namespace.as_str();
        self.get(
            "outside-namespace",
            path("", "kube-system", "secrets", Some(PROBE_NAME))?,
            Vec::new(),
            403,
            access("get", "", "secrets", "kube-system", Some(PROBE_NAME)),
        );
        self.get(
            "outside-name",
            path("", workload, "secrets", Some(PROBE_NAME))?,
            Vec::new(),
            403,
            access("get", "", "secrets", workload, Some(PROBE_NAME)),
        );
        self.get(
            "rbac-read",
            path("rbac.authorization.k8s.io", workload, "roles", None)?,
            Vec::new(),
            403,
            access("list", "rbac.authorization.k8s.io", "roles", workload, None),
        );
        self.get(
            "deploy-identity-read",
            path(
                "",
                control,
                "configmaps",
                Some(&format!("apsides-deploy-identity-{}", domain.id)),
            )?,
            Vec::new(),
            403,
            access(
                "get",
                "",
                "configmaps",
                control,
                Some(&format!("apsides-deploy-identity-{}", domain.id)),
            ),
        );
        self.push(Probe {
            id: "unmanaged-delete".into(),
            method: "DELETE",
            path: path("", workload, "configmaps", Some(PROBE_NAME))?,
            query: vec![("dryRun", "All".into())],
            body: None,
            content_type: "application/json",
            expected: 403,
            access: access("delete", "", "configmaps", workload, Some(PROBE_NAME)),
            gap: None,
        });
        for prefix in ["apsides-trust-", "apsides-policy-"] {
            let name = format!("{prefix}{}", domain.id);
            self.push(Probe {
                id: "platform-state-patch".into(),
                method: "PATCH",
                path: path("", control, "configmaps", Some(&name))?,
                query: vec![("dryRun", "All".into())],
                body: Some(json!({"metadata": {"annotations": {ANNOTATION: "true"}}})),
                content_type: "application/merge-patch+json",
                expected: 403,
                access: access("patch", "", "configmaps", control, Some(&name)),
                gap: None,
            });
        }
        let mut token = access("create", "", "serviceaccounts", control, Some("default"));
        token.subresource = Some("token".into());
        self.push(Probe {
            id: "token-create".into(),
            method: "POST",
            path: format!(
                "{}/token",
                path("", control, "serviceaccounts", Some("default"))?
            ),
            query: vec![("dryRun", "All".into())],
            body: Some(json!({
                "apiVersion": "authentication.k8s.io/v1", "kind": "TokenRequest",
                "spec": {"audiences": ["acceptance"], "expirationSeconds": 600},
            })),
            content_type: "application/json",
            expected: 403,
            access: token,
            gap: None,
        });
        Ok(())
    }
}

fn access(verb: &str, group: &str, resource: &str, namespace: &str, name: Option<&str>) -> Access {
    Access {
        verb: verb.into(),
        api_group: group.into(),
        resource: resource.into(),
        subresource: None,
        namespace: namespace.into(),
        name: name.map(str::to_owned),
    }
}

/// `current` without status or server bookkeeping, plus the probe
/// annotation.
fn annotated(current: &Value) -> Value {
    let mut body = current.clone();
    if let Some(object) = body.as_object_mut() {
        object.remove("status");
    }
    if let Some(metadata) = body["metadata"].as_object_mut() {
        metadata.remove("resourceVersion");
        metadata.remove("managedFields");
    }
    body["metadata"]["annotations"][ANNOTATION] = json!("true");
    body
}

/// Every desired object, keyed by its API identity.
fn desired_objects(plan: &CompiledPlan) -> Result<BTreeMap<Target, Value>> {
    let mut desired = BTreeMap::new();
    for resource in &plan.ir.resources {
        let identity = deploy::resource_identity(resource)?;
        path(
            &identity.api_group,
            &identity.namespace,
            &identity.resource,
            Some(&identity.name),
        )?;
        let key = (
            identity.api_group,
            identity.resource,
            identity.namespace,
            identity.name,
        );
        ensure(
            desired.insert(key, resource.desired.clone()).is_none(),
            "the plan declares one object twice",
        )?;
    }
    Ok(desired)
}

/// The API path of a collection, or of one object in it.
fn path(group: &str, namespace: &str, resource: &str, name: Option<&str>) -> Result<String> {
    let prefix = match group {
        "" => "/api/v1".to_owned(),
        "apps" | "rbac.authorization.k8s.io" | "postgresql.cnpg.io" => format!("/apis/{group}/v1"),
        other => return infrastructure(format!("the sweep does not know API group {other}")),
    };
    let segments = [Some(namespace), Some(resource), name];
    ensure(
        segments.iter().flatten().all(|segment| {
            !segment.is_empty()
                && segment.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || byte == b'-'
                        || byte == b'.'
                })
        }),
        "a request path segment is not a Kubernetes name",
    )?;
    let mut path = format!("{prefix}/namespaces/{namespace}/{resource}");
    if let Some(name) = name {
        path.push('/');
        path.push_str(name);
    }
    Ok(path)
}

/// `query` as a URL query string.
fn encode(query: &[(&'static str, String)]) -> String {
    let escape = |value: &str| {
        value
            .bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    char::from(byte).to_string()
                }
                other => format!("%{other:02X}"),
            })
            .collect::<String>()
    };
    query
        .iter()
        .map(|(key, value)| format!("{key}={}", escape(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Send every probe with `client` and return each one's status, in order.
/// Response bodies are dropped unread.
///
/// # Errors
/// Fails when a request cannot be built or does not reach the API server.
pub async fn statuses(client: &Client, sweep: &Sweep) -> Result<Vec<u16>> {
    let mut statuses = Vec::with_capacity(sweep.probes.len());
    for probe in &sweep.probes {
        let uri = if probe.query.is_empty() {
            probe.path.clone()
        } else {
            format!("{}?{}", probe.path, encode(&probe.query))
        };
        let body = match &probe.body {
            Some(body) => serde_json::to_vec(body)?,
            None => Vec::new(),
        };
        let mut request = http::Request::builder().method(probe.method).uri(uri);
        if probe.body.is_some() {
            request = request.header(http::header::CONTENT_TYPE, probe.content_type);
        }
        let request = request
            .body(Body::from(body))
            .map_err(|error| Error::Infrastructure(format!("request {}: {error}", probe.id)))?;
        let response = client.send(request).await?;
        statuses.push(response.status().as_u16());
    }
    Ok(statuses)
}

/// The runtime identity one generation's API proxy holds: its
/// `ServiceAccount`, its Pod and the lifetime of the token projected into
/// it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Binding {
    pub namespace: String,
    pub service_account_name: String,
    pub service_account_uid: String,
    pub pod_name: String,
    pub pod_uid: String,
    pub envoy_container_id: String,
    pub token_lifetime_seconds: i64,
}

impl Binding {
    /// The user name the API server gives the `ServiceAccount`.
    #[must_use]
    pub fn username(&self) -> String {
        format!(
            "system:serviceaccount:{}:{}",
            self.namespace, self.service_account_name
        )
    }
}

/// The active generation's proxy binding, as the writer checkpoint records
/// it and the cluster still runs it.
///
/// # Errors
/// Fails when the `ServiceAccount` or proxy Pod is absent, being deleted or
/// not the recorded one, or the token projection is not the single
/// API-audience token the proxy is staged with.
pub async fn binding(
    objects: &Objects,
    checkpoint: &platform::WriterCheckpoint,
) -> Result<Binding> {
    let namespace = checkpoint.control_namespace.as_str();
    let proxy = &checkpoint.proxy_identity;
    let account_name = &checkpoint.runtime_binding.service_account_name;
    let account = objects
        .require("v1", "ServiceAccount", Some(namespace), account_name)
        .await?;
    let pod = objects
        .require("v1", "Pod", Some(namespace), &proxy.pod_name)
        .await?;
    let envoy = pod["status"]["containerStatuses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|status| status["name"] == "envoy")
        .collect::<Vec<_>>();
    let projections = pod["spec"]["volumes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|volume| volume["name"] == "api-token")
        .flat_map(|volume| {
            volume["projected"]["sources"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .filter_map(|source| source.get("serviceAccountToken"))
        .collect::<Vec<_>>();
    let ([envoy], [projection]) = (envoy.as_slice(), projections.as_slice()) else {
        return Err(Error::Check(
            "the proxy Pod does not run one envoy with one projected token".into(),
        ));
    };
    ensure(
        !crate::objects::deleting(&account)
            && !crate::objects::deleting(&pod)
            && text(&pod, "/metadata/uid") == proxy.pod_uid
            && text(&pod, "/spec/serviceAccountName") == account_name
            && text(envoy, "/containerID") == proxy.envoy_container_id
            && envoy["ready"] == true
            && text(projection, "/path") == "token"
            && projection.get("audience").is_none(),
        "the proxy Pod or its ServiceAccount is not the recorded runtime binding",
    )?;
    Ok(Binding {
        namespace: namespace.to_owned(),
        service_account_name: account_name.clone(),
        service_account_uid: text(&account, "/metadata/uid").to_owned(),
        pod_name: proxy.pod_name.clone(),
        pod_uid: proxy.pod_uid.clone(),
        envoy_container_id: proxy.envoy_container_id.clone(),
        token_lifetime_seconds: projection["expirationSeconds"].as_i64().unwrap_or_default(),
    })
}

/// The token the kubelet projects into the proxy's envoy container, read
/// through the API server and held only in memory.
///
/// # Errors
/// Fails when the file cannot be read or the binding changed meanwhile.
pub async fn projected_token(objects: &Objects, binding: &Binding) -> Result<String> {
    let token = crate::cnpg::exec(
        objects.client(),
        &binding.namespace,
        &binding.pod_name,
        "envoy",
        &[
            "/bin/cat",
            "/var/run/secrets/kubernetes.io/serviceaccount/token",
        ],
    )
    .await?
    .trim()
    .to_owned();
    ensure(
        !token.is_empty() && token.len() <= 64 * 1024,
        "the projected token is empty or oversized",
    )?;
    Ok(token)
}

/// The identity a runtime token authenticates as, from a `TokenReview`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Authenticated {
    pub username: String,
    pub uid: String,
    pub groups: Vec<String>,
    pub pod_name: String,
    pub pod_uid: String,
}

/// Ask the API server who `token` is.
///
/// # Errors
/// Fails when the review is refused or does not authenticate the token as
/// a Pod-bound `ServiceAccount` token.
pub async fn review_token(admin: &Client, token: &str) -> Result<Authenticated> {
    let reviews: Api<TokenReview> = Api::all(admin.clone());
    let review = TokenReview {
        spec: TokenReviewSpec {
            token: token.to_owned(),
            audiences: None,
        },
        ..TokenReview::default()
    };
    let status = reviews
        .create(&PostParams::default(), &review)
        .await?
        .status
        .ok_or_else(|| Error::Check("the TokenReview has no status".into()))?;
    let user = status.user.unwrap_or_default();
    let extra = user.extra.unwrap_or_default();
    let single = |key: &str| match extra.get(key).map(Vec::as_slice) {
        Some([value]) => value.clone(),
        _ => String::new(),
    };
    let authenticated = Authenticated {
        username: user.username.unwrap_or_default(),
        uid: user.uid.unwrap_or_default(),
        groups: user.groups.unwrap_or_default(),
        pod_name: single("authentication.kubernetes.io/pod-name"),
        pod_uid: single("authentication.kubernetes.io/pod-uid"),
    };
    ensure(
        status.authenticated == Some(true)
            && status.error.as_deref().is_none_or(str::is_empty)
            && !authenticated.pod_name.is_empty()
            && !authenticated.pod_uid.is_empty(),
        "the TokenReview does not authenticate a Pod-bound token",
    )?;
    Ok(authenticated)
}

/// The claims of a `ServiceAccount` JWT, decoded without verification; only
/// a [`review_token`] authenticates them.
///
/// # Errors
/// Fails when `token` is not a JWT with a JSON payload.
pub fn claims(token: &str) -> Result<Value> {
    use base64::Engine as _;
    let parts = token.split('.').collect::<Vec<_>>();
    let [_, payload, _] = parts.as_slice() else {
        return Err(Error::Check("the runtime token is not a JWT".into()));
    };
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| Error::Check("the runtime token payload is not base64url".into()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Ask the API server, once per distinct request, whether `user` in
/// `groups` may make it, and return each answer with the expected one.
///
/// # Errors
/// Fails when a review is refused.
pub async fn review_access(
    admin: &Client,
    user: &Authenticated,
    sweep: &Sweep,
) -> Result<Vec<(Access, bool, bool)>> {
    let reviews: Api<SubjectAccessReview> = Api::all(admin.clone());
    let mut expected = BTreeMap::new();
    for probe in &sweep.probes {
        let allowed = probe.allowed();
        if expected.insert(probe.access.clone(), allowed) == Some(!allowed) {
            return Err(Error::Check(format!(
                "the sweep expects both outcomes for {:?}",
                probe.access
            )));
        }
    }
    let mut answers = Vec::with_capacity(expected.len());
    for (access, allowed) in expected {
        let review = SubjectAccessReview {
            spec: SubjectAccessReviewSpec {
                user: Some(user.username.clone()),
                uid: Some(user.uid.clone()),
                groups: Some(user.groups.clone()),
                resource_attributes: Some(ResourceAttributes {
                    verb: Some(access.verb.clone()),
                    group: Some(access.api_group.clone()),
                    resource: Some(access.resource.clone()),
                    subresource: access.subresource.clone(),
                    namespace: Some(access.namespace.clone()),
                    name: access.name.clone(),
                    ..ResourceAttributes::default()
                }),
                ..SubjectAccessReviewSpec::default()
            },
            ..SubjectAccessReview::default()
        };
        let answer = reviews
            .create(&PostParams::default(), &review)
            .await?
            .status
            .is_some_and(|status| status.allowed);
        answers.push((access, allowed, answer));
    }
    Ok(answers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capability(
        namespace: &str,
        group: &str,
        resource: &str,
        verb: &str,
        names: &[&str],
    ) -> Capability {
        Capability {
            namespace: namespace.into(),
            api_group: group.into(),
            resource: resource.into(),
            subresource: None,
            verb: verb.into(),
            names: names.iter().map(|name| (*name).to_owned()).collect(),
        }
    }

    fn plan(capabilities: Vec<Capability>, gaps: &[&str]) -> Result<CompiledPlan> {
        let mut plan: CompiledPlan = serde_json::from_value(json!({
            "irDigest": format!("sha256:{}", "0".repeat(64)),
            "ir": {
                "schemaVersion": "apsides.dev/ir/v1", "testOnly": false,
                "domain": {"id": "demo", "namespace": "default", "controlNamespace": "apsides-system",
                    "clusterBinding": "local", "policyRef": "platform/demo", "enforcement": "api-bound",
                    "budgets": {"maxResources": 4, "maxInputs": 4, "maxReactions": 4, "maxObjectBytes": 262_144,
                        "maxQueueDepth": 8, "maxConcurrentRequests": 1}},
                "resources": [{"id": "config", "apiVersion": "v1", "kind": "ConfigMap", "name": "config",
                    "namespace": "default",
                    "desired": {"apiVersion": "v1", "kind": "ConfigMap",
                        "metadata": {"name": "config", "namespace": "default"}, "data": {"key": "value"}},
                    "lifecycle": {"deletionPolicy": "orphan"},
                    "source": {"file": "program.tsx", "line": 1, "column": 1}}],
                "inputs": [], "reactions": [],
                "provenance": {"compiler": "apsides/0.1", "inputs": []},
            },
            "effects": [], "requestPlan": [], "capabilities": [], "enforcementGaps": [],
            "indirectRisks": [], "testPlan": [],
        }))?;
        plan.capabilities = capabilities;
        plan.enforcement_gaps = gaps.iter().map(|gap| (*gap).to_owned()).collect();
        Ok(plan)
    }

    fn inventory() -> BTreeMap<Target, Value> {
        BTreeMap::from([(
            (
                String::new(),
                "configmaps".into(),
                "apsides-system".into(),
                "apsides-inventory-demo".into(),
            ),
            json!({"apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {"name": "apsides-inventory-demo", "namespace": "apsides-system",
                    "uid": "u", "resourceVersion": "7", "managedFields": []},
                "data": {"state.json": "{}"}}),
        )])
    }

    fn declared() -> Vec<Capability> {
        vec![
            capability("default", "", "configmaps", "create", &[]),
            capability("default", "", "configmaps", "list", &["config"]),
            capability("default", "", "configmaps", "patch", &["config"]),
            capability(
                "apsides-system",
                "",
                "configmaps",
                "update",
                &["apsides-inventory-demo"],
            ),
            capability("default", "", "secrets", "watch", &["database-app"]),
        ]
    }

    #[test]
    fn every_declared_capability_and_gap_becomes_a_request() -> Result {
        let sweep = sweep(
            &plan(
                declared(),
                &[CREATE_GAP, PATCH_GAP, UPDATE_GAP, SENSITIVE_REVISION],
            )?,
            &inventory(),
        )?;
        assert!(sweep.sensitive_revision);
        assert_eq!(
            sweep.demonstrated,
            BTreeSet::from([CREATE_GAP, PATCH_GAP, UPDATE_GAP])
        );
        let find = |label: &str| {
            sweep
                .probes
                .iter()
                .find(|probe| probe.id.starts_with(label))
                .ok_or_else(|| Error::Check(format!("no {label} request")))
        };
        let create = find("undeclared-name-create")?;
        assert_eq!(
            (create.method, create.path.as_str(), create.expected),
            ("POST", "/api/v1/namespaces/default/configmaps", 201)
        );
        assert_eq!(
            create.body.as_ref().map(|body| &body["metadata"]["name"]),
            Some(&json!(PROBE_NAME))
        );
        let update = find("undeclared-field-update")?;
        let body = update.body.clone().unwrap_or_default();
        assert!(
            body["metadata"]["resourceVersion"].is_null()
                && body["metadata"]["managedFields"].is_null()
        );
        assert_eq!(body["metadata"]["annotations"][ANNOTATION], "true");
        let watch = find("declared-watch")?;
        assert_eq!(
            encode(&watch.query),
            "fieldSelector=metadata.name%3Ddatabase-app&watch=true&timeoutSeconds=1"
        );
        assert_eq!(find("unfiltered-watch")?.expected, 403);
        assert_eq!(
            find("intended-apply")?.content_type,
            "application/apply-patch+yaml"
        );
        let token = find("token-create")?;
        assert_eq!(
            (
                token.path.as_str(),
                token.access.subresource.as_deref(),
                token.allowed()
            ),
            (
                "/api/v1/namespaces/apsides-system/serviceaccounts/default/token",
                Some("token"),
                false
            )
        );
        Ok(())
    }

    #[test]
    fn gaps_must_match_what_the_capabilities_demonstrate() -> Result {
        let undisclosed = sweep(&plan(declared(), &[CREATE_GAP, PATCH_GAP])?, &inventory());
        assert!(
            matches!(&undisclosed, Err(Error::Check(message)) if message.contains(UPDATE_GAP)),
            "{:?}",
            undisclosed.err()
        );
        let unexercised = sweep(
            &plan(
                vec![capability("default", "", "configmaps", "list", &["config"])],
                &[PATCH_GAP],
            )?,
            &BTreeMap::new(),
        );
        assert!(unexercised.is_err());
        let unknown = sweep(
            &plan(Vec::new(), &["rbac-delete-anything"])?,
            &BTreeMap::new(),
        );
        assert!(unknown.is_err());
        Ok(())
    }

    #[test]
    fn paths_accept_only_kubernetes_names() -> Result {
        assert_eq!(
            path("apps", "default", "deployments", Some("api"))?,
            "/apis/apps/v1/namespaces/default/deployments/api"
        );
        assert!(path("", "default", "configmaps", Some("a/b")).is_err());
        assert!(path("example.com", "default", "widgets", None).is_err());
        Ok(())
    }

    #[test]
    fn claims_decode_the_jwt_payload() -> Result {
        use base64::Engine as _;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":7}"#);
        assert_eq!(claims(&format!("h.{payload}.s"))?, json!({"exp": 7}));
        assert!(claims("not-a-jwt").is_err());
        Ok(())
    }
}
