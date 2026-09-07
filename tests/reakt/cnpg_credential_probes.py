"""Bounded credential probes; callers own credentials, live objects and cluster cleanup.

No receipt is produced. A successful HTTP suite leaves Secret revision observation
outstanding. Creates and SSA use signed desired objects; updates use live readbacks.
"""
import copy
import http.client
import json
import re
from urllib.parse import urlencode, urlsplit


SENSITIVE = "sensitive-revision-change-observable"
GAPS = {"rbac-create-not-name-restricted", "rbac-patch-not-field-restricted",
        "rbac-update-not-field-restricted", SENSITIVE}
VERSIONS = {"": "v1", "apps": "v1", "rbac.authorization.k8s.io": "v1",
            "postgresql.cnpg.io": "v1"}


def _path(group, namespace, resource, name=None):
    parts = [namespace, resource, *([name] if name else [])]
    if group not in VERSIONS or any(not re.fullmatch(r"[a-z0-9][a-z0-9.-]*", x) for x in parts):
        raise ValueError("unsupported probe API identity")
    prefix = "/api/v1" if not group else f"/apis/{group}/{VERSIONS[group]}"
    return f"{prefix}/namespaces/{namespace}/{resource}" + (f"/{name}" if name else "")


def desired_objects(plan):
    """Accept the four concrete, compiler-validated CNPG demo resource shapes."""
    supported = {("v1", "ConfigMap"): "configmaps", ("v1", "Service"): "services",
                 ("apps/v1", "Deployment"): "deployments", ("postgresql.cnpg.io/v1", "Cluster"): "clusters"}
    objects = {}
    for resource in plan["ir"]["resources"]:
        version, kind = resource["apiVersion"], resource["kind"]
        plural = supported.get((version, kind))
        body = resource.get("desired")
        if plural is None or not isinstance(body, dict):
            raise ValueError("unsupported concrete desired resource")
        if (body.get("apiVersion") != version or body.get("kind") != kind
                or body.get("metadata") != {"name": resource["name"], "namespace": resource["namespace"]}
                or set(body) != {"apiVersion", "kind", "metadata", "data" if kind == "ConfigMap" else "spec"}):
            raise ValueError("desired resource identity or shape is unsupported")
        group = version.split("/")[0] if "/" in version else ""
        key = (group, plural, resource["namespace"], resource["name"])
        _path(group, resource["namespace"], plural, resource["name"])
        if key in objects:
            raise ValueError("duplicate desired resource identity")
        value = body.get("data") if kind == "ConfigMap" else body.get("spec")
        if not isinstance(value, dict):
            raise ValueError("desired object must contain concrete fields")
        if kind == "ConfigMap" and any(not isinstance(v, str) for v in value.values()):
            raise ValueError("ConfigMap data must be concrete strings")
        if kind == "Cluster" and (resource.get("apiResource", {}).get("plural") != plural
                or resource.get("apiResource", {}).get("scope") != "Namespaced"):
            raise ValueError("Cluster descriptor differs from supported CRD")
        # JSON encoding rejects non-JSON values; the compiler and API own schema validation.
        json.dumps(body, allow_nan=False)
        objects[key] = copy.deepcopy(body)
    return objects


def plan_probes(plan, live_objects, probe_name="aps-credential-probe"):
    """Return requests and outstanding gaps; never interpret this as acceptance.

    live_objects keys are (group, resource, namespace, name). They are used only
    for dry-run requests and must not be serialized into evidence.
    """
    desired = desired_objects(plan)
    declared = set(plan["enforcementGaps"])
    unsupported = declared - GAPS
    probes = []
    unsupported_operations = []
    covered = set()
    seen = set()

    def add(label, method, path, expected, body=None, query=None, content_type="application/json"):
        probes.append({"id": label, "method": method, "path": path,
                       "query": query or {}, "expected": expected, "body": body, "contentType": content_type})

    for request in plan["requestPlan"]:
        c = request["capability"]
        if c not in plan["capabilities"] or c["verb"] not in {"get", "list", "watch", "create", "patch", "update"}:
            raise ValueError("request plan differs from supported declared capabilities")
        if (c["verb"] == "create") != (not c["names"]):
            raise ValueError("unsupported name scope for declared operation")
        key = json.dumps(c, sort_keys=True)
        if key in seen:
            continue
        seen.add(key)
        group, resource, namespace, verb = (c[k] for k in ["apiGroup", "resource", "namespace", "verb"])
        if c.get("subresource") or group not in VERSIONS:
            raise ValueError("unsupported declared probe capability")
        collection = _path(group, namespace, resource)
        for name in c["names"]:
            path = _path(group, namespace, resource, name)
            if verb in {"get", "list", "watch"}:
                query = {} if verb == "get" else {"fieldSelector": "metadata.name=" + name}
                if verb == "watch":
                    query.update(watch="true", timeoutSeconds="1")
                add(f"allowed-{len(probes)}", "GET", path if verb == "get" else collection, [200], query=query)
                if verb in {"list", "watch"}:
                    add(f"unfiltered-{len(probes)}", "GET", collection, [403],
                        query={"watch": "true", "timeoutSeconds": "1"} if verb == "watch" else {})
            elif verb in {"patch", "update"}:
                gap = "rbac-" + verb + "-not-field-restricted"
                if verb == "patch":
                    body = {"metadata": {"annotations": {"qualification.apsides.dev/credential-probe": "true"}}}
                    method = "PATCH"
                else:
                    current = live_objects.get((group, resource, namespace, name))
                    if current is None:
                        unsupported.add(gap)
                        continue
                    metadata = current.get("metadata", {})
                    if (metadata.get("name") != name or metadata.get("namespace") != namespace
                            or not isinstance(metadata.get("resourceVersion"), str) or not metadata["resourceVersion"]):
                        raise ValueError("update readback requires exact identity and resourceVersion")
                    body = copy.deepcopy(current)
                    body.pop("status", None)
                    body["metadata"].setdefault("annotations", {})["qualification.apsides.dev/credential-probe"] = "true"
                    method = "PUT"
                add(f"broader-{verb}-{len(probes)}", method, path, [200], body, {"dryRun": "All"},
                    "application/merge-patch+json" if method == "PATCH" else "application/json")
                covered.add(gap)
        if verb == "create":
            gap = "rbac-create-not-name-restricted"
            candidates = [obj for (g, r, ns, _), obj in desired.items()
                          if (g, r, ns) == (group, resource, namespace)]
            if not candidates:
                unsupported.add(gap)
                continue
            body = copy.deepcopy(candidates[0])
            body.pop("status", None)
            body["metadata"] = {"name": probe_name, "namespace": namespace}
            _path(group, namespace, resource, probe_name)
            add(f"arbitrary-create-{len(probes)}", "POST", collection, [201], body, {"dryRun": "All"})
            covered.add(gap)

    for (group, plural, namespace, name), body in desired.items():
        path = _path(group, namespace, plural, name)
        add(f"intended-ssa-{len(probes)}", "PATCH", path, [200], body,
            {"dryRun": "All", "fieldManager": "aps-credential-qualification"}, "application/apply-patch+yaml")

    namespace = plan["ir"]["domain"]["namespace"]
    control = plan["ir"]["domain"]["controlNamespace"]
    domain = plan["ir"]["domain"]["id"]
    for label, path in [
        ("outside-namespace", _path("", "kube-system", "secrets", probe_name)),
        ("outside-name", _path("", namespace, "secrets", probe_name)),
        ("rbac-read", _path("rbac.authorization.k8s.io", namespace, "roles")),
    ]:
        add(label, "GET", path, [403])
    add("unmanaged-delete", "DELETE", _path("", namespace, "configmaps", probe_name), [403], query={"dryRun": "All"})
    for prefix in ["reakt-trust-", "reakt-policy-"]:
        add(prefix + "mutation", "PATCH", _path("", control, "configmaps", prefix + domain), [403],
            {"metadata": {"annotations": {"qualification.apsides.dev/credential-probe": "true"}}}, {"dryRun": "All"}, "application/merge-patch+json")
    add("token-create", "POST", _path("", control, "serviceaccounts", "default") + "/token", [403],
        {"apiVersion": "authentication.k8s.io/v1", "kind": "TokenRequest", "spec": {"audiences": ["qualification"], "expirationSeconds": 600}}, {"dryRun": "All"})
    return {"requests": probes, "unsupportedGaps": sorted(unsupported | (declared - covered - {SENSITIVE})),
            "outstandingObservations": [SENSITIVE] if SENSITIVE in declared else [],
            "undeclaredGaps": sorted(covered - declared), "unsupportedOperations": unsupported_operations}


def execute_probes(specification, request_status, identity):
    """request_status(method,path,query,body,content_type) returns HTTP status only.

    identity is public metadata: exact candidate/generation/ServiceAccount UID.
    The runner must authenticate that binding separately; it is not inferred here.
    """
    required = {"releaseDigest", "generation", "serviceAccountUid"}
    if set(identity) != required or any(not isinstance(v, str) or not v for v in identity.values()):
        raise ValueError("probe identity requires exact public bindings")
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", identity["releaseDigest"]):
        raise ValueError("probe release digest is malformed")
    if any(not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9.-]{0,252}", identity[k])
           for k in ["generation", "serviceAccountUid"]):
        raise ValueError("probe identity contains invalid public identifier")
    results = []
    for probe in specification["requests"]:
        status = request_status(probe["method"], probe["path"], probe["query"], probe["body"], probe["contentType"])
        if type(status) is not int or not 100 <= status <= 599:
            raise ValueError("probe transport must return an HTTP status")
        results.append({k: probe[k] for k in ["id", "method", "path", "query", "contentType"]} |
                       {"status": status, "passed": status in probe["expected"]})
    return {"identity": dict(identity), "requests": results,
            "httpChecksPassed": all(r["passed"] for r in results) and bool(results),
            "unsupportedGaps": specification["unsupportedGaps"],
            "outstandingObservations": specification["outstandingObservations"],
            "undeclaredGaps": specification["undeclaredGaps"],
            "unsupportedOperations": specification["unsupportedOperations"]}


def https_status_transport(server, token, ssl_context):
    """Use verified TLS, no redirects; discard responses without decoding bodies."""
    import ssl
    parsed = urlsplit(server)
    if (parsed.scheme != "https" or not parsed.hostname or parsed.path not in {"", "/"}
            or parsed.username or parsed.password or parsed.query or parsed.fragment
            or ssl_context.verify_mode != ssl.CERT_REQUIRED or not ssl_context.check_hostname):
        raise ValueError("probe server requires verified HTTPS origin")

    def request(method, path, query, body, content_type):
        connection = http.client.HTTPSConnection(parsed.hostname, parsed.port, context=ssl_context, timeout=10)
        try:
            headers = {"Authorization": "Bearer " + token}
            if body is not None:
                headers["Content-Type"] = content_type
            connection.request(method, path + ("?" + urlencode(query) if query else ""),
                               body=json.dumps(body).encode() if body is not None else None, headers=headers)
            return connection.getresponse().status
        except (OSError, http.client.HTTPException, ValueError):
            raise RuntimeError("credential probe transport failed") from None
        finally:
            connection.close()
    return request
