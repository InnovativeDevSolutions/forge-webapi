# forge-webapi

Standalone Web API for [Forge](https://github.com/InnovativeDevSolutions/forge), the Arma 3 persistent-server framework.

Forge's game-server persistence used to live in a Rust extension (`arma-rs`) that only runs while the Arma server is running — if the server crashed, unsaved session data could be lost. `forge-webapi` moves persistence out of the game process: a Rust HTTP API backed by SurrealDB, serving one or more Forge servers, so data survives a server crash and can be shared across servers.

## Architecture

One `docker compose` file per community:

```
Arma 3 server(s) ──webRequest──▶ forge-api (Rust, axum) ──▶ SurrealDB
                                    ▲ 8080 (public)
                                    │
                              SurrealDB 8000 (internal network only)
```

- **forge-api** — Rust API (`crates/forge-api`). Stateless; all durable state lives in SurrealDB.
- **SurrealDB** — the persistence store (namespace `forge`, database `main` by default). It is not exposed to the network — its port is bound to localhost only, for local debugging.
- **Web UI** — the existing Forge browser UIs (banking, garages, CAD, …) will talk to this API instead of the extension. Not yet wired up.

Each community runs its own stack — there is no shared hosted API.

## Quickstart

```sh
cp .env.example .env   # set a real SURREAL_PASSWORD
docker compose up --build
curl http://localhost:8080/health
# {"status":"ok","database":"connected"}
```

Or run the API directly (needs a SurrealDB on `SURREAL_ENDPOINT`):

```sh
export SURREAL_PASSWORD=...   # required, no default
cargo run -p forge-api
```

## Configuration

| Variable | Default | Description |
|---|---|---|
| `FORGE_API_BIND` | `0.0.0.0:8080` | Address the API listens on |
| `SURREAL_ENDPOINT` | `127.0.0.1:8000` | SurrealDB endpoint, `host:port` — no scheme (the HTTP engine adds it) |
| `SURREAL_NAMESPACE` | `forge` | SurrealDB namespace |
| `SURREAL_DATABASE` | `main` | SurrealDB database |
| `SURREAL_USER` | `root` | SurrealDB username |
| `SURREAL_PASSWORD` | *(required)* | SurrealDB password — no default |

**Security note:** the original Forge extension shipped `root`/`root` as SurrealDB defaults. This API deliberately has no password default — it refuses to start without `SURREAL_PASSWORD`. Set a long random password in `.env` (`.env` is gitignored; never commit it).

## Arma 3 / CORS

Arma 3's `webRequest` forces the `Origin` header to start with `arma://` (the `"origin"` key in the SQF request hashmap, e.g. `"forge"` → `Origin: arma://forge`) and sends a CORS preflight (`OPTIONS`) before every request. A wildcard `Access-Control-Allow-Origin: *` does not satisfy that, so the API echoes back the exact `arma://*` Origin it receives and answers preflights for `GET`/`POST`/`PUT`/`DELETE`/`OPTIONS`. See `crates/forge-api/src/cors.rs` and the example in `examples/sqf/webrequest_health.sqf`.

## Roadmap

- [x] Skeleton: health endpoint, SurrealDB connection, `arma://` CORS, Docker
- [ ] CAD / dispatch endpoints (port of `lib/services` dispatch)
- [ ] Task / contract endpoints (port of `lib/services` task)
- [ ] Player data: bank, garages, lockers, phones, orgs, stores
- [ ] Auth for game servers and web UI clients
- [ ] Thin the Arma extension down to a bridge (or remove it) once the API owns persistence

## License

TBD — expected to match the Forge project license; to be confirmed.
