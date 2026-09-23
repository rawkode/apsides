#!/usr/bin/env python3
"""Complete the compiler closure from source receipts and a verified native context."""
import argparse
import hashlib
import json
from pathlib import Path
from package_oci import ROOT, read_regular, saved_image, verify_context
from toolchain_snapshot import public as public_tools, verify as verify_tools


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


RUNTIME_PINS = ROOT / "scripts/runtime-base"


def pinned_blob(pins, value, size=None):
    """Exact registry bytes committed under scripts/runtime-base, by digest."""
    if not isinstance(value, str) or not value.startswith("sha256:") or len(value) != 71:
        raise ValueError("runtime base pins require SHA-256 descriptors")
    path = pins / "blobs/sha256" / value[7:]
    if not path.is_file():
        raise ValueError("the runtime base is not pinned; run `cuenv task runtimeBasePin`")
    data = read_regular(path)
    if digest(data) != value or (size is not None and len(data) != size):
        raise ValueError("runtime base pin differs from its digest")
    return data


def runtime_closure(archive, runtime, pins=RUNTIME_PINS):
    """Verify, offline, the pinned base index -> platform manifest -> saved config chain.

    The index and manifest are the registry bytes committed as pins; the
    config is what `docker image save` of the pinned base holds, in whichever
    format the local image store writes. The saved image must be the one
    `docker image inspect` matched to the pinned digest: the classic store
    names an image by its config digest, the containerd store by the index it
    pulled.
    """
    reference, platform = runtime["reference"], runtime["platform"]
    index_raw = pinned_blob(pins, reference.rsplit("@", 1)[1])
    index = json.loads(index_raw)
    if index.get("mediaType") != "application/vnd.oci.image.index.v1+json":
        raise ValueError("pinned runtime base must be an OCI image index")
    os_name, architecture = platform.split("/")
    matches = [item for item in index.get("manifests", [])
               if item.get("platform", {}).get("os") == os_name and item.get("platform", {}).get("architecture") == architecture]
    if len(matches) != 1:
        raise ValueError("pinned runtime base index must select exactly one platform manifest")
    manifest_raw = pinned_blob(pins, matches[0]["digest"], matches[0]["size"])
    manifest = json.loads(manifest_raw)
    if manifest.get("mediaType") != "application/vnd.oci.image.manifest.v1+json":
        raise ValueError("pinned runtime base platform entry must be an OCI manifest")
    image = saved_image(archive, platform)
    if runtime["localImageId"] not in {image["root"], digest(image["config"])}:
        raise ValueError("saved runtime base differs from the inspected pinned image")
    if (manifest["config"]["digest"] != digest(image["config"])
            or manifest["config"]["size"] != len(image["config"])):
        raise ValueError("saved runtime base config differs from the pinned manifest")
    layers = manifest.get("layers", [])
    if len(layers) != len(image["layers"]):
        raise ValueError("saved runtime base layers differ from the pinned manifest")
    for pinned, saved in zip(layers, image["layers"]):
        # An uncompressed saved layer is named by its diff ID, already checked
        # against the config; a compressed one must be the registry blob.
        if saved["mediaType"] == pinned["mediaType"] and saved["digest"] != pinned["digest"]:
            raise ValueError("saved runtime base layer differs from the pinned manifest")
    return {"baseImage": {"reference": reference, "platform": platform},
            "indexJson": index_raw.decode("utf-8"), "manifestJson": manifest_raw.decode("utf-8"),
            "configJson": image["config"].decode("utf-8")}


PACKAGING_SOURCES = [
    "scripts/Dockerfile",
    "scripts/build.py",
    "scripts/extract_binary.py",
    "scripts/finalize_inputs.py",
    "scripts/package_oci.py",
    "scripts/sbom.py",
    "scripts/toolchain_snapshot.py",
]


def finalize(source, context, runtime_base, packaging_inputs, profile, tool_snapshot, output):
    if output.exists():
        raise ValueError("final build-input output already exists")
    metadata=verify_context(context)
    packaging_bytes = read_regular(packaging_inputs)
    packaging = json.loads(packaging_bytes)
    if packaging["contextDigest"] != digest(read_regular(context / "context.json")) or packaging["builder"] != metadata["builder"] or packaging["irDigest"] != metadata["irDigest"]:
        raise ValueError("packaging inputs differ from the prepared compiler context")
    profile_bytes = read_regular(profile)
    build_profile = json.loads(profile_bytes)
    expected_profile_fields = {"schemaVersion", "builderImage", "runtimeImage", "platform", "sbomTool", "sbomToolSha256", "sourceDateEpoch"}
    if set(build_profile) != expected_profile_fields or build_profile["schemaVersion"] != "apsides.dev/build-profile/v1":
        raise ValueError("unsupported build profile in compiler closure")
    host_tools = public_tools(tool_snapshot)
    sbom_identity = next((tool for tool in host_tools if tool["role"] == "sbom"), None)
    if sbom_identity is None or build_profile["sbomToolSha256"] != sbom_identity["sha256"]:
        raise ValueError("build profile SBOM tool differs from captured executable bytes")
    if (build_profile["builderImage"] != metadata["builder"]["reference"]
            or build_profile["runtimeImage"] != packaging["runtime"]["reference"]
            or build_profile["platform"] != metadata["builder"]["platform"]
            or build_profile["sourceDateEpoch"] != metadata["sourceDateEpoch"]):
        raise ValueError("build profile differs from the executed packaging inputs")
    inputs=json.loads(read_regular(source))
    closure=inputs["compilerClosure"]
    if closure["nativeCompiler"] is not None:
        raise ValueError("source receipt already has a native compiler closure")
    build_context_files=[{"path":path,"sha256":sha} for path,sha in metadata["files"].items()]
    packaging_files=[]
    for relative in PACKAGING_SOURCES:
        packaging_files.append({"path":relative,"sha256":digest(read_regular(ROOT/relative))})
    cli_root=ROOT/"crates/cli"
    for path in sorted([cli_root/"Cargo.toml",*list((cli_root/"src").rglob("*.rs"))]):
        entry={"path":path.relative_to(ROOT).as_posix(),"sha256":digest(read_regular(path))}
        packaging_files.append(entry)
    packaging_files.sort(key=lambda item:item["path"])
    cli=[entry for entry in packaging_files if entry["path"].startswith("crates/cli/")]
    cli.sort(key=lambda item:item["path"])
    closure["schemaVersion"]="apsides.dev/compiler-closure/v2"
    closure["nativeCompiler"]={
        "builderImage":{key:metadata["builder"][key] for key in ["reference","platform"]},
        "cargoLockSha256":metadata["controllerLockDigest"],
        "buildContextFiles":build_context_files,"packagingFiles":packaging_files,
        "buildProfileSha256":digest(profile_bytes),"hostTools":host_tools,
        "trustedCliCodeSha256":digest(canonical(cli)),
        "approvedRuntime":runtime_closure(runtime_base, packaging["runtime"]),
    }
    inputs["compilerClosureDigest"]=digest(canonical(closure))
    if verify_context(context)!=metadata:
        raise ValueError("prepared compiler context changed during closure finalization")
    if read_regular(packaging_inputs) != packaging_bytes:
        raise ValueError("packaging metadata changed during finalization")
    sbom_path = Path(build_profile["sbomTool"])
    if not sbom_path.is_absolute():
        sbom_path = profile.parent / sbom_path
    verify_tools(tool_snapshot, sbom_path, Path(tool_snapshot["resolvedPaths"]["apsides"]))
    output.parent.mkdir(parents=True,exist_ok=True)
    output.write_bytes(canonical(inputs))
    return inputs["compilerClosureDigest"]


if __name__ == "__main__":
    try:
        parser=argparse.ArgumentParser(description=__doc__)
        parser.add_argument("--source",type=Path,required=True)
        parser.add_argument("--context",type=Path,required=True)
        parser.add_argument("--packaging-inputs",type=Path,required=True)
        parser.add_argument("--runtime-base",type=Path,required=True)
        parser.add_argument("--profile",type=Path,required=True)
        parser.add_argument("--tool-snapshot",type=Path,required=True)
        parser.add_argument("--output",type=Path,required=True)
        args=parser.parse_args()
        snapshot=json.loads(read_regular(args.tool_snapshot))
        print(finalize(args.source,args.context,args.runtime_base,args.packaging_inputs,args.profile,snapshot,args.output))
    except Exception:
        raise SystemExit("apsides: build input finalization failed") from None
