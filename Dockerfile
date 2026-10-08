# syntax=docker/dockerfile:1
#
# HexDB server image: the API server, the CLI and the admin UI.
#
#   docker build -t hexdb .
#   docker run -p 7700:7700 -v hexdb-data:/var/lib/hexdb \
#     -e HEXDB_STORAGE__ENCRYPTION_KEY="base64:$(openssl rand -base64 32)" hexdb
#
# Configuration: /etc/hexdb/hexdb.toml (docker/hexdb.toml), overridable with
# HEXDB_<SECTION>__<FIELD> variables. See docker-compose.yml for a lattice.

FROM node:22-bookworm-slim AS ui
WORKDIR /src/hexdb_admin
COPY hexdb_admin/package.json hexdb_admin/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY hexdb_admin/ ./
RUN npm run build

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml ./
COPY hexdb_core hexdb_core
COPY hexdb_api hexdb_api
COPY hexdb_cli hexdb_cli
COPY hexdb_query hexdb_query
COPY hexdb_tests hexdb_tests
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p hexdb_api -p hexdb_cli \
 && cp target/release/hexdb_api target/release/hexdb /usr/local/bin/

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl python3 nodejs \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /var/lib/hexdb --shell /usr/sbin/nologin hexdb \
 && mkdir -p /var/lib/hexdb /etc/hexdb \
 && chown hexdb:hexdb /var/lib/hexdb
COPY --from=build /usr/local/bin/hexdb_api /usr/local/bin/hexdb /usr/local/bin/
COPY --from=ui /src/hexdb_admin/dist /usr/share/hexdb/ui
COPY docker/hexdb.toml /etc/hexdb/hexdb.toml
COPY plugins /usr/share/hexdb/plugins
USER hexdb
WORKDIR /var/lib/hexdb
VOLUME /var/lib/hexdb
EXPOSE 7700 7702
ENV HEXDB_CONFIG=/etc/hexdb/hexdb.toml
HEALTHCHECK --interval=15s --timeout=3s --start-period=20s CMD curl -fsS http://127.0.0.1:7700/health || exit 1
ENTRYPOINT ["hexdb_api"]
