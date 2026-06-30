# Atrium — unified activity inbox

Atrium is the HOLDFAST estate's **"what's new for me"** command center: one pane that
aggregates **unread chat** (Murmur), **unread notifications** (Klaxon), and **fresh feed items**
(Current) for the signed-in user, with deep links out to each service plus quick-links to
mail / forum / wiki / home.

It is a **pure read aggregator** — it owns NO database of its own. Each column is a **read-only**
federation of a sibling service's database (mirroring how Cortex federates the content services),
scoped to the gateway-injected viewer. Atrium never writes anywhere.

## Identity

Internal-only, behind the Sluice gateway on a `auth=sso` route at `inbox.w33d.xyz`. The gateway
runs the OIDC login, strips any inbound `X-Auth-*`, and injects the verified
`X-Auth-Subject` / `X-Auth-Email` / `X-Auth-Scope`. Atrium trusts those headers: the subject is the
ownership key every federated query is scoped by. There is no own login UI and no state-changing
POST (so no CSRF surface).

## Endpoints

| Method | Path        | Auth | Description |
|--------|-------------|------|-------------|
| GET    | `/`         | sso  | The unified dashboard: summary bar + three columns (Chat / Notifications / Feed river), concurrently fetched, ~10 s cached, resilient. |
| GET    | `/healthz`  | none | Liveness probe (`200 ok`), used by the container HEALTHCHECK. |

## Resilience

Every column is best-effort (Portal's concurrent-fetch contract): the three sources are queried
**concurrently** and the result is cached per viewer for ~10 s. A source whose DSN is **unset**
renders an empty "all clear" column; an **unreachable** source renders an "unavailable"
placeholder. The page NEVER errors or hangs because a backend is down.

## Configuration (env)

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `BIND_ADDR` | no | `0.0.0.0:9070` | Listen address. |
| `MURMUR_DATABASE_URL` | no | — | Read-only DSN for the Murmur (chat) DB. Unset → empty Chat column. |
| `KLAXON_DATABASE_URL` | no | — | Read-only DSN for the Klaxon (notifications) DB. Unset → empty Notifications column. |
| `CURRENT_DATABASE_URL` | no | — | Read-only DSN for the Current (RSS) DB. Unset → empty Feed column. |
| `AUDIT_ENABLED` | no | `false` | Enable the non-blocking Watchtower audit emitter. |
| `WATCHTOWER_URL` | no | `http://watchtower:8500` | Watchtower base URL (audit ingest). |
| `AUDIT_INGEST_TOKEN` | no | — | Bearer token for Watchtower ingest. |

With zero config it boots and serves an empty inbox (no database, no network).

## Audit

When enabled, Atrium emits to Watchtower via a non-blocking, bounded-queue emitter (a down
Watchtower never blocks a request): an `info` `inbox.view` per real (uncached) inbox load, and a
`warning` `inbox.source_unavailable` per unreachable federated source. Private chat / notification
/ feed contents NEVER ride an event.

## Build

```sh
CARGO_BUILD_JOBS=2 cargo check --all-targets
cargo test
docker build -t atrium .
```
