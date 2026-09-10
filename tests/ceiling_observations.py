"""Bounded, metadata-only observations for live permission-ceiling checks."""

from __future__ import annotations

import copy
from collections import Counter
import hashlib
import json
import math
from pathlib import Path
import sqlite3
from urllib.parse import quote


MAX_DATABASE_BYTES = 512 * 1024 * 1024
MAX_ROWS = 200_000
MAX_HASHED_BYTES = 256 * 1024 * 1024
MAX_SCHEMA_OBJECTS = 64
MAX_COLUMNS = 64

COORDINATION_TABLES = frozenset({
    "apsides_coordination_schema",
    "apsides_coordination_claims",
    "apsides_coordination_audits",
    "apsides_coordination_review_objects",
})
INVENTORY_TABLES = frozenset({
    "apsides_inventory_schema",
    "apsides_inventory_snapshots",
    "apsides_inventory_heads",
})
ALLOWED_TABLES = COORDINATION_TABLES | INVENTORY_TABLES


def _canonical(value: object) -> bytes:
    return json.dumps(
        value, ensure_ascii=False, separators=(",", ":"), sort_keys=True,
    ).encode("utf-8")


def _digest(value: object) -> str:
    return "sha256:" + hashlib.sha256(_canonical(value)).hexdigest()


def _typed(value: object) -> list[object]:
    if value is None:
        return ["null"]
    if isinstance(value, int) and not isinstance(value, bool):
        return ["integer", str(value)]
    if isinstance(value, float):
        if not math.isfinite(value):
            raise RuntimeError("SQLite snapshot contains a non-finite real")
        return ["real", value.hex()]
    if isinstance(value, str):
        return ["text", value]
    if isinstance(value, bytes):
        return ["blob", value.hex()]
    raise RuntimeError("SQLite snapshot contains an unsupported value type")


def _validate_path(path: Path) -> None:
    if not isinstance(path, Path) or not path.is_absolute():
        raise RuntimeError("SQLite snapshot path must be an absolute pathlib.Path")
    metadata = path.lstat()
    if path.is_symlink() or not path.is_file() or metadata.st_size > MAX_DATABASE_BYTES:
        raise RuntimeError("SQLite snapshot path must be a bounded regular file")


def _schema(connection: sqlite3.Connection) -> dict[str, list[dict[str, str]]]:
    rows = connection.execute(
        "SELECT type,name,tbl_name,sql FROM sqlite_schema "
        "WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name"
    ).fetchall()
    if len(rows) > MAX_SCHEMA_OBJECTS:
        raise RuntimeError("SQLite schema exceeds its object bound")
    tables = {row[1] for row in rows if row[0] == "table"}
    if not tables or not tables <= ALLOWED_TABLES:
        raise RuntimeError("SQLite database contains an unexpected table")
    for family in (COORDINATION_TABLES, INVENTORY_TABLES):
        present = tables & family
        if present and present != family:
            raise RuntimeError("SQLite database contains an incomplete schema family")
    result = {table: [] for table in tables}
    for kind, name, table, sql in rows:
        if kind not in {"table", "index", "trigger"} or table not in tables:
            raise RuntimeError("SQLite database contains an unexpected schema object")
        if not all(isinstance(value, str) and value for value in (kind, name, table, sql)):
            raise RuntimeError("SQLite database contains an invalid schema object")
        result[table].append({"type": kind, "name": name, "sql": sql})
    return result


def capture(
    path: Path,
) -> tuple[dict[str, dict[str, object]], dict[str, list[bytes]]]:
    """Capture public digests and private canonical rows from one transaction.

    The second return value can contain private metadata and must remain inside
    the runner. Only the first value is suitable for evidence output.
    """
    _validate_path(path)
    uri = f"file:{quote(str(path), safe='/')}?mode=ro"
    connection = sqlite3.connect(uri, uri=True, timeout=5)
    try:
        connection.execute("PRAGMA query_only=ON")
        connection.execute("BEGIN")
        schema = _schema(connection)
        result: dict[str, dict[str, object]] = {}
        private_rows: dict[str, list[bytes]] = {}
        total_rows = 0
        total_bytes = 0
        for table in sorted(schema):
            cursor = connection.execute(f'SELECT * FROM "{table}"')
            columns = [item[0] for item in cursor.description or ()]
            if not columns or len(columns) > MAX_COLUMNS or len(set(columns)) != len(columns):
                raise RuntimeError("SQLite table has invalid columns")
            row_digests = []
            canonical_rows = []
            for row in cursor:
                total_rows += 1
                if total_rows > MAX_ROWS:
                    raise RuntimeError("SQLite snapshot exceeds its row bound")
                encoded = _canonical({
                    "columns": columns,
                    "values": [_typed(value) for value in row],
                })
                total_bytes += len(encoded)
                if total_bytes > MAX_HASHED_BYTES:
                    raise RuntimeError("SQLite snapshot exceeds its byte bound")
                canonical_rows.append(encoded)
                row_digests.append("sha256:" + hashlib.sha256(encoded).hexdigest())
            result[table] = {
                "schemaDigest": _digest(schema[table]),
                "rowCount": len(row_digests),
                "rowDigests": sorted(row_digests),
            }
            private_rows[table] = sorted(canonical_rows)
        connection.rollback()
        return result, private_rows
    finally:
        connection.close()


def snapshot(path: Path) -> dict[str, dict[str, object]]:
    """Read one consistent SQLite snapshot without exposing stored values."""
    return capture(path)[0]


def _validated_table(value: object, table: str) -> tuple[str, int, Counter[str]]:
    if not isinstance(value, dict) or set(value) != {
        "schemaDigest", "rowCount", "rowDigests",
    }:
        raise RuntimeError(f"invalid public SQLite snapshot table: {table}")
    schema = value["schemaDigest"]
    count = value["rowCount"]
    digests = value["rowDigests"]
    valid_digest = lambda item: (isinstance(item, str) and len(item) == 71
                                 and item.startswith("sha256:")
                                 and all(char in "0123456789abcdef" for char in item[7:]))
    if (not valid_digest(schema)
            or isinstance(count, bool) or not isinstance(count, int) or count < 0
            or not isinstance(digests, list) or len(digests) != count
            or digests != sorted(digests)
            or any(not valid_digest(item) for item in digests)):
        raise RuntimeError(f"invalid public SQLite snapshot table: {table}")
    return schema, count, Counter(digests)


def validate_reservation_rows(
    before_public: dict[str, dict[str, object]],
    after_public: dict[str, dict[str, object]],
) -> None:
    """Require the exact metadata-store delta of one initial reservation."""
    if not isinstance(before_public, dict) or not isinstance(after_public, dict):
        raise RuntimeError("reservation snapshots must be objects")
    if set(before_public) != set(after_public):
        raise RuntimeError("reservation changed the SQLite table set")
    if not set(before_public) <= ALLOWED_TABLES:
        raise RuntimeError("reservation snapshot contains an unexpected table")
    tables = set(before_public)
    if not COORDINATION_TABLES <= tables:
        raise RuntimeError("reservation snapshot lacks the complete coordination schema")
    if tables & INVENTORY_TABLES and not INVENTORY_TABLES <= tables:
        raise RuntimeError("reservation snapshot has an incomplete inventory schema")
    appended = {"apsides_coordination_claims", "apsides_coordination_review_objects"}
    for table in sorted(before_public):
        before_schema, before_count, before_rows = _validated_table(
            before_public[table], table,
        )
        after_schema, after_count, after_rows = _validated_table(
            after_public[table], table,
        )
        if before_schema != after_schema:
            raise RuntimeError(f"reservation changed SQLite schema: {table}")
        if table in appended:
            retained = before_rows & after_rows
            additions = after_rows - before_rows
            if (after_count != before_count + 1 or retained != before_rows
                    or sum(additions.values()) != 1):
                raise RuntimeError(
                    f"reservation did not append exactly one retained row: {table}"
                )
        elif before_count != after_count or before_rows != after_rows:
            raise RuntimeError(f"reservation changed unrelated SQLite rows: {table}")


def exact_object_digest(obj: dict[str, object]) -> str:
    """Hash every supplied JSON field, including UID and resourceVersion."""
    if not isinstance(obj, dict):
        raise RuntimeError("Kubernetes observation must be an object")
    return _digest(obj)


def stable_object(obj: dict[str, object]) -> str:
    """Hash a Kubernetes object after excluding only server managedFields."""
    if not isinstance(obj, dict):
        raise RuntimeError("Kubernetes observation must be an object")
    value = copy.deepcopy(obj)
    metadata = value.get("metadata")
    if metadata is not None and not isinstance(metadata, dict):
        raise RuntimeError("Kubernetes object metadata must be an object")
    if isinstance(metadata, dict):
        metadata.pop("managedFields", None)
    return _digest(value)


def _required_string(value: dict[str, object], key: str, description: str) -> str:
    result = value.get(key)
    if not isinstance(result, str) or not result or any(ord(char) < 32 for char in result):
        raise RuntimeError(f"{description} lacks a valid {key}")
    return result


def validate_kind_target(
    node: dict[str, object],
    container: dict[str, object],
    expected_cluster: str,
    pinned_node_reference: str,
    pinned_image_id: str,
) -> dict[str, str]:
    """Validate matching Kubernetes and Docker identities for one kind node."""
    if not all(isinstance(value, dict) for value in (node, container)):
        raise RuntimeError("kind target observations must be objects")
    if not isinstance(expected_cluster, str) or not expected_cluster.startswith("apsides-qual-"):
        raise RuntimeError("invalid qualification cluster name")
    node_name = f"{expected_cluster}-control-plane"
    metadata = node.get("metadata")
    spec = node.get("spec")
    status = node.get("status")
    config = container.get("Config")
    if not all(isinstance(value, dict) for value in (metadata, spec, status, config)):
        raise RuntimeError("kind target observation is missing typed identity fields")
    node_uid = _required_string(metadata, "uid", "kind node")
    node_version = _required_string(metadata, "resourceVersion", "kind node")
    expected_provider = f"kind://docker/{expected_cluster}/{node_name}"
    conditions = status.get("conditions")
    ready = ([item for item in conditions
              if isinstance(item, dict) and item.get("type") == "Ready"]
             if isinstance(conditions, list) else [])
    labels = config.get("Labels")
    container_name = container.get("Name")
    container_id = container.get("Id")
    if (metadata.get("name") != node_name
            or metadata.get("deletionTimestamp") is not None
            or spec.get("providerID") != expected_provider
            or len(ready) != 1 or ready[0].get("status") != "True"
            or not isinstance(labels, dict)
            or labels.get("io.x-k8s.kind.cluster") != expected_cluster
            or labels.get("io.x-k8s.kind.role") != "control-plane"
            or container_name != f"/{node_name}"
            or config.get("Image") != pinned_node_reference
            or container.get("Image") != pinned_image_id
            or not isinstance(container_id, str) or not container_id):
        raise RuntimeError("kind node and Docker container identities do not match")
    if not isinstance(pinned_node_reference, str) or "@sha256:" not in pinned_node_reference:
        raise RuntimeError("kind node image reference is not digest pinned")
    if not isinstance(pinned_image_id, str) or not pinned_image_id.startswith("sha256:"):
        raise RuntimeError("kind node Docker image ID is invalid")
    return {
        "clusterName": expected_cluster,
        "nodeName": node_name,
        "nodeUid": node_uid,
        "nodeResourceVersion": node_version,
        "nodeProviderId": expected_provider,
        "containerName": container_name,
        "containerId": container_id,
        "nodeImageReference": pinned_node_reference,
        "nodeImageId": pinned_image_id,
    }
