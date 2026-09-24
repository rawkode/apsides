use crate::{
    Capability, CompiledPlan, Effect, Error, HANDLER_OUTPUT_LABEL, HANDLER_TRIGGER_INPUT_CHANGED,
    Handler, IR_SCHEMA, Input, InputMode, Ir, MAX_EGRESS_HOSTS, MAX_HANDLER_HEAP_BYTES,
    MAX_HANDLER_TIMEOUT_MILLIS, MAX_HANDLERS, Output, PermissionCeiling, Provenance, Request,
    Resource, Result, SourceLocation, TestCase, canonical_bytes, digest, parse_strict,
    resolve_resource_api,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

fn require(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(Error::Invalid(message.into()))
    }
}

fn dns_label(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|v| v.is_ascii_lowercase() || v.is_ascii_digit() || v == b'-')
}

fn object_name(value: &str) -> bool {
    value.len() <= 253 && value.split('.').all(|part| dns_label(part, 63))
}

fn source_valid(source: &SourceLocation, provenance: &Provenance) -> bool {
    source.line > 0
        && source.column > 0
        && provenance
            .inputs
            .iter()
            .any(|input| input.path == source.file)
}

fn identity(api_group: &str, plural: &str, name: &str, namespace: &str) -> String {
    format!("{api_group}/{plural}/{namespace}/{name}")
}

fn api(kind: &str) -> Result<(&'static str, &'static str)> {
    Ok(match kind {
        "ConfigMap" => ("", "configmaps"),
        "Secret" => ("", "secrets"),
        "Service" => ("", "services"),
        "Deployment" => ("apps", "deployments"),
        _ => return Err(Error::Invalid("unsupported resource kind".into())),
    })
}

fn request(
    requests: &mut Vec<Request>,
    purpose: &str,
    kind: &str,
    namespace: &str,
    name: &str,
    verb: &str,
) -> Result<()> {
    let (api_group, resource) = api(kind)?;
    resolved_request(
        requests, purpose, api_group, resource, namespace, name, verb,
    );
    Ok(())
}

fn resolved_request(
    requests: &mut Vec<Request>,
    purpose: &str,
    api_group: &str,
    resource: &str,
    namespace: &str,
    name: &str,
    verb: &str,
) {
    requests.push(Request {
        purpose: purpose.into(),
        dry_run: purpose == "managed-target-dry-run-apply",
        capability: Capability {
            namespace: namespace.into(),
            api_group: api_group.into(),
            resource: resource.into(),
            subresource: None,
            verb: verb.into(),
            names: if verb == "create" {
                vec![]
            } else {
                vec![name.into()]
            },
        },
        field_selector: matches!(verb, "list" | "watch").then(|| format!("metadata.name={name}")),
    });
}

fn field_paths(value: &Value, path: &str, result: &mut Vec<String>) {
    if let Some(map) = value.as_object() {
        if map.is_empty() && !path.is_empty() {
            result.push(path.into());
        }
        for (key, value) in map {
            let key = key.replace('~', "~0").replace('/', "~1");
            let next = format!("{path}/{key}");
            if matches!(
                next.as_str(),
                "/apiVersion" | "/kind" | "/metadata/name" | "/metadata/namespace"
            ) {
                continue;
            }
            field_paths(value, &next, result);
        }
    } else {
        result.push(path.into());
    }
}

/// Validate the finite IR and recompute all derived authority.
///
/// # Errors
/// Rejects unsupported operations, scope, schema, provenance, or budgets.
pub fn validate(ir: &Ir) -> Result<CompiledPlan> {
    validate_domain(ir)?;
    let resources = validate_resources(ir)?;
    let outputs = validate_outputs(ir, &resources)?;
    let inputs = validate_inputs(ir, &resources)?;
    validate_reactions(ir, &resources, &inputs)?;
    validate_handlers(ir, &inputs, &outputs)?;
    derive_plan(ir, &inputs)
}

fn validate_domain(ir: &Ir) -> Result<()> {
    // Validate the same numeric and depth contract even for typed callers.
    let bytes = canonical_bytes(ir)?;
    parse_strict(
        std::str::from_utf8(&bytes).map_err(|_| Error::Invalid("invalid encoding".into()))?,
    )?;
    require(
        ir.schema_version == IR_SCHEMA,
        "unsupported IR schemaVersion",
    )?;
    let domain = &ir.domain;
    require(
        dns_label(&domain.id, 40),
        "domain.id must be a DNS label of at most 40 bytes",
    )?;
    require(
        dns_label(&domain.namespace, 63) && dns_label(&domain.control_namespace, 63),
        "invalid namespace",
    )?;
    require(
        domain.namespace != domain.control_namespace,
        "control namespace must be outside workload scope",
    )?;
    require(
        domain.enforcement == "api-bound",
        "unsupported enforcement profile; effect-bound requests cannot be downgraded",
    )?;
    require(
        !domain.cluster_binding.is_empty()
            && domain.cluster_binding.len() <= 256
            && !domain.policy_ref.is_empty()
            && domain.policy_ref.len() <= 256,
        "clusterBinding and policyRef must be explicit bounded strings",
    )?;
    require(
        crate::is_supported_min_version(&domain.min_kubernetes_version),
        format!(
            "domain.minKubernetesVersion must be MAJOR.MINOR with major {} and minor >= {} (the qualified floor); received {}",
            crate::KUBERNETES_QUALIFIED_FLOOR.0,
            crate::KUBERNETES_QUALIFIED_FLOOR.1,
            domain.min_kubernetes_version
        ),
    )?;
    let b = &domain.budgets;
    require(
        (1..=256).contains(&b.max_resources)
            && (1..=256).contains(&b.max_inputs)
            && (1..=256).contains(&b.max_reactions),
        "resource/input/reaction budgets must be in 1..=256",
    )?;
    require(
        (1024..=262_144).contains(&b.max_object_bytes)
            && (1..=1024).contains(&b.max_queue_depth)
            && (1..=32).contains(&b.max_concurrent_requests),
        "invalid object, queue, or concurrency budget",
    )?;
    require(
        ir.resources.len() <= b.max_resources
            && ir.inputs.len() <= b.max_inputs
            && ir.reactions.len() <= b.max_reactions
            && ir.resources.len() <= b.max_queue_depth,
        "domain exceeds declared resource/input/reaction/queue budget",
    )?;
    require(
        !ir.provenance.compiler.is_empty()
            && ir.provenance.compiler.len() <= 256
            && !ir.provenance.inputs.is_empty()
            && ir.provenance.inputs.len() <= 256,
        "missing or oversized build provenance",
    )?;
    let mut paths = BTreeSet::new();
    for input in &ir.provenance.inputs {
        require(
            !input.path.is_empty()
                && input.path.len() <= 512
                && !input.path.starts_with('/')
                && !input.path.contains('\\')
                && input
                    .path
                    .split('/')
                    .all(|part| !matches!(part, "" | "." | "..")),
            "provenance paths must be normalized relative paths",
        )?;
        require(paths.insert(&input.path), "duplicate provenance input path")?;
        require(
            input.sha256.len() == 64
                && input
                    .sha256
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "invalid provenance sha256",
        )?;
    }
    Ok(())
}

fn validate_resources(ir: &Ir) -> Result<BTreeMap<String, &Resource>> {
    let mut resources = BTreeMap::new();
    let mut identities = BTreeSet::new();
    for resource in &ir.resources {
        require(
            dns_label(&resource.id, 63)
                && resources.insert(resource.id.clone(), resource).is_none(),
            "invalid or duplicate resource id",
        )?;
        require(
            object_name(&resource.name) && resource.namespace == ir.domain.namespace,
            "resource identity is outside the fixed workload scope",
        )?;
        let api = resolve_resource_api(resource)?;
        require(
            identities.insert(identity(
                &api.api_group,
                &api.plural,
                &resource.name,
                &resource.namespace,
            )),
            "overlapping managed object identities",
        )?;
        require(
            source_valid(&resource.source, &ir.provenance),
            format!(
                "resource {} source is not a declared build input",
                resource.id
            ),
        )?;
        require(
            canonical_bytes(&resource.desired)?.len() <= ir.domain.budgets.max_object_bytes,
            format!("resource {} exceeds object byte budget", resource.id),
        )?;
        validate_handler_output_label(resource)?;
        crate::schema::validate_resource(resource)?;
    }
    Ok(resources)
}

/// The reserved `apsides.dev/handler-output` label is required on every
/// `handlerOutput` resource (with its value equal to the resource's own id)
/// and forbidden on every other resource, exactly like `apsides.dev/revision`
/// is reserved for the rollout primitive.
fn validate_handler_output_label(resource: &Resource) -> Result<()> {
    let label = resource
        .desired
        .pointer("/metadata/labels")
        .and_then(Value::as_object)
        .and_then(|labels| labels.get(HANDLER_OUTPUT_LABEL));
    require(
        label.is_some() == resource.handler_output,
        format!(
            "{HANDLER_OUTPUT_LABEL} is reserved for handler outputs (resource {})",
            resource.id
        ),
    )?;
    if resource.handler_output {
        require(
            resource.kind == "ConfigMap" && resource.desired.get("data").is_none(),
            format!(
                "handler output resource {} must be a metadata-only ConfigMap",
                resource.id
            ),
        )?;
        require(
            label.and_then(Value::as_str) == Some(resource.id.as_str()),
            format!(
                "{HANDLER_OUTPUT_LABEL} must equal the output id on resource {}",
                resource.id
            ),
        )?;
    }
    Ok(())
}

/// Validate every declared handler `Output`, and return the id-keyed map of
/// them alongside the identity set they occupy (used to reject aliasing).
fn validate_outputs<'a>(
    ir: &'a Ir,
    resources: &BTreeMap<String, &Resource>,
) -> Result<BTreeMap<String, &'a Output>> {
    let mut outputs = BTreeMap::new();
    for output in &ir.outputs {
        require(
            dns_label(&output.id, 63) && outputs.insert(output.id.clone(), output).is_none(),
            "invalid or duplicate output id",
        )?;
        require(
            object_name(&output.name) && output.namespace == ir.domain.namespace,
            "output identity is outside the fixed workload scope",
        )?;
        let resource = resources.get(&output.id).ok_or_else(|| {
            Error::Invalid(format!(
                "output {} has no backing handlerOutput resource",
                output.id
            ))
        })?;
        require(
            resource.handler_output
                && resource.kind == "ConfigMap"
                && resource.name == output.name
                && resource.namespace == output.namespace,
            format!(
                "output {} backing resource identity or handlerOutput flag mismatch",
                output.id
            ),
        )?;
    }
    require(
        resources
            .values()
            .filter(|resource| resource.handler_output)
            .count()
            == outputs.len(),
        "every handlerOutput resource must have a matching declared output",
    )?;
    Ok(outputs)
}

fn validate_inputs<'a>(
    ir: &'a Ir,
    resources: &BTreeMap<String, &Resource>,
) -> Result<BTreeMap<String, &'a Input>> {
    let identities: BTreeSet<_> = ir
        .resources
        .iter()
        .map(|resource| {
            resolve_resource_api(resource).map(|api| {
                identity(
                    &api.api_group,
                    &api.plural,
                    &resource.name,
                    &resource.namespace,
                )
            })
        })
        .collect::<Result<_>>()?;
    let mut inputs = BTreeMap::new();
    for input in &ir.inputs {
        require(
            dns_label(&input.id, 63) && inputs.insert(input.id.clone(), input).is_none(),
            "invalid or duplicate input id",
        )?;
        require(
            input.api_version == "v1" && matches!(input.kind.as_str(), "ConfigMap" | "Secret"),
            "unsupported observed API type",
        )?;
        require(
            object_name(&input.name) && input.namespace == ir.domain.namespace,
            "input identity is outside the fixed workload scope",
        )?;
        require(
            input.projection == ["data"] && input.missing == "block-dependent-mutations",
            "unsupported projection or missing-input policy",
        )?;
        require(
            input.sensitive == (input.kind == "Secret"),
            "Secret classification must be sensitive; sensitive ConfigMaps are unsupported in this increment",
        )?;
        require(
            source_valid(&input.source, &ir.provenance),
            format!("input {} source is not a declared build input", input.id),
        )?;
        match input.mode {
            InputMode::Managed => {
                let resource = input
                    .managed_resource_id
                    .as_ref()
                    .and_then(|id| resources.get(id))
                    .ok_or_else(|| {
                        Error::Invalid(
                            "managed input requires an existing managedResourceId".into(),
                        )
                    })?;
                require(
                    input.kind == "ConfigMap"
                        && resource.kind == input.kind
                        && resource.name == input.name
                        && resource.namespace == input.namespace,
                    "managed input identity differs from its managed resource",
                )?;
            }
            InputMode::Observed => {
                require(
                    input.managed_resource_id.is_none(),
                    "observed input cannot have managedResourceId",
                )?;
                let (api_group, plural) = api(&input.kind)?;
                require(
                    !identities.contains(&identity(
                        api_group,
                        plural,
                        &input.name,
                        &input.namespace,
                    )),
                    "observed input aliases a managed resource; use managed mode",
                )?;
            }
            InputMode::Output => {
                require(!input.sensitive, "handler output input cannot be sensitive")?;
                let resource = input
                    .managed_resource_id
                    .as_ref()
                    .and_then(|id| resources.get(id))
                    .ok_or_else(|| {
                        Error::Invalid("output input requires an existing managedResourceId".into())
                    })?;
                require(
                    input.kind == "ConfigMap"
                        && resource.handler_output
                        && resource.name == input.name
                        && resource.namespace == input.namespace,
                    "output input identity differs from its handler output resource",
                )?;
            }
        }
    }
    Ok(inputs)
}

fn validate_reactions(
    ir: &Ir,
    resources: &BTreeMap<String, &Resource>,
    inputs: &BTreeMap<String, &Input>,
) -> Result<()> {
    let mut reaction_ids = BTreeSet::new();
    for reaction in &ir.reactions {
        require(
            dns_label(&reaction.id, 63) && reaction_ids.insert(&reaction.id),
            "invalid or duplicate reaction id",
        )?;
        require(reaction.op == "rollout", "unsupported reaction operation")?;
        require(
            source_valid(&reaction.source, &ir.provenance),
            format!(
                "reaction {} source is not a declared build input",
                reaction.id
            ),
        )?;
        require(
            resources
                .get(&reaction.target)
                .is_some_and(|target| target.kind == "Deployment"),
            "rollout target must be a declared Deployment",
        )?;
        let unique: BTreeSet<_> = reaction.inputs.iter().collect();
        require(
            !unique.is_empty()
                && unique.len() == reaction.inputs.len()
                && unique.len() <= ir.domain.budgets.max_inputs,
            "reaction requires bounded distinct input references",
        )?;
        require(
            reaction.inputs.iter().all(|id| inputs.contains_key(id)),
            "reaction has unresolved input reference",
        )?;
    }

    Ok(())
}

/// A lowercase DNS name suitable for a handler's egress allowlist: never an
/// IP literal (v4 or v6), never containing a wildcard, at most 253 bytes.
fn is_egress_host(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_err()
        && host.len() <= 253
        && !host.is_empty()
        && host.split('.').all(|part| dns_label(part, 63))
}

/// A bare identifier suitable for a handler's named ES module export.
fn is_js_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    value.len() <= 128
        && chars
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || matches!(first, '_' | '$'))
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$'))
}

/// Depth-first cycle detection over the handler-trigger graph: `path` holds
/// every node on the current call stack, `visited` every node already known
/// acyclic from a prior traversal.
fn detect_handler_cycle<'a>(
    edges: &BTreeMap<&'a str, BTreeSet<&'a str>>,
    node: &'a str,
    visited: &mut BTreeSet<&'a str>,
    path: &mut BTreeSet<&'a str>,
) -> Result<()> {
    if !path.insert(node) {
        return Err(Error::Invalid(
            "handler graph contains a cycle through input-changed triggers".into(),
        ));
    }
    if visited.insert(node) {
        for next in edges.get(node).into_iter().flatten() {
            detect_handler_cycle(edges, next, visited, path)?;
        }
    }
    path.remove(node);
    Ok(())
}

/// Validate every declared `Handler`: identity, module/export/bundle shape,
/// budgets, resolved inputs/outputs, egress host syntax, and acyclicity of
/// the handler-trigger graph (a handler's outputs feeding another handler's
/// inputs through an [`InputMode::Output`] input).
/// Validate one handler's identity, module/export/bundle shape, source and
/// budgets (everything but its resolved inputs, outputs and egress hosts).
fn validate_handler_shape(ir: &Ir, handler: &Handler) -> Result<()> {
    require(
        dns_label(&handler.id, 63),
        "invalid or duplicate handler id",
    )?;
    require(
        !handler.module.is_empty()
            && handler.module.len() <= 512
            && !handler.module.starts_with('/')
            && !handler.module.contains('\\')
            && handler
                .module
                .split('/')
                .all(|part| !matches!(part, "" | "." | "..")),
        format!(
            "handler {} module must be a normalized relative path",
            handler.id
        ),
    )?;
    require(
        ir.provenance
            .inputs
            .iter()
            .any(|input| input.path == handler.module),
        format!(
            "handler {} module is not a declared build input",
            handler.id
        ),
    )?;
    require(
        is_js_identifier(&handler.export),
        format!("handler {} export must be a valid identifier", handler.id),
    )?;
    require(
        handler.bundle_sha256.len() == 64
            && handler
                .bundle_sha256
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        format!("handler {} has an invalid bundleSha256", handler.id),
    )?;
    require(
        source_valid(&handler.source, &ir.provenance),
        format!(
            "handler {} source is not a declared build input",
            handler.id
        ),
    )?;
    require(
        (1..=MAX_HANDLER_TIMEOUT_MILLIS).contains(&handler.timeout_millis),
        format!(
            "handler {} timeoutMillis must be in 1..={MAX_HANDLER_TIMEOUT_MILLIS}",
            handler.id
        ),
    )?;
    require(
        (1..=MAX_HANDLER_HEAP_BYTES).contains(&handler.max_heap_bytes),
        format!(
            "handler {} maxHeapBytes must be in 1..={MAX_HANDLER_HEAP_BYTES}",
            handler.id
        ),
    )?;
    require(
        handler.trigger.kind == HANDLER_TRIGGER_INPUT_CHANGED,
        format!("handler {} has an unsupported trigger kind", handler.id),
    )
}

/// Validate one handler's resolved inputs, outputs and egress hosts.
/// Inserts the handler's egress hosts into `egress_hosts` for the caller's
/// aggregate `maxEgressHosts` check.
fn validate_handler_edges(
    handler: &Handler,
    inputs: &BTreeMap<String, &Input>,
    outputs: &BTreeMap<String, &Output>,
    egress_hosts: &mut BTreeSet<String>,
) -> Result<()> {
    let unique_inputs: BTreeSet<_> = handler.inputs.iter().collect();
    require(
        !handler.inputs.is_empty() && unique_inputs.len() == handler.inputs.len(),
        format!("handler {} requires distinct nonempty inputs", handler.id),
    )?;
    require(
        unique_inputs.contains(&handler.trigger.input),
        format!(
            "handler {} trigger input must be one of its declared inputs",
            handler.id
        ),
    )?;
    for input_id in &handler.inputs {
        let input = inputs.get(input_id).ok_or_else(|| {
            Error::Invalid(format!(
                "handler {} has unresolved input {input_id}",
                handler.id
            ))
        })?;
        require(
            input.kind == "ConfigMap" && !input.sensitive,
            format!(
                "handler {} input {input_id} must be a non-sensitive ConfigMap input",
                handler.id
            ),
        )?;
        if input.mode == InputMode::Output {
            let backing = input.managed_resource_id.as_deref().unwrap_or_default();
            require(
                !handler.outputs.iter().any(|id| id == backing),
                format!(
                    "handler {} may not trigger on or read its own output",
                    handler.id
                ),
            )?;
        }
    }
    let unique_outputs: BTreeSet<_> = handler.outputs.iter().collect();
    require(
        unique_outputs.len() == handler.outputs.len(),
        format!("handler {} outputs must be distinct", handler.id),
    )?;
    for output_id in &handler.outputs {
        require(
            outputs.contains_key(output_id),
            format!("handler {} has unresolved output {output_id}", handler.id),
        )?;
    }
    let unique_egress: BTreeSet<_> = handler
        .egress
        .iter()
        .map(|host| (host.host.as_str(), host.port))
        .collect();
    require(
        unique_egress.len() == handler.egress.len(),
        format!("handler {} egress hosts must be distinct", handler.id),
    )?;
    for egress in &handler.egress {
        require(
            is_egress_host(&egress.host) && egress.port >= 1,
            format!(
                "handler {} egress host {} must be a lowercase DNS name (never an IP literal or wildcard), reached over HTTPS",
                handler.id, egress.host
            ),
        )?;
        egress_hosts.insert(egress.host.clone());
    }
    Ok(())
}

/// Build the handler-trigger graph (a handler's outputs feeding another
/// handler's inputs through an [`InputMode::Output`] input) and reject any
/// cycle, including a handler triggering on or reading its own output.
fn validate_handler_graph(
    ir: &Ir,
    inputs: &BTreeMap<String, &Input>,
    handler_ids: &BTreeSet<&str>,
) -> Result<()> {
    // Which handlers read each output through an Output-mode input.
    let mut consumers_of: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for handler in &ir.handlers {
        for input_id in &handler.inputs {
            let Some(input) = inputs.get(input_id) else {
                continue;
            };
            if input.mode == InputMode::Output
                && let Some(resource_id) = input.managed_resource_id.as_deref()
            {
                consumers_of
                    .entry(resource_id)
                    .or_default()
                    .insert(handler.id.as_str());
            }
        }
    }
    let mut edges: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for handler in &ir.handlers {
        for output_id in &handler.outputs {
            if let Some(consumers) = consumers_of.get(output_id.as_str()) {
                edges
                    .entry(handler.id.as_str())
                    .or_default()
                    .extend(consumers.iter().copied());
            }
        }
    }
    let mut visited = BTreeSet::new();
    for &handler_id in handler_ids {
        if !visited.contains(handler_id) {
            let mut path = BTreeSet::new();
            detect_handler_cycle(&edges, handler_id, &mut visited, &mut path)?;
        }
    }
    Ok(())
}

fn validate_handlers(
    ir: &Ir,
    inputs: &BTreeMap<String, &Input>,
    outputs: &BTreeMap<String, &Output>,
) -> Result<()> {
    require(
        ir.handlers.len() <= MAX_HANDLERS,
        format!("domain exceeds maxHandlers budget of {MAX_HANDLERS}"),
    )?;
    let mut handler_ids = BTreeSet::new();
    let mut egress_hosts = BTreeSet::new();
    for handler in &ir.handlers {
        require(
            handler_ids.insert(handler.id.as_str()),
            "duplicate handler id",
        )?;
        validate_handler_shape(ir, handler)?;
        validate_handler_edges(handler, inputs, outputs, &mut egress_hosts)?;
    }
    require(
        egress_hosts.len() <= MAX_EGRESS_HOSTS,
        format!("domain exceeds maxEgressHosts budget of {MAX_EGRESS_HOSTS}"),
    )?;
    validate_handler_graph(ir, inputs, &handler_ids)
}

fn derive_resources(
    ir: &Ir,
    requests: &mut Vec<Request>,
    effects: &mut Vec<Effect>,
    gaps: &mut BTreeSet<String>,
    risks: &mut BTreeSet<String>,
) -> Result<()> {
    for resource in &ir.resources {
        let api = resolve_resource_api(resource)?;
        if resource.api_resource.is_some() {
            risks.insert("opaque-custom-resource-effects".to_owned());
        }
        if resource.force {
            risks.insert(format!(
                "force-apply:{}/{}/{}",
                resource.kind, resource.namespace, resource.name
            ));
        }
        let mut paths = Vec::new();
        field_paths(&resource.desired, "", &mut paths);
        paths.sort();
        effects.push(Effect {
            target: resource.id.clone(),
            operation: "server-side-apply".into(),
            field_paths: paths,
            source: resource.source.clone(),
        });
        for verb in ["list", "patch"] {
            resolved_request(
                requests,
                "managed-target",
                &api.api_group,
                &api.plural,
                &resource.namespace,
                &resource.name,
                verb,
            );
        }
        resolved_request(
            requests,
            "managed-target-dry-run-apply",
            &api.api_group,
            &api.plural,
            &resource.namespace,
            &resource.name,
            "patch",
        );
        resolved_request(
            requests,
            "managed-target-apply-create-authorization",
            &api.api_group,
            &api.plural,
            &resource.namespace,
            &resource.name,
            "create",
        );
        resolved_request(
            requests,
            "managed-target-create-preflight",
            &api.api_group,
            &api.plural,
            &resource.namespace,
            &resource.name,
            "list",
        );
        resolved_request(
            requests,
            "managed-target-create-apply",
            &api.api_group,
            &api.plural,
            &resource.namespace,
            &resource.name,
            "patch",
        );
        gaps.insert("rbac-create-not-name-restricted".to_owned());
        gaps.insert("rbac-patch-not-field-restricted".to_owned());
        if resource.kind == "Deployment" {
            risks.insert("workload-template-modification".to_owned());
            risks.insert("workload-service-account-authority".to_owned());
            if let Some(containers) = resource
                .desired
                .pointer("/spec/template/spec/containers")
                .and_then(Value::as_array)
            {
                for container in containers {
                    if container["image"]
                        .as_str()
                        .is_some_and(|image| !immutable_image(image))
                    {
                        risks.insert("mutable-workload-image".to_owned());
                    }
                    if container_uses_secret(container) {
                        risks.insert("workload-secret-reference".to_owned());
                    }
                }
            }
        }
    }
    Ok(())
}

fn container_uses_secret(container: &Value) -> bool {
    container
        .get("envFrom")
        .and_then(Value::as_array)
        .is_some_and(|references| {
            references
                .iter()
                .any(|value| value.get("secretRef").is_some())
        })
        || container
            .get("env")
            .and_then(Value::as_array)
            .is_some_and(|environment| {
                environment
                    .iter()
                    .any(|value| value.pointer("/valueFrom/secretKeyRef").is_some())
            })
}

/// Requests and risks for every input that is read live rather than through
/// the reconciler's own applied state: `Observed` (external authority) and
/// `Output` (a handler's own written state, read the same way).
fn derive_input_capabilities(
    ir: &Ir,
    requests: &mut Vec<Request>,
    risks: &mut BTreeSet<String>,
) -> Result<()> {
    for input in &ir.inputs {
        if input.mode == InputMode::Observed {
            for verb in ["get", "list", "watch"] {
                request(
                    requests,
                    "external-input",
                    &input.kind,
                    &input.namespace,
                    &input.name,
                    verb,
                )?;
            }
            risks.insert("external-input-writers-influence-rollout".to_owned());
        }
        if input.mode == InputMode::Output {
            for verb in ["get", "list", "watch"] {
                request(
                    requests,
                    "handler-output-read",
                    &input.kind,
                    &input.namespace,
                    &input.name,
                    verb,
                )?;
            }
        }
        if input.sensitive {
            risks.insert("secret-read-authority".to_owned());
        }
    }
    Ok(())
}

fn derive_plan(ir: &Ir, inputs: &BTreeMap<String, &Input>) -> Result<CompiledPlan> {
    let domain = &ir.domain;
    let mut requests = Vec::new();
    let mut effects = Vec::new();
    let mut gaps = BTreeSet::new();
    let mut risks = BTreeSet::new();
    derive_resources(ir, &mut requests, &mut effects, &mut gaps, &mut risks)?;
    derive_input_capabilities(ir, &mut requests, &mut risks)?;
    for reaction in &ir.reactions {
        effects.push(Effect {
            target: reaction.target.clone(),
            operation: "rollout-contribution".into(),
            field_paths: vec!["/spec/template/metadata/annotations/apsides.dev~1revision".into()],
            source: reaction.source.clone(),
        });
    }
    request(
        &mut requests,
        "activation-check",
        "ConfigMap",
        &domain.control_namespace,
        &format!("apsides-activation-{}", domain.id),
        "get",
    )?;
    request(
        &mut requests,
        "platform-policy-check",
        "ConfigMap",
        &domain.control_namespace,
        &format!("apsides-policy-{}", domain.id),
        "get",
    )?;
    for verb in ["get", "update"] {
        request(
            &mut requests,
            "runtime-inventory",
            "ConfigMap",
            &domain.control_namespace,
            &format!("apsides-inventory-{}", domain.id),
            verb,
        )?;
    }
    gaps.insert("rbac-patch-not-field-restricted".to_owned());
    gaps.insert("rbac-update-not-field-restricted".to_owned());
    if ir
        .reactions
        .iter()
        .any(|reaction| reaction.inputs.iter().any(|id| inputs[id].sensitive))
    {
        request(
            &mut requests,
            "rollout-key",
            "Secret",
            &domain.control_namespace,
            &format!("apsides-rollout-key-{}", domain.id),
            "get",
        )?;
        risks.insert("rollout-key-read-authority".to_owned());
        gaps.insert("sensitive-revision-change-observable".to_owned());
    }
    let capabilities: BTreeSet<_> = requests
        .iter()
        .map(|request| request.capability.clone())
        .collect();
    let test_plan = test_plan(ir);
    Ok(CompiledPlan {
        ir_digest: digest(ir)?,
        ir: ir.clone(),
        effects,
        request_plan: requests,
        capabilities: capabilities.into_iter().collect(),
        enforcement_gaps: gaps.into_iter().collect(),
        indirect_risks: risks.into_iter().collect(),
        test_plan,
    })
}

fn immutable_image(image: &str) -> bool {
    image
        .rsplit_once("@sha256:")
        .is_some_and(|(prefix, digest)| {
            !prefix.is_empty()
                && digest.len() == 64
                && digest
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        })
}

fn test_plan(ir: &Ir) -> Vec<TestCase> {
    crate::test_plan::generate(ir)
}

/// Check derived authority against an independently supplied platform ceiling.
///
/// # Errors
/// Rejects forged plans, binding mismatches, wildcards, or unaccepted authority and risks.
pub fn validate_ceiling(plan: &CompiledPlan, ceiling: &PermissionCeiling) -> Result<()> {
    let actual = validate(&plan.ir)?;
    if &actual != plan {
        return Err(Error::Ceiling(
            "supplied plan differs from trusted derivation".into(),
        ));
    }
    let d = &plan.ir.domain;
    if ceiling.policy_ref != d.policy_ref
        || ceiling.domain_id != d.id
        || ceiling.cluster_binding != d.cluster_binding
        || ceiling.workload_namespace != d.namespace
        || ceiling.control_namespace != d.control_namespace
    {
        return Err(Error::Ceiling(
            "policy binding differs from domain, cluster or namespace".into(),
        ));
    }
    for capability in &ceiling.capabilities {
        if capability.api_group.contains('*')
            || capability.resource.contains('*')
            || capability.namespace.contains('*')
            || capability.verb.contains('*')
            || capability
                .subresource
                .as_ref()
                .is_some_and(|s| s.contains('*'))
            || capability.names.iter().any(|n| n.contains('*'))
        {
            return Err(Error::Ceiling("wildcards are unsupported".into()));
        }
    }
    for requested in &plan.capabilities {
        let covered = ceiling.capabilities.iter().any(|allowed| {
            allowed.namespace == requested.namespace
                && allowed.api_group == requested.api_group
                && allowed.resource == requested.resource
                && allowed.subresource == requested.subresource
                && allowed.verb == requested.verb
                && if requested.names.is_empty() {
                    allowed.names.is_empty()
                } else {
                    !allowed.names.is_empty()
                        && requested
                            .names
                            .iter()
                            .all(|name| allowed.names.contains(name))
                }
        });
        if !covered {
            return Err(Error::Ceiling(format!(
                "{} {}/{} in {} is not explicitly allowed",
                requested.verb, requested.api_group, requested.resource, requested.namespace
            )));
        }
    }
    if plan
        .enforcement_gaps
        .iter()
        .any(|gap| !ceiling.accepted_enforcement_gaps.contains(gap))
    {
        return Err(Error::Ceiling(
            "enforcement gap lacks explicit platform acceptance".into(),
        ));
    }
    if plan
        .indirect_risks
        .iter()
        .any(|risk| !ceiling.accepted_indirect_risks.contains(risk))
    {
        return Err(Error::Ceiling(
            "indirect privilege risk lacks explicit platform acceptance".into(),
        ));
    }
    Ok(())
}
