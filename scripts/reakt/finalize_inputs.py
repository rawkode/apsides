#!/usr/bin/env python3
"""Complete the compiler closure from source receipts and a verified native context."""
import argparse
import hashlib
import json
import tarfile
from pathlib import Path
from package_oci import ROOT, read_regular, verify_context
from toolchain_snapshot import public as public_tools, verify as verify_tools


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def runtime_closure(archive, reference, platform):
    """Preserve the digest-verified OCI index/manifest/config chain from Docker save."""
    with tarfile.open(archive) as contents:
        members = {}
        for member in contents:
            if member.isfile():
                if member.name in members:
                    raise ValueError("duplicate runtime base archive member")
                members[member.name] = member
        def blob(sha, size=None):
            if not sha.startswith("sha256:") or len(sha) != 71:
                raise ValueError("runtime base requires SHA-256 OCI descriptors")
            member = members.get("blobs/sha256/" + sha[7:])
            if member is None or member.size > 1024 * 1024:
                raise ValueError("runtime base metadata missing or oversized")
            data = contents.extractfile(member).read()
            if digest(data) != sha or (size is not None and len(data) != size):
                raise ValueError("runtime base OCI metadata digest mismatch")
            return data.decode("utf-8")
        index_raw = blob(reference.rsplit("@", 1)[1])
        index = json.loads(index_raw)
        os_name, architecture = platform.split("/")
        matches = [item for item in index["manifests"] if item.get("platform", {}).get("os") == os_name and item.get("platform", {}).get("architecture") == architecture]
        if len(matches) != 1:
            raise ValueError("runtime base index must select exactly one platform")
        descriptor = matches[0]
        manifest_raw = blob(descriptor["digest"], descriptor["size"])
        manifest = json.loads(manifest_raw)
        config = manifest["config"]
        config_raw = blob(config["digest"], config["size"])
        return {"baseImage": {"reference": reference, "platform": platform},
                "indexJson": index_raw, "manifestJson": manifest_raw, "configJson": config_raw}


PACKAGING_SOURCES = [
    "scripts/reakt/Dockerfile",
    "scripts/reakt/build.py",
    "scripts/reakt/extract_binary.py",
    "scripts/reakt/finalize_inputs.py",
    "scripts/reakt/package_oci.py",
    "scripts/reakt/sbom.py",
    "scripts/reakt/toolchain_snapshot.py",
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
    if set(build_profile) != expected_profile_fields or build_profile["schemaVersion"] != "reakt.dev/build-profile/v1":
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
    closure["schemaVersion"]="reakt.dev/compiler-closure/v2"
    closure["nativeCompiler"]={
        "builderImage":{key:metadata["builder"][key] for key in ["reference","platform"]},
        "cargoLockSha256":metadata["controllerLockDigest"],
        "buildContextFiles":build_context_files,"packagingFiles":packaging_files,
        "buildProfileSha256":digest(profile_bytes),"hostTools":host_tools,
        "trustedCliCodeSha256":digest(canonical(cli)),
        "approvedRuntime":runtime_closure(runtime_base, packaging["runtime"]["reference"], packaging["runtime"]["platform"]),
    }
    inputs["compilerClosureDigest"]=digest(canonical(closure))
    if verify_context(context)!=metadata:
        raise ValueError("prepared compiler context changed during closure finalization")
    if read_regular(packaging_inputs) != packaging_bytes:
        raise ValueError("packaging metadata changed during finalization")
    sbom_path = Path(build_profile["sbomTool"])
    if not sbom_path.is_absolute():
        sbom_path = profile.parent / sbom_path
    verify_tools(tool_snapshot, sbom_path, Path(tool_snapshot["resolvedPaths"]["reakt"]))
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
        raise SystemExit("reakt: build input finalization failed") from None
