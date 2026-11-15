FROM rust:1.96.1-bookworm AS build
WORKDIR /src
RUN rustup target add wasm32-unknown-unknown \
    && cargo install --locked wasm-bindgen-cli --version 0.2.130
COPY . .
ARG STRYK_WEB_CLUSTER
ARG STRYK_WEB_MARKET
ARG STRYK_WEB_DOMAIN
ARG STRYK_WEB_QUOTE_MINT
RUN cargo build --locked --release -p stryk-service \
    && bash scripts/build-web.sh

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/stryk-service /usr/local/bin/stryk-service
COPY --from=build /src/target/web /srv/stryk
ENV STRYK_LISTEN=0.0.0.0:8080 STRYK_WEB_DIR=/srv/stryk
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/stryk-service"]
