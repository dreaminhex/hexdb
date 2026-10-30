# HexDB

An encrypted, replicated database for JSON documents and binary files, in one Rust binary: REST, GraphQL, gRPC and read-only SQL; full-text and vector search; an admin UI; streams, functions, triggers and plugins; and an MCP server and A2A agent for AI tools. Data is encrypted at rest (AES-256-GCM) and replicated across a lattice of hexes with automatic failover.

- Website: https://hexdb.app
- Source and issues: https://github.com/dreaminhex/hexdb
- Manual: https://github.com/dreaminhex/hexdb/blob/main/MANUAL.md

## Run one hex

```bash
docker run -d --name hexdb -p 7700:7700 -v hexdb-data:/var/lib/hexdb \
  -e HEXDB_STORAGE__ENCRYPTION_KEY="base64:$(openssl rand -base64 32)" \
  dreaminhex/hexdb
docker logs hexdb | grep -A2 "first administrator"   # the generated admin password
```

Open http://localhost:7700/ui/ and sign in. Keep the encryption key: the data can't be read without it.

The server writes only its volume, so it also runs with a read-only root file system and no Linux capabilities:

```bash
docker run -d --read-only --tmpfs /tmp --cap-drop ALL --security-opt no-new-privileges \
  -p 7700:7700 -v hexdb-data:/var/lib/hexdb -e HEXDB_STORAGE__ENCRYPTION_KEY="base64:..." dreaminhex/hexdb
```

## Configuration

`/etc/hexdb/hexdb.toml` holds the defaults; any setting can be overridden with `HEXDB_<SECTION>__<FIELD>`:

| Variable | Meaning |
| --- | --- |
| `HEXDB_STORAGE__ENCRYPTION_KEY` | Required: `base64:<32 bytes>` (`docker run --rm --entrypoint hexdb dreaminhex/hexdb secret` prints one) |
| `HEXDB_NETWORK__LATTICE_NAME` | The same on every hex of a lattice |
| `HEXDB_NETWORK__LATTICE_SECRET` | Shared by every hex of a lattice |
| `HEXDB_NETWORK__PEERS` | Other hexes' discovery addresses (`host:7702`), comma-separated |
| `HEXDB_NETWORK__ADVERTISE_HOST` | How other hexes reach this one |
| `HEXDB_SECURITY__ADMIN_PASSWORD` | The first administrator's password (otherwise generated and printed once) |

Ports: 7700 (API and admin UI), 7702 (discovery between hexes). Data: `/var/lib/hexdb`.

## A lattice

The repository's [docker-compose.yml](https://github.com/dreaminhex/hexdb/blob/main/docker-compose.yml) runs three hexes with failover. On Kubernetes, use the Helm chart:

```bash
helm install hexdb oci://ghcr.io/dreaminhex/charts/hexdb --set replicas=3
```

## Tags

`latest`, the version (`2.2.0`) and the release tag (`v2.2.0`), for `linux/amd64` and `linux/arm64`. The same image is published as `ghcr.io/dreaminhex/hexdb`.

A forgotten admin password: `docker exec -u hexdb hexdb hexdb reset-password`.

Licensed under Apache 2.0.
