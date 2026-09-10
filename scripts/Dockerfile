ARG BUILDER_IMAGE
ARG RUNTIME_IMAGE

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

FROM ${RUNTIME_IMAGE} AS runtime
ARG IR_DIGEST
LABEL org.opencontainers.image.title="Apsides generated controller"
LABEL dev.apsides.ir-digest=${IR_DIGEST}
LABEL dev.apsides.release-status="blocked"
COPY --from=build /build/target/release/controller /usr/local/bin/apsides-controller
USER 65532:65532
WORKDIR /
ENTRYPOINT ["/usr/local/bin/apsides-controller"]
