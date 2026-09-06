ARG BUILDER_IMAGE
ARG RUNTIME_IMAGE

FROM ${BUILDER_IMAGE} AS build
WORKDIR /build
ARG SOURCE_DATE_EPOCH
ENV SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH}
ENV CARGO_HOME=/opt/reakt-cargo CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/build/target
COPY .cargo .cargo
COPY .generated .generated
COPY vendor vendor
COPY workspace workspace
COPY controller controller
RUN --network=none cargo build --frozen --release --manifest-path controller/Cargo.toml

FROM ${RUNTIME_IMAGE} AS runtime
ARG IR_DIGEST
LABEL org.opencontainers.image.title="Reakt generated controller"
LABEL dev.reakt.ir-digest=${IR_DIGEST}
LABEL dev.reakt.release-status="blocked"
COPY --from=build /build/target/release/controller /usr/local/bin/reakt-controller
USER 65532:65532
WORKDIR /
ENTRYPOINT ["/usr/local/bin/reakt-controller"]
