FROM rust:alpine AS server
RUN apk add --no-cache musl-dev build-base
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM alpine:3.20
WORKDIR /app
COPY --from=server /app/target/release/nostr-ssr /app/nostr-ssr
COPY configs /app/configs
ENV BIND=0.0.0.0:3000 DIST=/app/dist
EXPOSE 3000
CMD ["/app/nostr-ssr"]
