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
#
# By default everything is built from source. A release builds the binaries
# natively for each architecture first and passes them in instead, so nothing
# compiles under emulation (see .github/workflows/release.yml):
#
#   prebuilt/amd64/{hexdb_api,hexdb,ffmpeg}  prebuilt/arm64/{hexdb_api,hexdb,ffmpeg}  prebuilt/ui/
#   docker buildx build --build-arg BINARIES=prebuilt --platform linux/amd64,linux/arm64 .

ARG BINARIES=source

FROM node:22-bookworm-slim AS ui
WORKDIR /src/hexdb_admin
COPY hexdb_admin/package.json hexdb_admin/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY hexdb_admin/ ./
RUN npm run build

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY hexdb_core hexdb_core
COPY hexdb_api hexdb_api
COPY hexdb_cli hexdb_cli
COPY hexdb_query hexdb_query
COPY hexdb_tests hexdb_tests
# Workspace members that are not built here but must be present for Cargo to
# load the workspace manifest.
COPY drivers/odbc drivers/odbc
COPY bench bench
# The amd64 and arm64 images build in parallel: they take turns with the
# shared download cache, and each keeps its own build directory.
ARG TARGETPLATFORM
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,id=hexdb-target-$TARGETPLATFORM \
    cargo build --locked --release -p hexdb_api -p hexdb_cli \
 && cp target/release/hexdb_api target/release/hexdb /usr/local/bin/

# A small ffmpeg for thumbnails HexDB can't make itself (HEIC, AVIF, HEVC, VP9, AV1): only the
# decoders it uses, from Debian trixie's own source (docker/build-ffmpeg.sh).
FROM debian:trixie AS ffmpeg
COPY docker/build-ffmpeg.sh /tmp/build-ffmpeg.sh
RUN bash /tmp/build-ffmpeg.sh /opt/ffmpeg

# The binaries and the admin UI, from one of these two (BINARIES): the other isn't built.
FROM scratch AS source-binaries
COPY --from=build /usr/local/bin/hexdb_api /usr/local/bin/hexdb /usr/local/bin/
COPY --from=ffmpeg /opt/ffmpeg/ffmpeg /usr/local/bin/
COPY --from=ffmpeg /opt/ffmpeg/COPYING.LGPLv2.1 /usr/share/doc/ffmpeg/
COPY --from=ui /src/hexdb_admin/dist /usr/share/hexdb/ui

FROM scratch AS prebuilt-binaries
ARG TARGETARCH
COPY --chmod=0755 prebuilt/${TARGETARCH}/hexdb_api prebuilt/${TARGETARCH}/hexdb prebuilt/${TARGETARCH}/ffmpeg /usr/local/bin/
COPY prebuilt/${TARGETARCH}/COPYING.LGPLv2.1 /usr/share/doc/ffmpeg/
COPY prebuilt/ui /usr/share/hexdb/ui

FROM ${BINARIES}-binaries AS binaries

# Trixie: Tesseract 5.5 with English for OCR, and dav1d for the small ffmpeg's AV1 decoding; HexDB
# finds both programs on its own (files.tesseract, files.ffmpeg). Binaries built on bookworm run here.
FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl python3 nodejs tesseract-ocr tesseract-ocr-eng libdav1d7 \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /var/lib/hexdb --shell /usr/sbin/nologin hexdb \
 && mkdir -p /var/lib/hexdb /etc/hexdb \
 && chown hexdb:hexdb /var/lib/hexdb
COPY --from=binaries /usr/local/bin/ /usr/local/bin/
COPY --from=binaries /usr/share/hexdb/ui /usr/share/hexdb/ui
COPY docker/hexdb.toml /etc/hexdb/hexdb.toml
COPY LICENSE THIRD-PARTY-NOTICES.md /usr/share/doc/hexdb/
COPY plugins /usr/share/hexdb/plugins
COPY --chmod=0755 docker/entrypoint.sh /usr/local/bin/hexdb-entrypoint
# Starts as root only to give the data volume to the hexdb user, then runs HexDB
# as hexdb (see docker/entrypoint.sh).
WORKDIR /var/lib/hexdb
VOLUME /var/lib/hexdb
EXPOSE 7700 7702
ENV HEXDB_CONFIG=/etc/hexdb/hexdb.toml
HEALTHCHECK --interval=15s --timeout=3s --start-period=20s CMD curl -fsS http://127.0.0.1:7700/health || exit 1
ENTRYPOINT ["hexdb-entrypoint"]
