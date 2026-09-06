ARG BUILDER_IMAGE

FROM ${BUILDER_IMAGE} AS build
WORKDIR /build
ARG SOURCE_DATE_EPOCH
ENV CARGO_HOME=/opt/reakt-cargo CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/build/target SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH}
COPY .cargo .cargo
COPY vendor vendor
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY select-target.py /usr/local/bin/select-target
RUN --network=none cargo test --frozen --offline --release -p inventory --test pvc_restart --no-run --message-format=json > /tmp/cargo.json \
    && executable="$(python3 /usr/local/bin/select-target /tmp/cargo.json)" \
    && install -D -m 0755 "$executable" /out/inventory-pvc-probe

FROM ${BUILDER_IMAGE}
LABEL dev.reakt.qualification="test-only"
COPY --from=build /out/inventory-pvc-probe /usr/local/bin/inventory-pvc-probe
COPY storage-crash /usr/local/bin/storage-crash
RUN chmod 0755 /usr/local/bin/storage-crash
USER 65532:65532
WORKDIR /
ENTRYPOINT ["/usr/local/bin/inventory-pvc-probe"]
