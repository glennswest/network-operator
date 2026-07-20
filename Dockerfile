# Static musl build on scratch — the operator only needs the CA bundle to reach
# the apiserver.
FROM rust:1.95-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
# Cargo.lock is gitignored here, so it may not be in the build context. Commit
# it (and add it to this COPY) if you want byte-reproducible images.
COPY Cargo.toml ./
COPY src ./src
RUN cargo build --release --bin network-operator --target x86_64-unknown-linux-musl

FROM scratch
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/network-operator /network-operator
ENTRYPOINT ["/network-operator"]
CMD ["run"]
