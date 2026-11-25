FROM rust:1.96.0-bookworm AS build
WORKDIR /build
COPY Cargo.lock Cargo.toml LICENSE-* THIRD-PARTY.md ./
COPY tools/devices/licenses/ tools/devices/licenses/
COPY crates/ crates/
COPY src/ src/
RUN cargo build --locked --release --bins \
    && mkdir -p /notices/dependencies /notices/devices \
    && cp LICENSE-* THIRD-PARTY.md Cargo.lock /notices/ \
    && cp -L crates/zigbee-devices/profiles/LICENS* /notices/devices/ \
    && cp tools/devices/licenses/* /notices/devices/ \
    && cp "$CARGO_HOME"/registry/cache/*/*.crate /notices/dependencies/ \
    && chmod -R a+rX /notices

FROM debian:bookworm-slim
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /build/target/release/zigbee-bridge /build/target/release/zigbee-discovery /build/target/release/z2m_migrate /usr/local/bin/
COPY --from=build /notices/ /usr/share/doc/zigbee-bridge/
USER 65534:65534
ENTRYPOINT ["zigbee-bridge"]
