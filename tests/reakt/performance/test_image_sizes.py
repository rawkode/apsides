import copy
import hashlib
import json
import io
from pathlib import Path
import tarfile
import tempfile
import unittest

import image_sizes as sizes


def raw(value):
    return json.dumps(value, separators=(",", ":")).encode()


def digest(value):
    return "sha256:" + hashlib.sha256(value).hexdigest()


class ImageSizeTests(unittest.TestCase):
    def fixture(self):
        config = "sha256:" + "1" * 64
        layer = {"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": "sha256:" + "2" * 64, "size": 10}
        manifest = raw({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
                        "config": {"digest": config, "size": 5}, "layers": [layer]})
        index = {"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": [
            {"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": digest(manifest), "size": len(manifest),
             "platform": {"os": "linux", "architecture": "amd64"}}]}
        local = [{"Id": config, "Os": "linux", "Architecture": "amd64", "Size": 30,
                  "RootFS": {"Layers": ["sha256:" + "3" * 64]}}]
        return manifest, index, local

    def inspect(self, manifest, index, local):
        index_raw = raw(index)
        reference = "registry.example/app@" + digest(index_raw)
        calls = []
        def run(args):
            calls.append(args)
            if args[:3] == ["docker", "image", "inspect"]:
                return raw(local)
            return index_raw if args[-1] == reference else manifest
        result = sizes.inspect_image(reference, "linux/amd64", run)
        self.assertTrue(all("pull" not in c and "build" not in c for c in calls))
        return result

    def test_exact_platform_and_local_config_match(self):
        result = self.inspect(*self.fixture())
        self.assertEqual(result["declaredCompressedLayerBytes"], 10)
        self.assertEqual(result["dockerReportedSizeBytes"], 30)
        self.assertEqual(result["manifestDigest"], digest(self.fixture()[0]))

    def test_modern_manifest_and_index_descriptor_identity(self):
        for root in [False, True]:
            manifest, index, local = self.fixture()
            payload = raw(index) if root else manifest
            identity = digest(payload)
            local[0]["Id"] = identity
            local[0]["Descriptor"] = {"digest": identity, "size": len(payload),
                "mediaType": index["mediaType"] if root else json.loads(manifest)["mediaType"]}
            result = self.inspect(manifest, index, local)
            self.assertEqual(result["dockerIdentityKind"], "descriptor")
            self.assertIn("packed resources", result["dockerSizeSemantics"])
            local[0]["Descriptor"]["size"] += 1
            with self.assertRaises(ValueError): self.inspect(manifest, index, local)
            local[0]["Descriptor"]["size"] -= 1
            local[0]["Id"] = "sha256:" + "f" * 64
            with self.assertRaises(ValueError): self.inspect(manifest, index, local)

    def test_missing_or_ambiguous_platform_rejected(self):
        for duplicate in [False, True]:
            manifest, index, local = self.fixture()
            if duplicate:
                index["manifests"].append(copy.deepcopy(index["manifests"][0]))
            else:
                index["manifests"][0]["platform"]["architecture"] = "arm64"
            with self.assertRaises(ValueError):
                self.inspect(manifest, index, local)

    def test_wrong_manifest_size_digest_or_local_identity_rejected(self):
        for change in ["size", "digest", "architecture", "config", "diffids"]:
            manifest, index, local = self.fixture()
            if change == "size": index["manifests"][0]["size"] += 1
            if change == "digest": index["manifests"][0]["digest"] = "sha256:" + "9" * 64
            if change == "architecture": local[0]["Architecture"] = "arm64"
            if change == "config": local[0]["Id"] = "sha256:" + "9" * 64
            if change == "diffids": local[0]["RootFS"]["Layers"] = []
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.inspect(manifest, index, local)

    def test_shared_layers_deduplicated_unpacked_unknown(self):
        first = self.inspect(*self.fixture())
        second = {**first, "reference": "registry.example/alias@" + "sha256:" + "4" * 64}
        report = sizes.summarize([first, second])
        self.assertEqual(report["aggregate"]["uniqueCompressedLayerBytes"], 10)
        self.assertEqual(report["aggregate"]["uniqueLayerAndConfigBytes"], 15)
        self.assertIsNone(report["aggregate"]["deduplicatedUnpackedBytes"])
        self.assertFalse(report["completeSection26"])
        second = copy.deepcopy(second)
        second["layers"][0]["size"] = 11
        with self.assertRaises(ValueError): sizes.summarize([first, second])

    def test_mutable_reference_and_nonlinux_rejected_without_command(self):
        def fail(_): self.fail("unexpected command")
        for reference, platform in [("example:latest", "linux/amd64"), ("example@sha256:" + "1" * 64, "windows/amd64")]:
            with self.assertRaises(ValueError): sizes.inspect_image(reference, platform, fail)

    def test_local_archive_validates_blobs_without_registry_access(self):
        config, layer = raw({"architecture": "amd64", "os": "linux", "rootfs": {"diff_ids": ["sha256:" + "3" * 64]}}), b"compressed-layer"
        manifest = raw({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
                        "config": {"digest": digest(config), "size": len(config)},
                        "layers": [{"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip", "digest": digest(layer), "size": len(layer)}]})
        index = raw({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": [
            {"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": digest(manifest), "size": len(manifest),
             "platform": {"os": "linux", "architecture": "amd64"}}]})
        local = [{"Id": digest(config), "Os": "linux", "Architecture": "amd64", "Size": 30,
                  "RootFS": {"Layers": ["sha256:" + "3" * 64]}}]
        def run(args):
            self.assertEqual(args, ["docker", "image", "inspect", digest(manifest)])
            return raw(local)
        for corrupt in [None, "layer", "index", "duplicate"]:
            with tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "image.tar"
                documents = {"oci-layout": b'{"imageLayoutVersion":"1.0.0"}', "index.json": index,
                    "blobs/sha256/" + digest(manifest)[7:]: manifest,
                    "blobs/sha256/" + digest(config)[7:]: config,
                    "blobs/sha256/" + digest(layer)[7:]: layer}
                if corrupt == "layer": documents["blobs/sha256/" + digest(layer)[7:]] = b"changed-layer!!!"
                if corrupt == "index": documents["index.json"] = raw({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": []})
                with tarfile.open(path, "w") as archive:
                    for name, data in documents.items():
                        item = tarfile.TarInfo(name); item.size = len(data)
                        archive.addfile(item, io.BytesIO(data))
                    if corrupt == "duplicate":
                        item = tarfile.TarInfo("index.json"); item.size = len(index)
                        archive.addfile(item, io.BytesIO(index))
                if corrupt:
                    with self.subTest(corrupt=corrupt), self.assertRaises(ValueError):
                        sizes.inspect_image("localhost/app@" + digest(manifest), "linux/amd64", run, path)
                else:
                    result = sizes.inspect_image("localhost/app@" + digest(manifest), "linux/amd64", run, path)
                    self.assertEqual(result["manifestSource"], "local-oci-archive")
                    self.assertEqual(result["declaredCompressedLayerBytes"], len(layer))

    def test_direct_manifest_and_cli_trailing_newline(self):
        manifest, _, local = self.fixture()
        def run(args): return raw(local) if args[1] == "image" else manifest + b"\n"
        result = sizes.inspect_image("example@" + digest(manifest), "linux/amd64", run)
        self.assertEqual(result["manifestDigest"], digest(manifest))


if __name__ == "__main__":
    unittest.main()
