#!/usr/bin/env python3
"""Disposable Envoy admission/drain canary. No Kubernetes credentials are used."""

import argparse
import concurrent.futures
import copy
import http.client
import http.server
import json
import os
import shutil
import socketserver
import subprocess
import threading
import time
from pathlib import Path

def container_engine():
    """podman when it is on PATH, Docker otherwise, unless APSIDES_CONTAINER_ENGINE names one.

    Both map host.docker.internal to the host, which the Envoy upstream uses.
    """
    engine = os.environ.get("APSIDES_CONTAINER_ENGINE") or ("podman" if shutil.which("podman") else "docker")
    if engine not in ("podman", "docker"):
        raise SystemExit("APSIDES_CONTAINER_ENGINE must be podman or docker")
    return engine


ENGINE = container_engine()
IMAGE = "envoyproxy/envoy@sha256:57e14a549d7bd43c8d3f6d03e8cfa653e037d4b38e133acd9b54f38c524401b4"
STATS_PATH = "/stats?format=json&filter=%5E%28server%5C.live%7Chttp%5C.apsides%5C.downstream_rq_active%7Ccluster%5C.write_api%5C.upstream_rq_%28active%7Ctotal%29%29%24"
REMOVE = [
    "authorization",
    "x-envoy-retry-on",
    "x-envoy-retry-grpc-on",
    "x-envoy-max-retries",
    "x-envoy-upstream-rq-timeout-ms",
    "x-envoy-upstream-rq-per-try-timeout-ms",
    "x-envoy-hedge-on-per-try-timeout",
    "x-envoy-upstream-rq-timeout-alt-response",
    "x-envoy-expected-rq-timeout-ms",
]


def base_config(upstream_port):
    return {
        "node": {"id": "apsides-canary", "cluster": "apsides-canary"},
        "admin": {"address": {"pipe": {"path": "/evidence/admin.sock", "mode": 384}}},
        "static_resources": {
            "listeners": [
                {
                    "name": "gate",
                    "address": {
                        "socket_address": {"address": "0.0.0.0", "port_value": 8080}
                    },
                    "filter_chains": [
                        {
                            "filters": [
                                {
                                    "name": "envoy.filters.network.http_connection_manager",
                                    "typed_config": {
                                        "@type": "type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager",
                                        "stat_prefix": "apsides",
                                        "codec_type": "HTTP1",
                                        "stream_idle_timeout": "5s",
                                        "request_timeout": "5s",
                                        "route_config": {
                                            "name": "api",
                                            "virtual_hosts": [
                                                {
                                                    "name": "api",
                                                    "domains": ["*"],
                                                    "routes": [
                                                        {
                                                            "match": {"prefix": "/"},
                                                            "route": {
                                                                "cluster": "api",
                                                                "timeout": "4s",
                                                            },
                                                        }
                                                    ],
                                                }
                                            ],
                                        },
                                        "http_filters": [
                                            {
                                                "name": "envoy.filters.http.health_check",
                                                "typed_config": {
                                                    "@type": "type.googleapis.com/envoy.extensions.filters.http.health_check.v3.HealthCheck",
                                                    "pass_through_mode": True,
                                                },
                                            },
                                            {
                                                "name": "envoy.filters.http.header_mutation",
                                                "typed_config": {
                                                    "@type": "type.googleapis.com/envoy.extensions.filters.http.header_mutation.v3.HeaderMutation",
                                                    "mutations": {
                                                        "request_mutations": [
                                                            {"remove": header}
                                                            for header in REMOVE
                                                        ]
                                                    },
                                                },
                                            },
                                            {
                                                "name": "envoy.filters.http.credential_injector",
                                                "typed_config": {
                                                    "@type": "type.googleapis.com/envoy.extensions.filters.http.credential_injector.v3.CredentialInjector",
                                                    "allow_request_without_credential": False,
                                                    "overwrite": True,
                                                    "credential": {
                                                        "name": "envoy.http.injected_credentials.generic",
                                                        "typed_config": {
                                                            "@type": "type.googleapis.com/envoy.extensions.http.injected_credentials.generic.v3.Generic",
                                                            "credential": {
                                                                "name": "api-token",
                                                                "sds_config": {
                                                                    "path_config_source": {
                                                                        "path": "/evidence/token-sds.json",
                                                                        "watched_directory": {
                                                                            "path": "/evidence"
                                                                        },
                                                                    },
                                                                    "resource_api_version": "V3",
                                                                },
                                                            },
                                                            "header_value_prefix": "Bearer ",
                                                        },
                                                    },
                                                },
                                            },
                                            {
                                                "name": "envoy.filters.http.router",
                                                "typed_config": {
                                                    "@type": "type.googleapis.com/envoy.extensions.filters.http.router.v3.Router"
                                                },
                                            },
                                        ],
                                        "access_log": [
                                            {
                                                "name": "envoy.access_loggers.file",
                                                "typed_config": {
                                                    "@type": "type.googleapis.com/envoy.extensions.access_loggers.file.v3.FileAccessLog",
                                                    "path": "/evidence/access.jsonl",
                                                    "log_format": {
                                                        "json_format": {
                                                            "method": "%REQ(:METHOD)%",
                                                            "code": "%RESPONSE_CODE%",
                                                            "details": "%RESPONSE_CODE_DETAILS%",
                                                            "flags": "%RESPONSE_FLAGS%",
                                                            "attempts": "%UPSTREAM_REQUEST_ATTEMPT_COUNT%",
                                                            "upstream": "%UPSTREAM_HOST%",
                                                        }
                                                    },
                                                },
                                            }
                                        ],
                                    },
                                }
                            ]
                        }
                    ],
                }
            ],
            "clusters": [
                {
                    "name": "api",
                    "type": "STRICT_DNS",
                    "connect_timeout": "1s",
                    "load_assignment": {
                        "cluster_name": "api",
                        "endpoints": [
                            {
                                "lb_endpoints": [
                                    {
                                        "endpoint": {
                                            "address": {
                                                "socket_address": {
                                                    "address": "host.docker.internal",
                                                    "port_value": upstream_port,
                                                }
                                            }
                                        }
                                    }
                                ]
                            }
                        ],
                    },
                }
            ],
            "secrets": [],
        },
    }


def config(upstream_port):
    result = base_config(upstream_port)
    hcm = result["static_resources"]["listeners"][0]["filter_chains"][0]["filters"][0][
        "typed_config"
    ]
    method = {
        "name": ":method",
        "string_match": {
            "safe_regex": {"google_re2": {}, "regex": "^(POST|PUT|PATCH|DELETE)$"}
        },
    }
    routes = hcm["route_config"]["virtual_hosts"][0]["routes"]
    writes = copy.deepcopy(routes[0])
    writes["match"]["headers"] = [method]
    writes["route"]["cluster"] = "write_api"
    routes[0]["route"]["cluster"] = "read_api"
    routes.insert(0, writes)
    hcm["access_log"][0]["filter"] = {"header_filter": {"header": method}}
    clusters = result["static_resources"]["clusters"]
    read = copy.deepcopy(clusters[0])
    read["name"] = "read_api"
    read["load_assignment"]["cluster_name"] = "read_api"
    clusters[0]["name"] = "write_api"
    clusters[0]["load_assignment"]["cluster_name"] = "write_api"
    clusters.append(read)
    return result


CURL_IMAGE = "curlimages/curl@sha256:d9b4541e214bcd85196d6e92e2753ac6d0ea699f0af5741f8c6cccbfcf00ef4b"


def admin(root, path, method="GET"):
    return subprocess.check_output(
        [
            ENGINE,
            "run",
            "--rm",
            "--network",
            "none",
            "--user",
            "0",
            "-v",
            f"{root}:/evidence",
            CURL_IMAGE,
            "--silent",
            "--show-error",
            "--fail",
            "--max-time",
            "2",
            "--unix-socket",
            "/evidence/admin.sock",
            "-X",
            method,
            "http://localhost" + path,
        ],
        stderr=subprocess.DEVNULL,
        timeout=5,
    )


def request(port, path="/write", headers=None):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=8)
    connection.request("POST", path, body=b"canary", headers=headers or {})
    response = connection.getresponse()
    data = response.read()
    connection.close()
    return response.status, data.decode()


def wait_until(predicate, seconds=10):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            if predicate():
                return
        except (OSError, AssertionError, subprocess.CalledProcessError):
            pass
        time.sleep(0.05)
    raise AssertionError("bounded canary wait expired")


def ledger(root):
    path = root / "access.jsonl"
    return (
        [json.loads(line) for line in path.read_text().splitlines()]
        if path.exists()
        else []
    )


def stats(root):
    raw = admin(root, STATS_PATH)
    (root / "filtered-stats.json").write_bytes(raw)
    def reject_fraction(value):
        raise AssertionError("filtered Envoy statistics contained a fractional/unsupported number: " + value)
    parsed = json.loads(raw, parse_float=reject_fraction, parse_constant=reject_fraction)
    allowed = {"server.live", "http.apsides.downstream_rq_active", "cluster.write_api.upstream_rq_active", "cluster.write_api.upstream_rq_total"}
    assert {item["name"] for item in parsed["stats"]} == allowed
    assert len(parsed["stats"]) == len(allowed)
    return {
        item["name"]: item["value"]
        for item in parsed["stats"]
        if "name" in item and "value" in item
    }


def validate_complete(entries, snapshot):
    if (
        snapshot.get("http.apsides.downstream_rq_active") != 0
        or snapshot.get("cluster.write_api.upstream_rq_active") != 0
    ):
        return False
    forwarded = [entry for entry in entries if entry["upstream"] not in ("-", None)]
    if len(forwarded) != snapshot.get("cluster.write_api.upstream_rq_total"):
        return False
    for entry in forwarded:
        if (
            entry["details"] != "via_upstream"
            or entry["flags"] != "-"
            or int(entry["attempts"]) != 1
        ):
            return False
        code = int(entry["code"])
        if not (
            200 <= code < 300
            or code in (400, 401, 403, 404, 405, 406, 409, 410, 413, 415, 422, 429)
        ):
            return False
    return True


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    root = Path(args.output).resolve()
    root.mkdir(parents=True, exist_ok=False)
    received = []
    entered = threading.Event()
    release = threading.Event()
    completed = threading.Event()
    simulated_scale_zero = threading.Event()
    lock = threading.Lock()

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            self.rfile.read(int(self.headers.get("content-length", "0")))
            with lock:
                received.append(
                    {
                        "path": self.path,
                        "authorization": self.headers.get("authorization"),
                        "retry": self.headers.get("x-envoy-retry-on"),
                    }
                )
            if self.path == "/slow":
                entered.set()
                release.wait(6)
            code = 504 if self.path == "/ambiguous" else 200
            self.send_response(code)
            self.send_header("Content-Length", "2")
            self.end_headers()
            try:
                self.wfile.write(b"ok")
                self.wfile.flush()
                if self.path == "/slow":
                    completed.set()
            except (BrokenPipeError, ConnectionResetError):
                pass

        def log_message(self, *args):
            pass

    class FixtureServer(http.server.ThreadingHTTPServer):
        def server_bind(self):
            socketserver.TCPServer.server_bind(self)
            self.server_name = "localhost"
            self.server_port = self.socket.getsockname()[1]

    upstream = FixtureServer(("0.0.0.0", 0), Handler)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    (root / "token").write_text("canary-only-not-a-real-credential")
    (root / "token-sds.json").write_text(
        json.dumps(
            {
                "version_info": "1",
                "resources": [
                    {
                        "@type": "type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.Secret",
                        "name": "api-token",
                        "generic_secret": {"secret": {"filename": "/evidence/token"}},
                    }
                ],
            }
        )
    )
    (root / "envoy.json").write_text(json.dumps(config(upstream.server_port)))
    docker = [ENGINE]
    name = "apsides-envoy-canary-" + str(os.getpid())
    started = False
    try:
        subprocess.run(
            docker
            + [
                "run",
                "--rm",
                "--network",
                "none",
                "-v",
                f"{root}:/evidence",
                IMAGE,
                "--mode",
                "validate",
                "-c",
                "/evidence/envoy.json",
            ],
            check=True,
            stdout=(root / "validate.log").open("w"),
            stderr=subprocess.STDOUT,
            timeout=30,
        )
        subprocess.run(
            docker
            + [
                "run",
                "-d",
                "--name",
                name,
                "-p",
                "127.0.0.1::8080",
                "-v",
                f"{root}:/evidence",
                IMAGE,
                "-c",
                "/evidence/envoy.json",
                "--concurrency",
                "2",
                "--log-level",
                "warning",
                "--file-flush-interval-msec",
                "50",
            ],
            check=True,
            capture_output=True,
            timeout=30,
        )
        started = True
        mapping = subprocess.check_output(
            docker + ["port", name, "8080/tcp"], text=True
        ).strip()
        port = int(mapping.rsplit(":", 1)[1])
        wait_until(lambda: admin(root, "/ready") == b"LIVE\n")
        with concurrent.futures.ThreadPoolExecutor() as pool:
            pending = pool.submit(
                request,
                port,
                "/slow",
                {
                    "Authorization": "attacker-value",
                    "x-envoy-retry-on": "5xx",
                    "x-envoy-max-retries": "9",
                },
            )
            assert entered.wait(5), "slow upstream never entered"
            (root / "unfiltered-stats.json").write_bytes(admin(root, "/stats?format=json"))
            before = stats(root)
            assert before["cluster.write_api.upstream_rq_active"] == 1
            admin(root, "/healthcheck/fail", "POST")
            blocked = [pool.submit(request, port, "/write") for _ in range(10)]
            assert all(f.result()[0] == 503 for f in blocked)
            during = stats(root)
            assert during["cluster.write_api.upstream_rq_active"] == 1
            # Match the preliminary Rust scheduling check. No scale-to-zero
            # request may be issued while an admitted write remains upstream.
            def schedule_client_stop(snapshot):
                assert snapshot["server.live"] == 0
                if snapshot["cluster.write_api.upstream_rq_active"] != 0:
                    return False
                assert completed.is_set(), "client stop raced the slow response"
                simulated_scale_zero.set()
                return True

            assert not schedule_client_stop(during)
            assert not simulated_scale_zero.is_set()
            release.set()
            assert pending.result()[0] == 200
            wait_until(lambda: schedule_client_stop(stats(root)))
            assert simulated_scale_zero.is_set()
        wait_until(lambda: len(ledger(root)) == 11)
        after = stats(root)
        entries = ledger(root)
        assert validate_complete(entries, after)
        assert (
            len(received) == 1
            and received[0]["authorization"]
            == "Bearer canary-only-not-a-real-credential"
            and received[0]["retry"] is None
        )
        # Standard SDS watches referenced secret files and observes an atomic token rotation.
        (root / "next-token").write_text("rotated-canary-not-a-real-credential")
        (root / "next-token").replace(root / "token")
        admin(root, "/healthcheck/ok", "POST")

        def rotated():
            assert request(port, "/rotation")[0] == 200
            return (
                received[-1]["authorization"]
                == "Bearer rotated-canary-not-a-real-credential"
            )

        wait_until(rotated)
        admin(root, "/healthcheck/fail", "POST")
        wait_until(lambda: len(ledger(root)) == len(received) + 10)
        assert validate_complete(ledger(root), stats(root))
        # An upstream timeout response looks idle but is not trusted completion.
        admin(root, "/healthcheck/ok", "POST")
        assert request(port, "/ambiguous")[0] == 504
        admin(root, "/healthcheck/fail", "POST")
        wait_until(lambda: len(ledger(root)) == len(received) + 10)
        ambiguous = stats(root)
        assert not validate_complete(ledger(root), ambiguous)
        evidence = {
            "image": IMAGE,
            "checks": {
                "all_match_freeze_rejected_new_writes": True,
                "inflight_visible_until_complete": True,
                "client_scale_zero_scheduled_only_after_slow_response": True,
                "credential_overwrite_and_retry_stripping": True,
                "idle_with_upstream_504_rejected": True,
                "atomic_projected_token_rotation_observed": True,
                "exact_admin_filter_omits_fractional_histogram_metadata": True,
            },
            "before": before,
            "during": during,
            "after": after,
            "ambiguous": ambiguous,
            "received": received,
            "ledger": ledger(root),
            "qualification": "canary-only; not a Kubernetes hard-drain authorization",
        }
        (root / "result.json").write_text(json.dumps(evidence, indent=2) + "\n")
        print(json.dumps({"result": "passed", "evidence": str(root / "result.json")}))
    finally:
        release.set()
        upstream.shutdown()
        if started:
            logs = subprocess.run(
                docker + ["logs", name],
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
            )
            (root / "container.log").write_text(logs.stdout + logs.stderr)
            subprocess.run(
                docker + ["rm", "-f", name],
                check=False,
                capture_output=True,
                timeout=20,
            )


if __name__ == "__main__":
    main()
