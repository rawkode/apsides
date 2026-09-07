#!/usr/bin/env python3
"""Verify imported CNPG types through Deno's actual language server."""

import argparse
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
import time


CRD_URL = "https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/v1.30.0/config/crd/bases/postgresql.cnpg.io_clusters.yaml"


class EditorError(Exception):
    """The editor acceptance contract was not satisfied."""


class LanguageServer:
    def __init__(self, project: Path):
        self.messages = queue.Queue()
        self.next_id = 0
        self.process = subprocess.Popen(
            ["deno", "lsp"], cwd=project, stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        )
        self.reader = threading.Thread(target=self.read_messages, daemon=True)
        self.reader.start()
        self.settings = {"enable": True, "config": str(project / "deno.json")}

    def read_messages(self):
        try:
            while True:
                headers = {}
                while True:
                    line = self.process.stdout.readline()
                    if not line:
                        raise EditorError("Deno language server closed its output")
                    if line == b"\r\n":
                        break
                    name, value = line.decode("ascii").split(":", 1)
                    headers[name.lower()] = value.strip()
                size = int(headers["content-length"])
                if size < 0 or size > 8 * 1024 * 1024:
                    raise EditorError("Deno language-server message exceeds limit")
                data = self.process.stdout.read(size)
                if len(data) != size:
                    raise EditorError("Deno language-server message was truncated")
                self.messages.put(json.loads(data))
        except (EditorError, OSError, ValueError, KeyError) as error:
            self.messages.put(EditorError(str(error)))

    def send(self, message):
        data = json.dumps({"jsonrpc": "2.0", **message}).encode()
        self.process.stdin.write(f"Content-Length: {len(data)}\r\n\r\n".encode() + data)
        self.process.stdin.flush()

    def notify(self, method, params):
        self.send({"method": method, "params": params})

    def receive(self, deadline):
        try:
            message = self.messages.get(timeout=max(0, deadline - time.monotonic()))
        except queue.Empty as error:
            raise EditorError("Deno language-server response timed out") from error
        if isinstance(message, Exception):
            raise message
        if "method" in message and "id" in message:
            if message["method"] == "workspace/configuration":
                result = [self.settings for _ in message["params"]["items"]]
                self.send({"id": message["id"], "result": result})
            else:
                self.send({"id": message["id"], "result": None})
        return message

    def request(self, method, params):
        self.next_id += 1
        request_id = self.next_id
        self.send({"id": request_id, "method": method, "params": params})
        deadline = time.monotonic() + 30
        while True:
            message = self.receive(deadline)
            if message.get("id") == request_id and "method" not in message:
                if "error" in message:
                    raise EditorError(f"Deno rejected {method}: {message['error']}")
                return message.get("result")

    def diagnostics(self, uri):
        response = self.request("textDocument/diagnostic", {"textDocument": {"uri": uri}})
        if not isinstance(response, dict) or response.get("kind") != "full":
            raise EditorError("Expected a full Deno diagnostic report")
        items = response.get("items")
        if not isinstance(items, list):
            raise EditorError("Deno diagnostic report did not contain an item list")
        return items

    def close(self):
        try:
            self.request("shutdown", None)
            self.notify("exit", None)
            self.process.wait(timeout=5)
        except (EditorError, OSError, subprocess.TimeoutExpired):
            self.process.kill()
            self.process.wait(timeout=5)
        finally:
            self.process.stdin.close()
            self.process.stdout.close()
            self.reader.join(timeout=1)


def cursor(source):
    before, after = source.split("/*cursor*/")
    return before + after, {"line": before.count("\n"), "character": len(before.rsplit("\n", 1)[-1])}


IMPORT = 'import { Cluster, type ClusterSpec } from "./.aps/crds/cnpg.ts";\n'


def verify(project: Path):
    for name in ("deno.json", "tsconfig.json", ".aps/crds/cnpg.ts"):
        if not (project / name).is_file():
            raise EditorError(f"Initialized project is missing {name}")
    descriptor, filename = tempfile.mkstemp(prefix="aps-editor-probe-", suffix=".tsx", dir=project)
    os.close(descriptor)
    probe = Path(filename)
    uri = probe.as_uri()
    server = None
    try:
        server = LanguageServer(project)
        initialization = server.request("initialize", {
            "processId": os.getpid(), "rootUri": project.as_uri(),
            "workspaceFolders": [{"uri": project.as_uri(), "name": project.name}],
            "capabilities": {"workspace": {"configuration": True}, "textDocument": {
                "publishDiagnostics": {"versionSupport": True},
                "completion": {"completionItem": {"documentationFormat": ["markdown", "plaintext"]}},
            }},
            "initializationOptions": server.settings,
        })
        if not isinstance(initialization, dict):
            raise EditorError("Deno did not return initialization capabilities")
        server.notify("initialized", {})
        version = 0

        def document(source):
            nonlocal version
            version += 1
            probe.write_text(source)
            if version == 1:
                server.notify("textDocument/didOpen", {"textDocument": {
                    "uri": uri, "languageId": "typescriptreact", "version": version, "text": source,
                }})
            else:
                server.notify("textDocument/didChange", {
                    "textDocument": {"uri": uri, "version": version}, "contentChanges": [{"text": source}],
                })

        document(IMPORT + 'const spec: ClusterSpec = { instances: 1, storage: { size: "1Gi" } };\n'
                 + 'export default <Cluster metadata={{ name: "database", namespace: "default" }} spec={spec} />;\n')
        if server.diagnostics(uri) != []:
            raise EditorError("Valid imported CNPG component has editor diagnostics")

        completed = {}
        for name, source, expected in (
            ("spec", IMPORT + 'const spec: ClusterSpec = { /*cursor*/ };\n', {"instances", "storage", "imageName"}),
            ("storage", IMPORT + 'const spec: ClusterSpec = { storage: { /*cursor*/ } };\n', {"size", "storageClass", "resizeInUseVolumes"}),
            ("jsx-storage", IMPORT + 'const resource = <Cluster metadata={{ name: "database" }} spec={{ storage: { /*cursor*/ } }} />;\n', {"size", "storageClass"}),
        ):
            text, position = cursor(source)
            document(text)
            response = server.request("textDocument/completion", {
                "textDocument": {"uri": uri}, "position": position,
            })
            items = response.get("items", []) if isinstance(response, dict) else response or []
            labels = {item["label"].removesuffix("?") for item in items}
            if not expected <= labels or "apsDefinitelyNotAField" in labels:
                raise EditorError(f"{name} completions did not contain the expected CNPG properties: {sorted(labels)}")
            completed[name] = sorted(expected)

        text, position = cursor(IMPORT + 'const spec: ClusterSpec = { storage: { si/*cursor*/ze: "1Gi" } };\n')
        document(text)
        hover = server.request("textDocument/hover", {"textDocument": {"uri": uri}, "position": position})
        if "Size of the storage" not in json.dumps(hover):
            raise EditorError("CNPG storage documentation was missing from hover")

        document(IMPORT + 'const spec: ClusterSpec = { apsDefinitelyNotAField: true };\n')
        if not any(str(item.get("code")) == "2353" for item in server.diagnostics(uri)):
            raise EditorError("Unknown CNPG field was not diagnosed")
        document(IMPORT + 'const spec: ClusterSpec = { instances: "wrong" };\n')
        if not any(str(item.get("code")) == "2322" for item in server.diagnostics(uri)):
            raise EditorError("Wrong CNPG property type was not diagnosed")
        server.notify("textDocument/didClose", {"textDocument": {"uri": uri}})
        return {
            "status": "passed", "server": initialization.get("serverInfo"),
            "completions": completed, "hoverDocumentation": True,
            "diagnostics": {"validDocument": [], "unknownField": 2353, "wrongType": 2322},
        }
    finally:
        if server is not None:
            server.close()
        probe.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--project", type=Path)
    source.add_argument("--aps", type=Path, help="Initialize and fetch CNPG in a temporary project")
    args = parser.parse_args()
    try:
        if args.project is not None:
            result = verify(args.project.resolve())
        else:
            aps = args.aps.resolve(strict=True)
            with tempfile.TemporaryDirectory(prefix="aps-authoring-editor-") as directory:
                project = Path(directory) / "project"
                subprocess.run([str(aps), "init", "--directory", str(project)],
                               check=True, timeout=120, stdout=subprocess.DEVNULL)
                subprocess.run([str(aps), "crds", "fetch", "--url", CRD_URL,
                                "--kind", "Cluster", "--version", "v1", "--output", ".aps/crds/cnpg.ts"],
                               cwd=project, check=True, timeout=120, stdout=subprocess.DEVNULL)
                result = verify(project.resolve())
        print(json.dumps(result, indent=2))
    except (EditorError, OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"Editor acceptance failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
