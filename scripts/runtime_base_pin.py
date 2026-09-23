#!/usr/bin/env python3
"""Refresh the committed registry metadata of the pinned runtime base image.

`aps build` never pulls, and Docker's classic image store keeps neither the
registry index nor the platform manifests of a pulled image. This is the one
step that contacts the registry: it fetches the index named by the digest in
scripts/runtime-base/reference and its linux/amd64 and linux/arm64 manifests,
checks each against its digest, and stores their exact bytes content-addressed
under scripts/runtime-base/blobs/sha256. The build and `aps release` verify the
image chain offline from those pins.
"""
import hashlib
import json
from pathlib import Path
import re
import shutil
import tempfile
import urllib.error
import urllib.parse
import urllib.request

PINS = Path(__file__).resolve().parent / "runtime-base"
PLATFORMS = [("linux", "amd64"), ("linux", "arm64")]
INDEX_TYPES = ["application/vnd.oci.image.index.v1+json", "application/vnd.docker.distribution.manifest.list.v2+json"]
MANIFEST_TYPES = ["application/vnd.oci.image.manifest.v1+json", "application/vnd.docker.distribution.manifest.v2+json"]
MAX_DOCUMENT = 1024 * 1024


def parse(reference):
    match = re.fullmatch(r"([a-z0-9][a-z0-9./:_-]*)@(sha256:[0-9a-f]{64})", reference)
    if match is None:
        raise ValueError("the pinned runtime base must be repository@sha256:<digest>")
    repository, digest = match.groups()
    first, _, rest = repository.partition("/")
    if rest and ("." in first or ":" in first or first == "localhost"):
        host, name = first, rest
    else:
        host, name = "registry-1.docker.io", repository if rest else "library/" + repository
    if host == "docker.io":
        host = "registry-1.docker.io"
    return host, name, digest


def token(host, name):
    """Anonymous pull token from the registry's advertised bearer realm, if any."""
    try:
        urllib.request.urlopen(f"https://{host}/v2/", timeout=30)
        return None
    except urllib.error.HTTPError as error:
        challenge = error.headers.get("WWW-Authenticate", "")
        if error.code != 401 or not challenge.lower().startswith("bearer "):
            raise
    fields = dict(re.findall(r'(\w+)="([^"]*)"', challenge))
    query = urllib.parse.urlencode({"service": fields.get("service", ""), "scope": f"repository:{name}:pull"})
    with urllib.request.urlopen(f"{fields['realm']}?{query}", timeout=30) as response:
        body = json.loads(response.read())
    return body.get("token") or body.get("access_token")


def fetch(host, name, bearer, digest, accept):
    headers = {"Accept": ", ".join(accept)}
    if bearer:
        headers["Authorization"] = f"Bearer {bearer}"
    request = urllib.request.Request(f"https://{host}/v2/{name}/manifests/{digest}", headers=headers)
    with urllib.request.urlopen(request, timeout=60) as response:
        data = response.read(MAX_DOCUMENT + 1)
    if len(data) > MAX_DOCUMENT or "sha256:" + hashlib.sha256(data).hexdigest() != digest:
        raise ValueError(f"registry bytes differ from {digest}")
    return data


def main():
    reference = (PINS / "reference").read_text().strip()
    host, name, digest = parse(reference)
    bearer = token(host, name)
    index = fetch(host, name, bearer, digest, INDEX_TYPES)
    if json.loads(index).get("mediaType") not in INDEX_TYPES:
        raise ValueError("the pinned runtime base digest must name an image index")
    blobs = {digest: index}
    for os_name, architecture in PLATFORMS:
        matches = [item for item in json.loads(index)["manifests"]
                   if item.get("platform", {}).get("os") == os_name and item.get("platform", {}).get("architecture") == architecture]
        if len(matches) != 1:
            raise ValueError(f"the pinned index must list exactly one {os_name}/{architecture} manifest")
        blobs[matches[0]["digest"]] = fetch(host, name, bearer, matches[0]["digest"], MANIFEST_TYPES)
    staging = Path(tempfile.mkdtemp(prefix=".blobs-", dir=PINS))
    try:
        (staging / "sha256").mkdir()
        for blob, data in blobs.items():
            (staging / "sha256" / blob.removeprefix("sha256:")).write_bytes(data)
        if (PINS / "blobs").exists():
            shutil.rmtree(PINS / "blobs")
        staging.rename(PINS / "blobs")
    except BaseException:
        shutil.rmtree(staging, ignore_errors=True)
        raise
    print(json.dumps({"reference": reference, "blobs": sorted(blobs)}, indent=2))


if __name__ == "__main__":
    main()
