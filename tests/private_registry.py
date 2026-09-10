"""Owned Distribution registry for private-pull qualification; credentials stay temporary."""
import base64
import hashlib
import http.client
import json
from pathlib import Path
import re
import secrets
import ssl
import subprocess
import tempfile
import time
import tomllib

REGISTRY_IMAGE = "registry@sha256:1be55279f18a2fe1a74edf2664cac61c1bea305b7b4642dab412e7affdcb3e33"
ORAS_SHA256 = "50ae33461ea6e2746b774587d2cb060850961ce01bfd6e70b48d4267919b491c"
SECRET_NAME = "qualification-registry"


class RegistryError(RuntimeError):
    """Bounded diagnostics without registry credentials or subprocess output."""


def command(args, data=None, timeout=30):
    try:
        result = subprocess.run([str(arg) for arg in args], input=data,
                                capture_output=True, timeout=timeout)
    except (OSError, subprocess.TimeoutExpired):
        raise RegistryError("registry command unavailable or timed out") from None
    if result.returncode:
        raise RegistryError("registry command failed")
    return result.stdout


def inventory(kind):
    return sorted(command(["docker", kind, "ls", "-q", *(["--no-trunc", "-a"] if kind == "container" else [])]).decode().splitlines())


def auth_config(host, password):
    token = base64.b64encode(("qualification:" + password).encode()).decode()
    return {"auths": {host: {"auth": token}}}


class PrivateRegistry:
    def __init__(self, oras):
        self.oras = Path(oras).resolve()
        self.owner = secrets.token_hex(16)
        self.name = "aps-private-" + self.owner
        self.host = self.name + ":5000"
        self.container_id = None
        self.temporary = None
        self.publishedrefs = {}
        self.cleanup_verified = False

    def __enter__(self):
        self.docker_context = command(["docker", "context", "show"]).decode().strip()
        self.before_containers = inventory("container")
        self.before_volumes = inventory("volume")
        try:
            self.start()
            return self
        except BaseException:
            self.close()
            raise

    def start(self):
        if hashlib.sha256(self.oras.read_bytes()).hexdigest() != ORAS_SHA256:
            raise RegistryError("ORAS binary differs from qualification pin")
        image = json.loads(command(["docker", "image", "inspect", REGISTRY_IMAGE]))
        if len(image) != 1 or REGISTRY_IMAGE not in image[0]["RepoDigests"]:
            raise RegistryError("registry image differs from qualification pin")
        self.image_id = image[0]["Id"]
        self.temporary = tempfile.TemporaryDirectory(prefix="aps-private-registry-")
        self.directory = Path(self.temporary.name)
        self.directory.chmod(0o700)
        self.ca_path = self.directory / "cert.pem"
        self.password = secrets.token_urlsafe(32)
        command(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                 "-subj", "/CN=localhost", "-addext",
                 f"subjectAltName=DNS:localhost,DNS:{self.name},IP:127.0.0.1",
                 "-keyout", self.directory / "key.pem", "-out", self.ca_path])
        command(["/usr/sbin/htpasswd", "-B", "-i", "-c", self.directory / "htpasswd", "qualification"],
                (self.password + "\n").encode())
        for name in ("key.pem", "htpasswd"):
            (self.directory / name).chmod(0o600)
        storage = self.directory / "storage"
        storage.mkdir(mode=0o700)
        self.container_id = command([
            "docker", "create", "--name", self.name, "--label", "apsides.test.owner=" + self.owner,
            "--pull=never", "--publish", "127.0.0.1::5000",
            "--mount", f"type=bind,src={self.directory},dst=/qualification,readonly",
            "--mount", f"type=bind,src={storage},dst=/var/lib/registry",
            "--env", "REGISTRY_HTTP_TLS_CERTIFICATE=/qualification/cert.pem",
            "--env", "REGISTRY_HTTP_TLS_KEY=/qualification/key.pem",
            "--env", "REGISTRY_AUTH=htpasswd", "--env", "REGISTRY_AUTH_HTPASSWD_REALM=qualification",
            "--env", "REGISTRY_AUTH_HTPASSWD_PATH=/qualification/htpasswd", REGISTRY_IMAGE,
        ]).decode().strip()
        self.require_owned()
        command(["docker", "start", self.container_id])
        state = json.loads(command(["docker", "inspect", self.container_id]))[0]
        port = int(state["NetworkSettings"]["Ports"]["5000/tcp"][0]["HostPort"])
        self.local_host = f"localhost:{port}"
        self.client_config = self.directory / "client.json"
        self.client_config.touch(mode=0o600)
        self.client_config.write_text(json.dumps(auth_config(self.local_host, self.password)))
        context = ssl.create_default_context(cafile=str(self.ca_path))
        absent = None
        for _ in range(20):
            try:
                absent = self.status(port, context)
                break
            except (OSError, http.client.HTTPException):
                time.sleep(0.25)
        invalid = self.status(port, context, "invalid")
        valid = self.status(port, context, self.password)
        untrusted = False
        try:
            self.status(port, ssl.create_default_context())
        except ssl.SSLCertVerificationError:
            untrusted = True
        if (absent, invalid, valid, untrusted) != (401, 401, 200, True):
            raise RegistryError("registry TLS/authentication preflight failed")

    @staticmethod
    def status(port, context, password=None):
        client = http.client.HTTPSConnection("localhost", port, context=context, timeout=3)
        try:
            headers = {} if password is None else {"Authorization": "Basic " + auth_config("unused", password)["auths"]["unused"]["auth"]}
            client.request("GET", "/v2/", headers=headers)
            response = client.getresponse()
            if len(response.read(4097)) > 4096:
                raise RegistryError("registry response exceeds bound")
            return response.status
        finally:
            client.close()

    def require_owned(self):
        if not self.container_id or not re.fullmatch(r"[a-f0-9]{64}", self.container_id):
            raise RegistryError("registry container identity unavailable")
        state = json.loads(command(["docker", "inspect", self.container_id]))[0]
        if (self.container_id in self.before_containers or state["Name"] != "/" + self.name
                or state["Image"] != self.image_id
                or state["Config"]["Labels"].get("apsides.test.owner") != self.owner):
            raise RegistryError("registry container ownership mismatch")

    def publish(self, archive, repository, expected_digest):
        if (not re.fullmatch(r"[a-z0-9]+(?:[-/][a-z0-9]+)*", repository)
                or not re.fullmatch(r"sha256:[a-f0-9]{64}", expected_digest)):
            raise RegistryError("registry publication target invalid")
        local_ref = self.local_host + "/" + repository
        command([self.oras, "cp", "--from-oci-layout", "--to-plain-http=false", "--to-ca-file", self.ca_path,
                 "--to-registry-config", self.client_config, str(Path(archive).resolve()) + "@" + expected_digest,
                 local_ref + ":qualification"], timeout=180)
        raw = command([self.oras, "manifest", "fetch", "--plain-http=false", "--ca-file", self.ca_path,
                       "--registry-config", self.client_config, local_ref + "@" + expected_digest])
        if "sha256:" + hashlib.sha256(raw).hexdigest() != expected_digest:
            raise RegistryError("published manifest digest differs from archive")
        manifest = json.loads(raw)
        config = manifest["config"]["digest"]
        if not re.fullmatch(r"sha256:[a-f0-9]{64}", config):
            raise RegistryError("published image configuration digest invalid")
        reference = self.host + "/" + repository + "@" + expected_digest
        self.publishedrefs[reference] = {"manifestDigest": expected_digest, "configDigest": config}
        return reference

    def install_node_trust(self, runner):
        if runner.docker_context != self.docker_context:
            raise RegistryError("registry and kind use different Docker contexts")
        node = runner.container_id
        if not re.fullmatch(r"[a-f0-9]{64}", node) or node in self.before_containers:
            raise RegistryError("private-pull node is not a fresh owned container")
        self.require_owned()
        command(["docker", "network", "connect", "kind", self.container_id])
        config = tomllib.loads(command(["docker", "exec", node, "containerd", "config", "dump"]).decode())
        registry = config.get("plugins", {}).get("io.containerd.cri.v1.images", {}).get("registry", {})
        if "/etc/containerd/certs.d" not in registry.get("config_path", "").split(":"):
            raise RegistryError("kind containerd registry trust directory unavailable")
        target = "/etc/containerd/certs.d/" + self.host
        command(["docker", "exec", node, "mkdir", "-p", target])
        command(["docker", "exec", "-i", node, "tee", target + "/ca.crt"], self.ca_path.read_bytes())
        hosts = f'server = "https://{self.host}"\n[host."https://{self.host}"]\n  capabilities = ["pull", "resolve"]\n  ca = "{target}/ca.crt"\n'
        command(["docker", "exec", "-i", node, "tee", target + "/hosts.toml"], hosts.encode())

    def secret_object(self, namespace, valid):
        if not re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", namespace) or type(valid) is not bool:
            raise RegistryError("registry Secret scope invalid")
        data = json.dumps(auth_config(self.host, self.password if valid else "invalid")).encode()
        return {"apiVersion": "v1", "kind": "Secret", "metadata": {"name": SECRET_NAME, "namespace": namespace},
                "type": "kubernetes.io/dockerconfigjson", "data": {".dockerconfigjson": base64.b64encode(data).decode()}}

    def close(self):
        try:
            if self.container_id is None:
                matches = command(["docker", "ps", "-aq", "--no-trunc", "--filter", "label=apsides.test.owner=" + self.owner]).decode().splitlines()
                if len(matches) > 1:
                    raise RegistryError("registry cleanup identity ambiguous")
                self.container_id = matches[0] if matches else None
            if self.container_id:
                self.require_owned()
                command(["docker", "rm", "-fv", self.container_id])
                self.container_id = None
        finally:
            if self.temporary is not None:
                self.temporary.cleanup()
                self.password = None
        if inventory("container") != self.before_containers or inventory("volume") != self.before_volumes:
            raise RegistryError("registry cleanup did not restore inventories")
        self.cleanup_verified = True

    def __exit__(self, *_):
        self.close()
