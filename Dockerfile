# OxideLoop runtime images are deliberately built in separate targets. The
# production target never compiles the test-failpoints feature; the e2e target
# is a debug-only image and is not a production deployment artifact.

FROM rust:1.98-bookworm AS build-production
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN cargo build --locked --release -p oxideloop-controller-runtime --bin oxideloop-controller \
    && cargo build --locked --release -p oxideloop-renderer-worker --bin renderer-worker

FROM rust:1.98-bookworm AS build-e2e-failpoints
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
# This must stay a debug build: controller-runtime intentionally rejects a
# release build with test-failpoints at compile time.
RUN cargo build --locked -p oxideloop-controller-runtime --features test-failpoints --bin oxideloop-controller \
    && cargo build --locked -p oxideloop-renderer-worker --bin renderer-worker

# The e2e init container uses this statically linked BusyBox binary to create
# its private failpoint directory. It is the only utility copied into a
# runtime image; no package manager, compiler, source tree, or build cache is
# present.
FROM busybox:1.36.1-musl AS e2e-tools

FROM gcr.io/distroless/cc-debian12:nonroot AS e2e-failpoints
COPY --from=build-e2e-failpoints /src/target/debug/oxideloop-controller /usr/local/bin/oxideloop-controller
COPY --from=build-e2e-failpoints /src/target/debug/renderer-worker /usr/local/bin/renderer-worker
COPY --from=e2e-tools /bin/busybox /bin/busybox
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/oxideloop-controller"]

# Keep production as the safe default target. The e2e image is intentionally
# separate and must be selected explicitly with --target e2e-failpoints.
FROM gcr.io/distroless/cc-debian12:nonroot AS production
COPY --from=build-production /src/target/release/oxideloop-controller /usr/local/bin/oxideloop-controller
COPY --from=build-production /src/target/release/renderer-worker /usr/local/bin/renderer-worker
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/oxideloop-controller"]
