# Runtime base pins

`reference` is the digest-pinned runtime base image that `aps build` puts the
controller on. `blobs/sha256/` holds that image's registry index and its
linux/amd64 and linux/arm64 manifests, byte for byte, named by their digests.

These are source pins, like a lockfile, not generated output. Neither Docker's
classic image store nor podman keeps these documents, and `aps build` never
pulls, so the build and `aps release` verify the index → manifest → config
chain offline from them.

To move to a new base, change `reference`, run `cuenv task runtimeBasePin`
(the only step that contacts the registry), pull the new image, and commit the
changed pins. Never edit a blob by hand: its name is its SHA-256.
