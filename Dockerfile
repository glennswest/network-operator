# Static musl build on scratch — the operator only needs the CA bundle to reach
# the apiserver.
FROM rust:1.95-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
# Cargo.lock is committed, so --locked pins the exact dependency versions a
# release was built against and the image is reproducible from a tag.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin network-operator --target x86_64-unknown-linux-musl

FROM scratch
# Links the ghcr package back to this repo, so the package page carries
# provenance and can inherit the repository's access settings.
LABEL org.opencontainers.image.source="https://github.com/glennswest/network-operator"
LABEL org.opencontainers.image.description="Cluster Network Operator for the rustkube/stormcos stack — manages the Cilium CNI lifecycle from a Network CR"
LABEL org.opencontainers.image.licenses="Apache-2.0"
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/network-operator /network-operator
ENTRYPOINT ["/network-operator"]
CMD ["run"]
