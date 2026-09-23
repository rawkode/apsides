ARG BUILDER_IMAGE

FROM ${BUILDER_IMAGE} AS build
WORKDIR /build
ARG SOURCE_DATE_EPOCH
ENV SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH}
ENV CARGO_HOME=/opt/apsides-cargo CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/build/target
COPY .cargo .cargo
COPY vendor vendor
COPY workspace workspace
COPY controller controller
RUN --network=none cargo build --frozen --release --manifest-path controller/Cargo.toml

# The local exporter hands over only the binary; scripts/package_oci.py puts it
# on the pinned runtime base.
FROM scratch AS controller
COPY --from=build /build/target/release/controller /apsides-controller
