FROM rust:alpine AS server
RUN apk add --no-cache musl-dev build-base
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM alpine:3.20
WORKDIR /app
COPY --from=server /app/target/release/nostr-ssr /app/nostr-ssr
COPY configs /app/configs
ENV BIND=0.0.0.0:3000 DIST=/app/dist
# The server only needs to read its config and write its dist/cache paths;
# run it as an unprivileged user so a parser bug in rendered content is not
# root in the container.
RUN adduser -D -u 1000 ssr && mkdir -p /app/dist && chown -R ssr:ssr /app/dist
USER ssr
EXPOSE 3000
CMD ["/app/nostr-ssr"]
