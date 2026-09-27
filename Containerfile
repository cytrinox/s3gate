# The builder tag pins the toolchain; rust-toolchain.toml is deliberately not copied
# so rustup does not try to download a differently named 1.98 toolchain.
FROM docker.io/library/rust:1.98-slim-trixie AS builder
WORKDIR /src

# Build dependencies against a stub binary so they are cached across source changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release --locked

FROM gcr.io/distroless/cc-debian13:nonroot
LABEL org.opencontainers.image.licenses="AGPL-3.0-or-later"
COPY LICENSE /usr/share/doc/s3gate/LICENSE
COPY --from=builder /src/target/release/s3gate /usr/local/bin/s3gate
USER nonroot
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/s3gate"]
