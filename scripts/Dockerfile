ARG BUILDER_IMAGE

FROM ${BUILDER_IMAGE} AS build
WORKDIR /build
ARG SOURCE_DATE_EPOCH
ENV SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH}
ENV CARGO_HOME=/opt/apsides-cargo CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/build/target
# crates/engine pins deno_core, which pulls in the `v8` crate (denoland/
# rusty_v8); its build.rs otherwise downloads a prebuilt static-lib archive
# from GitHub Releases, which the --network=none RUN below cannot reach.
# scripts/package_oci.py fetches that archive and verifies its SHA-256
# against scripts/rusty-v8/pins.json before staging this context, always at
# this fixed path; no RUSTY_V8_SRC_BINDING_PATH is set because the pinned
# v8 crate version ships a pre-generated binding for both supported targets
# (see scripts/rusty-v8/pins.json's bindingFile).
ENV RUSTY_V8_ARCHIVE=/build/rusty-v8/librusty_v8.a.gz
COPY .cargo .cargo
COPY vendor vendor
COPY workspace workspace
COPY controller controller
COPY rusty-v8 rusty-v8
RUN --network=none cargo build --frozen --release --manifest-path controller/Cargo.toml

# The local exporter hands over only the binary; scripts/package_oci.py puts it
# on the pinned runtime base.
FROM scratch AS controller
COPY --from=build /build/target/release/controller /apsides-controller
