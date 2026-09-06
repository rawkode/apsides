import copy
from contextlib import contextmanager
import importlib.util
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("ceiling_observations.py")
SPEC = importlib.util.spec_from_file_location("ceiling_observations", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("ceiling observation helper could not be imported")
OBSERVATIONS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(OBSERVATIONS)


SCHEMA = """
CREATE TABLE reakt_coordination_schema
 (id INTEGER PRIMARY KEY CHECK(id=1), version INTEGER NOT NULL);
CREATE TABLE reakt_coordination_claims
 (digest TEXT PRIMARY KEY NOT NULL, metadata BLOB NOT NULL);
CREATE TABLE reakt_coordination_audits
 (registry_uid TEXT NOT NULL, cluster_binding TEXT NOT NULL,
  control_namespace TEXT NOT NULL, workload_namespace TEXT NOT NULL,
  domain_id TEXT NOT NULL, transition_id TEXT NOT NULL, sequence TEXT NOT NULL,
  digest TEXT NOT NULL, metadata BLOB NOT NULL,
  PRIMARY KEY(registry_uid, cluster_binding, control_namespace,
              workload_namespace, domain_id, transition_id));
CREATE TABLE reakt_coordination_review_objects
 (registry_uid TEXT NOT NULL, cluster_binding TEXT NOT NULL,
  control_namespace TEXT NOT NULL, workload_namespace TEXT NOT NULL,
  domain_id TEXT NOT NULL, transition_id TEXT NOT NULL, sequence TEXT NOT NULL,
  digest TEXT NOT NULL, metadata BLOB NOT NULL,
  PRIMARY KEY(registry_uid, cluster_binding, control_namespace,
              workload_namespace, domain_id, transition_id, digest));
INSERT INTO reakt_coordination_schema VALUES (1, 1);
INSERT INTO reakt_coordination_claims VALUES ('private-digest', x'736563726574');
"""


@contextmanager
def open_database(path):
    connection = sqlite3.connect(path)
    try:
        with connection:
            yield connection
    finally:
        connection.close()


def database(root):
    path = root / "coordination.db"
    with open_database(path) as connection:
        connection.executescript(SCHEMA)
    return path


def target():
    cluster = "reakt-qual-0123456789abcdef0123456789abcdef"
    node_name = cluster + "-control-plane"
    reference = "kindest/node@sha256:" + "a" * 64
    image_id = "sha256:" + "b" * 64
    node = {
        "metadata": {"name": node_name, "uid": "node-uid", "resourceVersion": "7"},
        "spec": {"providerID": f"kind://docker/{cluster}/{node_name}"},
        "status": {"conditions": [{"type": "Ready", "status": "True"}]},
    }
    container = {
        "Id": "container-id",
        "Name": "/" + node_name,
        "Image": image_id,
        "Config": {"Image": reference, "Labels": {
            "io.x-k8s.kind.cluster": cluster,
            "io.x-k8s.kind.role": "control-plane",
        }},
    }
    return cluster, reference, image_id, node, container


class CeilingObservationTests(unittest.TestCase):
    def test_capture_returns_private_typed_rows_separately(self):
        with tempfile.TemporaryDirectory() as directory:
            path = database(Path(directory))
            public, private = OBSERVATIONS.capture(path)
            self.assertEqual(public, OBSERVATIONS.snapshot(path))
            claim_rows = private["reakt_coordination_claims"]
            self.assertEqual(len(claim_rows), 1)
            self.assertIn(b'"blob","736563726574"', claim_rows[0])
            self.assertNotIn("private-digest", json.dumps(public))

    def test_sqlite_snapshot_detects_rows_insertions_and_schema_changes(self):
        with tempfile.TemporaryDirectory() as directory:
            path = database(Path(directory))
            first = OBSERVATIONS.snapshot(path)
            with open_database(path) as connection:
                connection.execute(
                    "UPDATE reakt_coordination_claims SET metadata=x'6368616e676564'"
                )
            changed = OBSERVATIONS.snapshot(path)
            self.assertNotEqual(
                first["reakt_coordination_claims"]["rowDigests"],
                changed["reakt_coordination_claims"]["rowDigests"],
            )
            with open_database(path) as connection:
                connection.execute(
                    "INSERT INTO reakt_coordination_claims VALUES (?, ?)",
                    ("second", b"value"),
                )
            inserted = OBSERVATIONS.snapshot(path)
            self.assertEqual(inserted["reakt_coordination_claims"]["rowCount"], 2)
            with open_database(path) as connection:
                connection.execute(
                    "CREATE INDEX claims_metadata ON reakt_coordination_claims(metadata)"
                )
            indexed = OBSERVATIONS.snapshot(path)
            self.assertNotEqual(
                inserted["reakt_coordination_claims"]["schemaDigest"],
                indexed["reakt_coordination_claims"]["schemaDigest"],
            )

    def test_sqlite_snapshot_is_read_only_bounded_and_discloses_no_values(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = database(root)
            before = (path.stat().st_mtime_ns, sorted(item.name for item in root.iterdir()))
            result = OBSERVATIONS.snapshot(path)
            after = (path.stat().st_mtime_ns, sorted(item.name for item in root.iterdir()))
            self.assertEqual(before, after)
            self.assertNotIn("private-digest", json.dumps(result))
            self.assertNotIn("secret", json.dumps(result))
            with self.assertRaisesRegex(RuntimeError, "absolute"):
                OBSERVATIONS.snapshot(Path("relative.db"))
            link = root / "linked.db"
            link.symlink_to(path)
            with self.assertRaisesRegex(RuntimeError, "regular file"):
                OBSERVATIONS.snapshot(link)

    def test_sqlite_snapshot_rejects_unknown_and_incomplete_schemas(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            unknown = root / "unknown.db"
            with open_database(unknown) as connection:
                connection.execute("CREATE TABLE unrelated(value TEXT)")
            with self.assertRaisesRegex(RuntimeError, "unexpected table"):
                OBSERVATIONS.snapshot(unknown)
            incomplete = root / "incomplete.db"
            with open_database(incomplete) as connection:
                connection.execute(
                    "CREATE TABLE reakt_inventory_schema(id INTEGER PRIMARY KEY, version INTEGER)"
                )
            with self.assertRaisesRegex(RuntimeError, "incomplete schema"):
                OBSERVATIONS.snapshot(incomplete)

    def test_reservation_rows_accept_exact_claim_and_review_append(self):
        with tempfile.TemporaryDirectory() as directory:
            path = database(Path(directory))
            before = OBSERVATIONS.snapshot(path)
            with open_database(path) as connection:
                connection.execute(
                    "INSERT INTO reakt_coordination_claims VALUES (?, ?)",
                    ("second", b"claim"),
                )
                connection.execute(
                    "INSERT INTO reakt_coordination_review_objects VALUES "
                    "(?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    ("registry", "cluster", "control", "workload", "domain",
                     "transition", "1", "review", b"review"),
                )
            after = OBSERVATIONS.snapshot(path)
            self.assertIsNone(OBSERVATIONS.validate_reservation_rows(before, after))

    def test_reservation_rows_reject_deletion_unrelated_rows_and_schema_drift(self):
        with tempfile.TemporaryDirectory() as directory:
            path = database(Path(directory))
            baseline = OBSERVATIONS.snapshot(path)
            valid = copy.deepcopy(baseline)
            for table in ["reakt_coordination_claims", "reakt_coordination_review_objects"]:
                valid[table]["rowCount"] += 1
                valid[table]["rowDigests"].append("sha256:" + "f" * 64)
                valid[table]["rowDigests"].sort()

            deleted = copy.deepcopy(valid)
            deleted["reakt_coordination_claims"]["rowDigests"].remove(
                baseline["reakt_coordination_claims"]["rowDigests"][0]
            )
            deleted["reakt_coordination_claims"]["rowDigests"].append(
                "sha256:" + "e" * 64
            )
            deleted["reakt_coordination_claims"]["rowDigests"].sort()
            with self.assertRaisesRegex(RuntimeError, "exactly one retained row"):
                OBSERVATIONS.validate_reservation_rows(baseline, deleted)

            unrelated = copy.deepcopy(valid)
            unrelated["reakt_coordination_schema"]["rowCount"] += 1
            unrelated["reakt_coordination_schema"]["rowDigests"].append(
                "sha256:" + "d" * 64
            )
            unrelated["reakt_coordination_schema"]["rowDigests"].sort()
            with self.assertRaisesRegex(RuntimeError, "unrelated SQLite rows"):
                OBSERVATIONS.validate_reservation_rows(baseline, unrelated)

            wrong_table = copy.deepcopy(valid)
            wrong_table["unrelated"] = copy.deepcopy(
                wrong_table["reakt_coordination_schema"]
            )
            with self.assertRaisesRegex(RuntimeError, "table set"):
                OBSERVATIONS.validate_reservation_rows(baseline, wrong_table)

            schema = copy.deepcopy(valid)
            schema["reakt_coordination_audits"]["schemaDigest"] = "sha256:" + "c" * 64
            with self.assertRaisesRegex(RuntimeError, "changed SQLite schema"):
                OBSERVATIONS.validate_reservation_rows(baseline, schema)

    def test_object_digests_retain_uid_resource_version_and_all_but_managed_fields(self):
        value = {"metadata": {
            "uid": "uid", "resourceVersion": "7", "managedFields": [{"manager": "server"}],
        }, "data": {"state.json": "private"}}
        changed_fields = copy.deepcopy(value)
        changed_fields["metadata"]["managedFields"] = [{"manager": "other"}]
        self.assertNotEqual(
            OBSERVATIONS.exact_object_digest(value),
            OBSERVATIONS.exact_object_digest(changed_fields),
        )
        self.assertEqual(OBSERVATIONS.stable_object(value), OBSERVATIONS.stable_object(changed_fields))
        changed_version = copy.deepcopy(value)
        changed_version["metadata"]["resourceVersion"] = "8"
        self.assertNotEqual(OBSERVATIONS.stable_object(value), OBSERVATIONS.stable_object(changed_version))
        with self.assertRaisesRegex(RuntimeError, "must be an object"):
            OBSERVATIONS.exact_object_digest([])

    def test_kind_target_returns_identity_metadata_only(self):
        cluster, reference, image_id, node, container = target()
        identity = OBSERVATIONS.validate_kind_target(
            node, container, cluster, reference, image_id,
        )
        self.assertEqual(identity["nodeUid"], "node-uid")
        self.assertEqual(identity["nodeImageId"], image_id)
        self.assertNotIn("Labels", json.dumps(identity))
        self.assertNotIn("conditions", json.dumps(identity))

    def test_kind_target_rejects_missing_replaced_and_ambiguous_identity(self):
        cluster, reference, image_id, node, container = target()
        mutations = {
            "wrong-type": lambda n, c: n.update(metadata=[]),
            "missing-uid": lambda n, c: n["metadata"].pop("uid"),
            "deleting": lambda n, c: n["metadata"].update(deletionTimestamp="now"),
            "duplicate-ready": lambda n, c: n["status"]["conditions"].append(
                {"type": "Ready", "status": "True"}
            ),
            "provider": lambda n, c: n["spec"].update(providerID="kind://docker/other/node"),
            "cluster-label": lambda n, c: c["Config"]["Labels"].update(
                {"io.x-k8s.kind.cluster": "other"}
            ),
            "image-reference": lambda n, c: c["Config"].update(Image="kindest/node:latest"),
            "image-id": lambda n, c: c.update(Image="sha256:" + "c" * 64),
        }
        for name, mutate in mutations.items():
            with self.subTest(name=name):
                changed_node, changed_container = copy.deepcopy(node), copy.deepcopy(container)
                mutate(changed_node, changed_container)
                with self.assertRaises(RuntimeError):
                    OBSERVATIONS.validate_kind_target(
                        changed_node, changed_container, cluster, reference, image_id,
                    )


if __name__ == "__main__":
    unittest.main()
