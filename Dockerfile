# OxideLoop runtime images are deliberately built in separate targets. The
# production target never compiles the test-failpoints feature; the e2e target
# is a debug-only image and is not a production deployment artifact.

FROM rust:1.98-bookworm AS build-production
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --locked --release -p controller-runtime --bin oxideloop-controller \
    && cargo build --locked --release -p renderer-worker --bin renderer-worker

FROM rust:1.98-bookworm AS build-e2e-failpoints
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
# This must stay a debug build: controller-runtime intentionally rejects a
# release build with test-failpoints at compile time.
RUN cargo build --locked -p controller-runtime --features test-failpoints --bin oxideloop-controller \
    && cargo build --locked -p controller-runtime --features test-failpoints --bin oxideloop-e2e-failpoint-control \
    && cargo build --locked -p renderer-worker --bin renderer-worker

# The e2e init container uses this statically linked BusyBox binary to create
# its private failpoint directory. It is the only utility copied into a
# runtime image; no package manager, compiler, source tree, or build cache is
# present.
FROM busybox:1.36.1-musl AS e2e-tools
ARG OXIDELOOP_BUNDLE_ARCHIVE
ARG OXIDELOOP_BUNDLE_DIGEST_FILE
COPY ${OXIDELOOP_BUNDLE_ARCHIVE} /embedded-input/bundle.archive
COPY ${OXIDELOOP_BUNDLE_DIGEST_FILE} /embedded-input/bundle.digest
RUN mkdir -p /embedded/usr/share/oxideloop \
    && cp /embedded-input/bundle.archive /embedded/usr/share/oxideloop/bundle.archive \
    && cp /embedded-input/bundle.digest /embedded/usr/share/oxideloop/bundle.digest \
    && chmod 0555 /embedded/usr/share/oxideloop \
    && chmod 0444 /embedded/usr/share/oxideloop/bundle.archive /embedded/usr/share/oxideloop/bundle.digest

FROM gcr.io/distroless/cc-debian12:nonroot AS e2e-failpoints
COPY --from=build-e2e-failpoints /src/target/debug/oxideloop-controller /usr/local/bin/oxideloop-controller
COPY --from=build-e2e-failpoints /src/target/debug/oxideloop-e2e-failpoint-control /usr/local/bin/oxideloop-e2e-failpoint-control
COPY --from=build-e2e-failpoints /src/target/debug/renderer-worker /usr/local/bin/renderer-worker
COPY --from=e2e-tools /bin/busybox /bin/busybox
COPY --from=e2e-tools --chown=65532:65532 /embedded/usr/share/oxideloop /usr/share/oxideloop
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/oxideloop-controller"]

# Keep production as the safe default target. The e2e image is intentionally
# separate and must be selected explicitly with --target e2e-failpoints.
FROM gcr.io/distroless/cc-debian12:nonroot AS production
COPY --from=build-production /src/target/release/oxideloop-controller /usr/local/bin/oxideloop-controller
COPY --from=build-production /src/target/release/renderer-worker /usr/local/bin/renderer-worker
COPY --from=e2e-tools --chown=65532:65532 /embedded/usr/share/oxideloop /usr/share/oxideloop
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/oxideloop-controller"]
