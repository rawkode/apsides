#!/usr/bin/env python3
"""Capture and recheck executable bytes used by the native packaging pipeline."""
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys


SCHEMA = "reakt.dev/host-tool-snapshot/v1"
ROLES = ("buildx", "cargo", "docker", "python", "reakt", "sbom")


def _digest(path):
    return "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()


def _run(argv):
    result = subprocess.run(argv, check=False, capture_output=True, text=True, timeout=30)
    if result.returncode:
        raise ValueError(f"cannot identify host tool {argv[0]}: {result.stderr[-1000:]}")
    value = " ".join(result.stdout.split())
    if not value or len(value) > 4096:
        raise ValueError(f"host tool {argv[0]} returned an invalid version")
    return value


def _executable(value):
    selected = shutil.which(value) if "/" not in value else value
    if selected is None:
        raise ValueError(f"host tool is unavailable: {value}")
    invocation = Path(selected).absolute()
    identity = invocation.resolve()
    if not identity.is_file():
        raise ValueError(f"host tool is not a regular file: {value}")
    return {"invocation": invocation, "identity": identity}


def _buildx(docker):
    raw = _run([str(docker["invocation"]), "info", "--format", "{{json .ClientInfo.Plugins}}"])
    try:
        plugins = json.loads(raw)
    except json.JSONDecodeError as error:
        raise ValueError("Docker did not report its resolved client plugins") from error
    matches = [item for item in plugins if item.get("Name") == "buildx" and item.get("Path")]
    if len(matches) != 1:
        raise ValueError("Docker must resolve exactly one Buildx client plugin")
    return _executable(matches[0]["Path"])


def _selection(sbom_tool, cli):
    docker = _executable("docker")
    return {
        "buildx": _buildx(docker),
        "cargo": _executable("cargo"),
        "docker": docker,
        "python": _executable(sys.executable),
        "reakt": _executable(str(cli)),
        "sbom": _executable(str(sbom_tool)),
    }


def _version(role, path):
    invocation = path["invocation"]
    if role == "buildx":
        return _run([str(invocation), "version"])
    if role == "python":
        return _run([str(invocation), "--version"])
    if role == "sbom":
        return _run([str(invocation), "cyclonedx", "--version"])
    return _run([str(invocation), "--version"])


def capture(sbom_tool, cli):
    selected = _selection(sbom_tool, cli)
    tools = [
        {"role": role, "sha256": _digest(selected[role]["identity"]), "version": _version(role, selected[role])}
        for role in ROLES
    ]
    return {
        "schemaVersion": SCHEMA,
        "tools": tools,
        "resolvedPaths": {role: str(selected[role]["identity"]) for role in ROLES},
        "invocationPaths": {role: str(selected[role]["invocation"]) for role in ROLES},
    }


def public(snapshot):
    if (snapshot.get("schemaVersion") != SCHEMA
            or sorted(snapshot.get("resolvedPaths", {})) != list(ROLES)
            or sorted(snapshot.get("invocationPaths", {})) != list(ROLES)):
        raise ValueError("invalid host tool snapshot")
    return snapshot["tools"]


def verify(snapshot, sbom_tool, cli):
    current = capture(sbom_tool, cli)
    if current != snapshot:
        raise ValueError("host tool selection, bytes, or version changed during native assembly")
